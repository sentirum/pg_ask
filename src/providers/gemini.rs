//! Google Gemini provider (`generateContent` REST API, v1beta).
//!
//! Spec: <https://ai.google.dev/api/generate-content>
//!
//! Wire-format differences from Anthropic + OpenAI that this module hides:
//!
//! * URL embeds the model name: `POST /v1beta/models/{model}:generateContent`.
//! * Auth via `x-goog-api-key` header (not Bearer, not x-api-key).
//! * Roles are `user` / `model` (no "assistant").
//! * Content is `parts[]` — each part is `{text}`, `{functionCall:{name,args}}`,
//!   or `{functionResponse:{name,response}}`. No string fast-path; even a
//!   plain reply is `parts: [{text: "..."}]`.
//! * System prompt sits at `systemInstruction.parts[].text`, not in `contents`.
//! * Tool calls have no id — Gemini matches the response back to the request
//!   purely by `name`. Our canonical `ToolCall.id` is kept on the Rust side
//!   for history bookkeeping; on the wire we round-trip `name`.
//! * Tool result is `role: "user"` + `functionResponse` part. The response
//!   payload must be a JSON object; we wrap the textual output as `{output, is_error}`.
//! * `finishReason: "STOP" | "MAX_TOKENS" | ...` — tool calls don't get a
//!   distinct reason; presence of `functionCall` parts is the trigger.

use super::wire::{Message, MessageContent, ProviderResponse, Role, ToolCall, ToolSpec};
use super::Provider;
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_MODEL: &str = "gemini-2.5-flash";

pub struct GeminiProvider {
    http: HttpClient,
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: u32,
}

impl GeminiProvider {
    pub fn new(cfg: &RuntimeConfig, http: HttpClient) -> Self {
        Self {
            http,
            api_key: cfg.api_key.clone(),
            model: cfg
                .model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            base_url: cfg
                .base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_tokens: cfg.max_tokens,
        }
    }
}

impl Provider for GeminiProvider {
    fn complete(
        &self,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ProviderResponse> {
        let body = build_request(self.max_tokens, system, history, tools);
        let url = format!(
            "{}/v1beta/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            self.model
        );
        let headers: [(&str, &str); 1] = [("x-goog-api-key", self.api_key.as_str())];

        let parsed: GenerateResponse = self.http.post_json(&url, &headers, &body)?;
        parse_response(parsed)
    }
}

// ---------- Request construction ----------

fn build_request(max_tokens: u32, system: &str, history: &[Message], tools: &[ToolSpec]) -> Value {
    let contents: Vec<Value> = history.iter().filter_map(message_to_wire).collect();

    let mut req = json!({
        "contents":          contents,
        "systemInstruction": { "parts": [{ "text": system }] },
        "generationConfig":  { "maxOutputTokens": max_tokens },
    });

    if !tools.is_empty() {
        let function_decls: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name":        t.name,
                    "description": t.description,
                    "parameters":  t.input_schema,
                })
            })
            .collect();
        req["tools"] = json!([{ "functionDeclarations": function_decls }]);
    }

    req
}

/// Returns None when the message should be dropped on the wire (e.g. a
/// system message that doesn't belong in `contents`).
fn message_to_wire(msg: &Message) -> Option<Value> {
    match (&msg.role, &msg.content) {
        (Role::User, MessageContent::Text(t)) => Some(json!({
            "role":  "user",
            "parts": [{ "text": t }],
        })),

        (Role::Assistant, MessageContent::Text(t)) => Some(json!({
            "role":  "model",
            "parts": [{ "text": t }],
        })),

        (Role::Assistant, MessageContent::AssistantWithTools { text, tool_calls }) => {
            let mut parts: Vec<Value> = Vec::with_capacity(tool_calls.len() + 1);
            if let Some(t) = text.as_ref().filter(|s| !s.is_empty()) {
                parts.push(json!({ "text": t }));
            }
            for call in tool_calls {
                parts.push(json!({
                    "functionCall": {
                        "name": call.name,
                        "args": call.arguments,
                    }
                }));
            }
            Some(json!({ "role": "model", "parts": parts }))
        }

        (
            Role::Tool,
            MessageContent::ToolResult {
                tool_call_id,
                output,
                is_error,
            },
        ) => {
            // Gemini matches responses back to calls by function name, not id.
            // The dispatcher in agent::run records the original name in
            // `tool_call_id` for us by virtue of the OpenAI/Anthropic loop
            // shape — but we cannot rely on that, so we encode the original
            // call name as the prefix of `tool_call_id`. For safety, if the
            // id doesn't parse, fall back to "sql_query" (the only tool
            // registered in v0.2 / v0.3); this avoids dropping the message.
            let function_name = extract_function_name(tool_call_id);
            Some(json!({
                "role":  "user",
                "parts": [{
                    "functionResponse": {
                        "name": function_name,
                        "response": {
                            "output":   output,
                            "is_error": is_error,
                        }
                    }
                }],
            }))
        }

        // System messages live in `systemInstruction`; never in `contents`.
        _ => None,
    }
}

/// Best-effort extraction of a function name from our canonical
/// `tool_call_id`. The other providers issue ids like "call_abc123" or
/// "toolu_01XYZ" that don't carry the function name; we attach the name to
/// the id in `parse_response` below so this round-trip works.
///
/// ## P8 (v0.5.2 review): id format is provider-locked
///
/// The `"<name>::<id>"` shape is an internal contract between
/// `parse_response` (which produces the id) and this function (which
/// consumes it). It is **only** valid for tool calls that originated
/// from a Gemini response in the same agent loop iteration. Two
/// consequences operators should be aware of:
///
///   * **Do not** pass a foreign `tool_call_id` (e.g. one captured
///     from an OpenAI session log) into `ask.chat()` history when
///     switching providers mid-conversation. The Gemini path will
///     fail open by attributing the response to `sql_query`, which
///     can confuse the model.
///   * The fallback to `"sql_query"` is a v0.2-era safety net. If we
///     ever expose tool-call replay, the right fix is to refuse
///     unparseable ids outright and surface the conflict to the
///     caller — silently rewriting the function name hides real
///     bugs. Tracked in the v0.5.2 review as P8.
fn extract_function_name(tool_call_id: &str) -> String {
    // We stash `"<name>::<id>"` in tool_call_id below; strip the suffix.
    match tool_call_id.split_once("::") {
        Some((name, _id)) if !name.is_empty() => name.to_string(),
        _ => "sql_query".to_string(), // safe default for v0.2/v0.3
    }
}

// ---------- Response parsing ----------

#[derive(Debug, Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    candidates: Vec<Candidate>,
    /// P4 fix: token usage from Gemini.
    #[serde(default)]
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Debug, Deserialize)]
struct GeminiUsage {
    prompt_token_count: Option<i64>,
    candidates_token_count: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct Candidate {
    content: Option<CandidateContent>,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CandidateContent {
    #[serde(default)]
    parts: Vec<Part>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Part {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    function_call: Option<FunctionCall>,
}

#[derive(Debug, Deserialize)]
struct FunctionCall {
    name: String,
    #[serde(default)]
    args: Value,
}

fn parse_response(resp: GenerateResponse) -> Result<ProviderResponse> {
    let usage = resp.usage_metadata.and_then(|u| {
        match (u.prompt_token_count, u.candidates_token_count) {
            (Some(p), Some(c)) => Some(crate::providers::TokenUsage {
                prompt_tokens: p,
                completion_tokens: c,
            }),
            _ => None,
        }
    });
    let candidate = resp
        .candidates
        .into_iter()
        .next()
        .ok_or(AskError::EmptyResponse)?;
    let parts = candidate.content.map(|c| c.parts).unwrap_or_default();

    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for (idx, part) in parts.into_iter().enumerate() {
        if let Some(t) = part.text.filter(|s| !s.is_empty()) {
            text_parts.push(t);
        }
        if let Some(fc) = part.function_call {
            // Synthesise an id that round-trips the function name so that
            // when this assistant turn is replayed as history and we need
            // to send a `functionResponse` back, we can recover the name.
            // See `extract_function_name` above.
            let id = format!("{}::call_{idx}", fc.name);
            tool_calls.push(ToolCall {
                id,
                name: fc.name,
                arguments: fc.args,
            });
        }
    }

    let combined_text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };

    if !tool_calls.is_empty() {
        return Ok(ProviderResponse::ToolCalls {
            text: combined_text,
            calls: tool_calls,
            usage,
        });
    }

    // No tool calls and no text — that's an empty response. STOP without
    // text is unusual but defensible (e.g. safety blocks); surface it as
    // an explicit error so the SQL caller doesn't get a silent "".
    match combined_text {
        Some(text) => Ok(ProviderResponse::Final { text, usage }),
        None => {
            let reason = candidate
                .finish_reason
                .unwrap_or_else(|| "UNKNOWN".to_string());
            Err(AskError::Sql(format!(
                "Gemini returned no content (finishReason={reason})"
            )))
        }
    }
}
