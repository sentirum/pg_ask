//! Streaming agent entry point.
//!
//! `ask.ask_stream(question) RETURNS SETOF text` runs the same agent
//! loop as `ask()` but yields every assistant turn and tool result as a
//! separate row. The caller fetches rows one at a time (e.g. `FETCH 1`)
//! so latency is chunked rather than monolithic.
//!
//! Row prefixes:
//!   `[answer] `   — final text (last row unless the loop errors out).
//!   `[thinking] ` — reasoning the model emitted alongside tool calls.
//!   `[tool] `     — tool name + truncated output.
//!   `[error] `    — harness-level failure (provider HTTP error, etc).

use super::{dispatch, prompt, AgentMode};
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::Result;
use crate::infra::http::HttpClient;
use crate::providers::{self, Message, MessageContent, ProviderResponse, Role};
use crate::schema::{self, SchemaMode};
use crate::tools::{self, Tool};
use pgrx::prelude::*;

/// Run the agent loop and collect every observable event into a flat
/// list of strings. The API layer feeds this into `SetOfIterator`.
///
/// Loads the runtime config itself — callers that already have a
/// snapshot (the `api::*` entry points all do via
/// [`crate::api::trace::with_trace`]) should use [`run_stream_with_cfg`]
/// instead. See P1 in the v0.5.2 review. Kept as a convenience for
/// background workers / tests that don't go through `with_trace`.
#[allow(dead_code)]
pub fn run_stream(question: &str, mode: AgentMode) -> Result<Vec<String>> {
    let cfg = RuntimeConfig::load()?;
    run_stream_with_cfg(&cfg, question, mode)
}

/// Variant that uses a pre-loaded snapshot. P1 (v0.5.2 review):
/// `with_trace` already loads the config; threading it through avoids a
/// second GUC scan plus a second _config table fallback round-trip.
pub fn run_stream_with_cfg(
    cfg: &RuntimeConfig,
    question: &str,
    mode: AgentMode,
) -> Result<Vec<String>> {
    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = providers::build(cfg, http.clone())?;

    let schema_summary = schema::summarize_within(cfg.schema_char_budget)?;
    let system_prompt = prompt::build(&schema_summary.text, mode, cfg.readonly);
    let need_describe = matches!(schema_summary.mode, SchemaMode::Compact);
    let memory_ready = cfg.memory_enabled
        && cfg.embedding_provider.is_some()
        && cfg.embedding_api_key.is_some()
        && crate::memory::store::pgvector_installed().unwrap_or(false);

    let tools_vec: Vec<Box<dyn Tool>> = match mode {
        AgentMode::Execute => {
            tools::default_toolset(cfg, need_describe, memory_ready, http.clone())
        }
        AgentMode::GenerateOnly => Vec::new(),
    };
    let specs = tools_vec.iter().map(|t| t.spec()).collect::<Vec<_>>();

    let mut history: Vec<Message> = Vec::new();
    history.push(Message {
        role: Role::User,
        content: MessageContent::Text(question.to_string()),
    });

    let mut out: Vec<String> = Vec::new();

    for _iteration in 0..cfg.max_iterations {
        check_for_interrupts!();

        let resp = provider.complete(&system_prompt, &history, &specs)?;
        match resp {
            ProviderResponse::Final { text } => {
                history.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Text(text.clone()),
                });
                out.push(format!("[answer] {text}"));
                break;
            }

            ProviderResponse::ToolCalls { text, calls } => {
                if let Some(ref t) = text {
                    if !t.is_empty() {
                        out.push(format!("[thinking] {t}"));
                    }
                }
                history.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::AssistantWithTools {
                        text: text.clone(),
                        tool_calls: calls.clone(),
                    },
                });

                for call in calls {
                    let output = dispatch::dispatch(&tools_vec, &call.name, &call.arguments);
                    // v0.5.2 review #11: cap the streamed line. A
                    // 500-row sql_query result can be hundreds of KB,
                    // and the previous `[tool] {} → {}` line
                    // dumped the entire thing into ONE element of
                    // the SetOfIterator, which (a) blows past the
                    // libpq reply buffer for `ask.ask_stream`
                    // consumers and (b) is unreadable anyway.
                    // Truncated copy goes to the streaming surface;
                    // the model still sees the full text via
                    // `history` so its next reasoning step has
                    // complete information.
                    out.push(format!(
                        "[tool] {} → {}",
                        call.name,
                        crate::telemetry::truncate_tool_output(&output.text)
                    ));
                    history.push(Message {
                        role: Role::Tool,
                        content: MessageContent::ToolResult {
                            tool_call_id: call.id.clone(),
                            output: output.text,
                            is_error: output.is_error,
                        },
                    });
                }
            }
        }
    }

    if out.is_empty() {
        out.push("[error] no response from model".to_string());
    }

    Ok(out)
}
