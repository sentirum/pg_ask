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
use crate::schema;
use crate::tools::{self, Tool};
use pgrx::prelude::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Full loop with tools — used by `pg_ask.ask`.
    Execute,
    /// No tools; ask the model for a single SQL statement — used by `pg_ask.sql`.
    GenerateOnly,
}

/// What the loop produced. Today only `text`; v0.2 adds tool-call trace,
/// token counts, and timings consumed by [`crate::telemetry`].
#[derive(Debug)]
pub struct AgentOutcome {
    pub text: String,
    /// Number of provider round-trips it took to converge. Surfaced via
    /// telemetry (and SQL once `pg_ask._traces` lands in v0.2).
    #[allow(dead_code)]
    pub iterations: u32,
}

pub fn run(question: &str, mode: AgentMode) -> Result<AgentOutcome> {
    let cfg = RuntimeConfig::load()?;
    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = providers::build(&cfg, http)?;

    let schema_summary = schema::summarize()?;
    let system_prompt = prompt::build(&schema_summary.text, mode, cfg.readonly);

    let tools_vec: Vec<Box<dyn Tool>> = match mode {
        AgentMode::Execute => tools::default_toolset(&cfg),
        AgentMode::GenerateOnly => Vec::new(),
    };
    let specs: Vec<ToolSpec> = tools_vec.iter().map(|t| t.spec()).collect();

    let mut history: Vec<Message> = vec![Message {
        role: Role::User,
        content: MessageContent::Text(question.to_string()),
    }];

    for iteration in 0..cfg.max_iterations {
        // Cooperative cancellation — lets `pg_cancel_backend` interrupt long loops.
        check_for_interrupts!();

        let resp = provider.complete(&system_prompt, &history, &specs)?;
        match resp {
            ProviderResponse::Final { text } => {
                return Ok(AgentOutcome {
                    text,
                    iterations: iteration + 1,
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
                    let output = dispatch::dispatch(&tools_vec, &call.name, &call.arguments);
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
