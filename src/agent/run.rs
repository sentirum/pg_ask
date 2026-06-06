//! Agent loop.
//!
//! Build a runtime snapshot → introspect schema → build system prompt →
//! seed history → call provider → if it asks for tools, invoke them and
//! feed results back → repeat until a final text answer or `max_iterations`.

use super::{dispatch, prompt};
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use crate::providers::{self, Message, MessageContent, ProviderResponse, Role, ToolSpec};
use crate::schema::{self, SchemaMode};
use crate::telemetry::ToolCallTrace;
use crate::tools::{self, Tool};
use pgrx::prelude::*;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMode {
    /// Full loop with tools — used by `ask.ask`.
    Execute,
    /// No tools; ask the model for a single SQL statement — used by `ask.sql`.
    GenerateOnly,
}

/// What the loop produced. The text is what the SQL caller sees; the
/// `iterations` and `tool_calls` flow into [`crate::telemetry`] so
/// `ask._traces` reflects what really happened. `new_turns` is the
/// suffix of history added during this call (initial user message +
/// assistant turns + tool results) so the session layer can persist
/// exactly that slice without recomputing.
#[derive(Debug)]
pub struct AgentOutcome {
    pub text: String,
    pub iterations: u32,
    pub tool_calls: Vec<ToolCallTrace>,
    pub new_turns: Vec<crate::providers::Message>,
    /// P4 fix: accumulated token usage across all provider calls.
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

/// Single-shot entry: no prior conversation, no persistence.
///
/// Loads the runtime config itself — use [`run_with_cfg`] when the
/// caller already has a snapshot (the `api::*` entry points all do via
/// [`crate::api::trace::with_trace`]). Kept as a convenience for
/// background workers / tests that don't go through `with_trace`.
#[allow(dead_code)]
pub fn run(question: &str, mode: AgentMode) -> Result<AgentOutcome> {
    let cfg = RuntimeConfig::load()?;
    run_with_cfg(&cfg, question, Vec::new(), mode)
}

/// Resume a conversation: `history` is the persisted turn list, `question`
/// is the new user message appended to it. The caller is responsible for
/// persisting the resulting turns (api::chat does this). The public
/// `ask.chat` SQL surface goes through `run_with_cfg` directly; this
/// remains as the same-signature convenience for non-API callers.
#[allow(dead_code)]
pub fn run_with_history(
    question: &str,
    prior_history: Vec<Message>,
    mode: AgentMode,
) -> Result<AgentOutcome> {
    let cfg = RuntimeConfig::load()?;
    run_with_cfg(&cfg, question, prior_history, mode)
}

/// Variant that uses a pre-loaded snapshot. P1 (v0.5.2 review): every
/// `ask()` invocation used to call `RuntimeConfig::load` 2–3 times
/// (`with_trace` + `agent::run` + the first memory tool); the API
/// layer now loads once via `with_trace` and threads the snapshot
/// through here.
pub fn run_with_cfg(
    cfg: &RuntimeConfig,
    question: &str,
    prior_history: Vec<Message>,
    mode: AgentMode,
) -> Result<AgentOutcome> {
    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = providers::build(cfg, http.clone())?;

    let schema_summary = schema::summarize_within(cfg.schema_char_budget)?;
    let system_prompt = prompt::build(&schema_summary.text, mode, cfg.readonly);

    // When the schema render went compact we expose describe_table so the
    // model can pull column detail on demand. In Full mode the menu stays
    // minimal to keep tool-routing cheap.
    let need_describe = matches!(schema_summary.mode, SchemaMode::Compact);

    // Memory layer is opt-in (master GUC) AND requires pgvector + embedding
    // config. We detect it once here so the model isn't shown a `recall`
    // tool it cannot actually use. pgvector check is a single cheap SPI
    // call; cached for the lifetime of the agent loop via this snapshot.
    let memory_ready = cfg.memory_enabled
        && cfg.embedding_provider.is_some()
        && cfg.embedding_api_key.is_some()
        && crate::memory::store::pgvector_installed().unwrap_or(false);

    // Pin search_path to the introspected schemas so the model's queries
    // resolve even if it forgets to qualify or assumes `public` — this is
    // the single biggest source of wasted iterations on multi-schema DBs.
    let search_path = schema::search_path_clause(&schema_summary.text);
    let tools_vec: Vec<Box<dyn Tool>> = match mode {
        AgentMode::Execute => {
            tools::default_toolset(cfg, need_describe, memory_ready, http.clone(), &search_path)
        }
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
    let mut total_prompt_tokens: i64 = 0;
    let mut total_completion_tokens: i64 = 0;

    for iteration in 0..cfg.max_iterations {
        // Cooperative cancellation — lets `pg_cancel_backend` interrupt long loops.
        check_for_interrupts!();

        let resp = provider.complete(&system_prompt, &history, &specs)?;
        match resp {
            ProviderResponse::Final { text, usage } => {
                // P4: accumulate token usage from this final response.
                if let Some(u) = usage {
                    total_prompt_tokens += u.prompt_tokens;
                    total_completion_tokens += u.completion_tokens;
                }

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
                    prompt_tokens: total_prompt_tokens,
                    completion_tokens: total_completion_tokens,
                });
            }

            ProviderResponse::ToolCalls {
                text: tool_text,
                calls,
                usage,
            } => {
                // P4: accumulate token usage from this tool-call response.
                if let Some(u) = usage {
                    total_prompt_tokens += u.prompt_tokens;
                    total_completion_tokens += u.completion_tokens;
                }
                history.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::AssistantWithTools {
                        text: tool_text.clone(),
                        tool_calls: calls.clone(),
                    },
                });

                if calls.is_empty() {
                    // D6 fix: instead of a hard error, give the model a
                    // chance to self-correct. Return the final text if
                    // the model produced any; otherwise add a nudge to
                    // history so the next iteration tries harder.
                    if let Some(ref t) = tool_text {
                        if !t.is_empty() {
                            history.push(Message {
                                role: Role::Assistant,
                                content: MessageContent::Text(t.clone()),
                            });
                            let new_turns = history.split_off(prior_len);
                            return Ok(AgentOutcome {
                                text: t.clone(),
                                iterations: iteration + 1,
                                tool_calls: tool_trace,
                                new_turns,
                                prompt_tokens: total_prompt_tokens,
                                completion_tokens: total_completion_tokens,
                            });
                        }
                    }
                    // No text and no tool calls — nudge the model.
                    history.push(Message {
                        role: Role::User,
                        content: MessageContent::Text(
                            "You returned no tool calls and no answer. \
                             Please respond with your final answer now."
                                .into(),
                        ),
                    });
                    continue;
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

    // Iteration budget exhausted. Before failing, give the model ONE final
    // turn with NO tools and an explicit instruction to answer using what it
    // has already gathered. Complex questions sometimes spend the whole
    // budget on tool calls and would otherwise error out even though the
    // data needed for an answer is already in `history`. This converts many
    // "max iterations" failures into a useful (if caveated) answer.
    history.push(Message {
        role: Role::User,
        content: MessageContent::Text(
            "You have reached the step limit. Do not call any more tools. \
             Give your best final answer now using the information you have \
             already gathered above. If it is incomplete, say so briefly and \
             answer with what you know."
                .into(),
        ),
    });
    // Empty tool-spec slice => the model cannot request tools on this turn.
    if let Ok(ProviderResponse::Final { text, usage })
    | Ok(ProviderResponse::ToolCalls {
        text: Some(text),
        usage,
        ..
    }) = provider.complete(&system_prompt, &history, &[])
    {
        if !text.is_empty() {
            if let Some(u) = usage {
                total_prompt_tokens += u.prompt_tokens;
                total_completion_tokens += u.completion_tokens;
            }
            history.push(Message {
                role: Role::Assistant,
                content: MessageContent::Text(text.clone()),
            });
            let new_turns = history.split_off(prior_len);
            return Ok(AgentOutcome {
                text,
                iterations: cfg.max_iterations,
                tool_calls: tool_trace,
                new_turns,
                prompt_tokens: total_prompt_tokens,
                completion_tokens: total_completion_tokens,
            });
        }
    }

    Err(AskError::MaxIterations {
        max: cfg.max_iterations,
    })
}
