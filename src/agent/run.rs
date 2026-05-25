//! Agent loop.
//!
//! Build a runtime snapshot → introspect schema → build system prompt →
//! seed history → call provider → if it asks for tools, invoke them and
//! feed results back → repeat until a final text answer or `max_iterations`.

use super::{dispatch, prompt};
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use crate::providers::{
    self, Message, MessageContent, ProviderResponse, Role, ToolSpec,
};
use crate::schema::{self, SchemaMode};
use crate::telemetry::ToolCallTrace;
use crate::tools::{self, Tool};
use pgrx::prelude::*;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Full loop with tools — used by `pg_ask.ask`.
    Execute,
    /// No tools; ask the model for a single SQL statement — used by `pg_ask.sql`.
    GenerateOnly,
}

/// What the loop produced. The text is what the SQL caller sees; the
/// `iterations` and `tool_calls` flow into [`crate::telemetry`] so
/// `pg_ask._traces` reflects what really happened. `new_turns` is the
/// suffix of history added during this call (initial user message +
/// assistant turns + tool results) so the session layer can persist
/// exactly that slice without recomputing.
#[derive(Debug)]
pub struct AgentOutcome {
    pub text: String,
    pub iterations: u32,
    pub tool_calls: Vec<ToolCallTrace>,
    pub new_turns: Vec<crate::providers::Message>,
}

/// Single-shot entry: no prior conversation, no persistence.
pub fn run(question: &str, mode: AgentMode) -> Result<AgentOutcome> {
    run_with_history(question, Vec::new(), mode)
}

/// Resume a conversation: `history` is the persisted turn list, `question`
/// is the new user message appended to it. The caller is responsible for
/// persisting the resulting turns (api::chat does this).
pub fn run_with_history(
    question: &str,
    prior_history: Vec<Message>,
    mode: AgentMode,
) -> Result<AgentOutcome> {
    let cfg = RuntimeConfig::load()?;
    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = providers::build(&cfg, http)?;

    let schema_summary = schema::summarize_within(cfg.schema_char_budget)?;
    let system_prompt = prompt::build(&schema_summary.text, mode, cfg.readonly);

    // When the schema render went compact we expose describe_table so the
    // model can pull column detail on demand. In Full mode the menu stays
    // minimal (just sql_query) to keep tool-routing cheap.
    let need_describe = matches!(schema_summary.mode, SchemaMode::Compact);
    let tools_vec: Vec<Box<dyn Tool>> = match mode {
        AgentMode::Execute => tools::default_toolset(&cfg, need_describe),
        AgentMode::GenerateOnly => Vec::new(),
    };
    let specs: Vec<ToolSpec> = tools_vec.iter().map(|t| t.spec()).collect();

    let prior_len = prior_history.len();
    let mut history: Vec<Message> = prior_history;
    history.push(Message {
        role: Role::User,
        content: MessageContent::Text(question.to_string()),
    });
    let mut tool_trace: Vec<ToolCallTrace> = Vec::new();

    for iteration in 0..cfg.max_iterations {
        // Cooperative cancellation — lets `pg_cancel_backend` interrupt long loops.
        check_for_interrupts!();

        let resp = provider.complete(&system_prompt, &history, &specs)?;
        match resp {
            ProviderResponse::Final { text } => {
                // Append the final assistant message so it gets persisted
                // along with the rest of this turn's history slice.
                history.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Text(text.clone()),
                });
                let new_turns = history.split_off(prior_len);
                return Ok(AgentOutcome {
                    text,
                    iterations: iteration + 1,
                    tool_calls: tool_trace,
                    new_turns,
                });
            }

            ProviderResponse::ToolCalls { text, calls } => {
                history.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::AssistantWithTools {
                        text,
                        tool_calls: calls.clone(),
                    },
                });

                if calls.is_empty() {
                    return Err(AskError::EmptyResponse);
                }

                for call in calls {
                    let started = Instant::now();
                    let output = dispatch::dispatch(&tools_vec, &call.name, &call.arguments);
                    let elapsed_ms = started.elapsed().as_millis() as u64;

                    tool_trace.push(ToolCallTrace::from_call(
                        &call,
                        &output.text,
                        output.is_error,
                        elapsed_ms,
                    ));

                    history.push(Message {
                        role: Role::Tool,
                        content: MessageContent::ToolResult {
                            tool_call_id: call.id,
                            output: output.text,
                            is_error: output.is_error,
                        },
                    });
                }
            }
        }
    }

    Err(AskError::MaxIterations {
        max: cfg.max_iterations,
    })
}
