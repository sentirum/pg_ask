//! OpenAI Chat Completions provider.
//!
//! Spec: <https://platform.openai.com/docs/api-reference/chat>
//!
//! Also speaks every OpenAI-compatible endpoint (Groq, Together, Mistral,
//! Ollama, vLLM, LM Studio, …) when `pg_ask.base_url` is overridden.
//!
//! Wire-format differences from Anthropic that this module hides:
//!
//! * The system prompt rides inside `messages[]` as `{role:"system"}`
//!   rather than a top-level field.
//! * Assistant messages carry a plain string `content` plus an optional
//!   `tool_calls[]` array (no block list).
//! * `tool_calls[].function.arguments` is a **JSON string** the model
//!   produced, not a parsed JSON value — we parse on the way in.
//! * Tool results are `{role:"tool", tool_call_id, content}`.
//! * Auth header is `Authorization: Bearer <key>`.

use super::wire::{Message, MessageContent, ProviderResponse, Role, ToolCall, ToolSpec};
use super::Provider;
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use serde::Deserialize;
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MODEL: &str = "gpt-4o-mini";

pub struct OpenAiProvider {
    http: HttpClient,
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: u32,
}

impl OpenAiProvider {
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

impl Provider for OpenAiProvider {
    fn complete(
        &self,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ProviderResponse> {
        let body = build_request(&self.model, self.max_tokens, system, history, tools);
        // URL construction: if base_url already contains the full
        // endpoint path (e.g. ZAI uses /api/paas/v4/chat/completions
        // instead of the OpenAI-standard /v1/chat/completions), respect
        // it. Otherwise append /v1/chat/completions.
        let base = self.base_url.trim_end_matches('/');
        let url = if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/v1/chat/completions")
        };
        let bearer = format!("Bearer {}", self.api_key);
        let headers: [(&str, &str); 1] = [("authorization", bearer.as_str())];

        let parsed: ChatResponse = self.http.post_json(&url, &headers, &body)?;
        parse_response(parsed)
    }
}

// ---------- Request construction ----------

fn build_request(
    model: &str,
    max_tokens: u32,
    system: &str,
    history: &[Message],
    tools: &[ToolSpec],
) -> Value {
    let mut messages: Vec<Value> = Vec::with_capacity(history.len() + 1);
    messages.push(json!({ "role": "system", "content": system }));
    messages.extend(history.iter().map(message_to_wire));

    let mut req = json!({
        "model":      model,
        "max_tokens": max_tokens,
        "messages":   messages,
    });

    if !tools.is_empty() {
        let tool_specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name":        t.name,
                        "description": t.description,
                        "parameters":  t.input_schema,
                    }
                })
            })
            .collect();
        req["tools"] = Value::Array(tool_specs);
    }

    req
}

fn message_to_wire(msg: &Message) -> Value {
    match (&msg.role, &msg.content) {
        (Role::User, MessageContent::Text(t)) => json!({ "role": "user", "content": t }),

        (Role::Assistant, MessageContent::Text(t)) => {
            json!({ "role": "assistant", "content": t })
        }

        (Role::Assistant, MessageContent::AssistantWithTools { text, tool_calls }) => {
            let calls: Vec<Value> = tool_calls
                .iter()
                .map(|c| {
                    // arguments must be a JSON-encoded string per the spec.
                    let args_str =
                        serde_json::to_string(&c.arguments).unwrap_or_else(|_| "{}".to_string());
                    json!({
                        "id":       c.id,
                        "type":     "function",
                        "function": { "name": c.name, "arguments": args_str }
                    })
                })
                .collect();
            json!({
                "role":       "assistant",
                "content":    text.clone().unwrap_or_default(),
                "tool_calls": calls,
            })
        }

        (
            Role::Tool,
            MessageContent::ToolResult {
                tool_call_id,
                output,
                is_error,
            },
        ) => {
            // OpenAI has no `is_error` field; we prefix the output so the
            // model still notices the failure.
            let content = if *is_error {
                format!("ERROR: {output}")
            } else {
                output.clone()
            };
            json!({
                "role":         "tool",
                "tool_call_id": tool_call_id,
                "content":      content,
            })
        }

        // System messages belong in the top-level slot we prepended above,
        // not in history. Anything reaching this arm is a programming error.
        _ => json!({ "role": "user", "content": "[pg_ask: invalid message]" }),
    }
}

// ---------- Response parsing ----------

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    /// P4 fix: token usage from the provider.
    #[serde(default)]
    usage: Option<WireUsage>,
}

#[derive(Debug, Deserialize)]
struct WireUsage {
    prompt_tokens: i64,
    completion_tokens: i64,
}

#[derive(Debug, Deserialize)]
struct Choice {
    message: ChoiceMessage,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChoiceMessage {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<WireToolCall>>,
}

#[derive(Debug, Deserialize)]
struct WireToolCall {
    id: String,
    #[serde(rename = "type")]
    _kind: String,
    function: WireFunction,
}

#[derive(Debug, Deserialize)]
struct WireFunction {
    name: String,
    /// JSON-encoded argument blob; we parse it before handing to the dispatcher.
    arguments: String,
}

fn parse_response(resp: ChatResponse) -> Result<ProviderResponse> {
    let usage = resp.usage.map(|u| crate::providers::TokenUsage {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
    });

    let choice = resp
        .choices
        .into_iter()
        .next()
        .ok_or(AskError::EmptyResponse)?;

    let text = choice.message.content.filter(|s| !s.is_empty());
    let raw_calls = choice.message.tool_calls.unwrap_or_default();

    if raw_calls.is_empty() && choice.finish_reason.as_deref() != Some("tool_calls") {
        return match text {
            Some(t) => Ok(ProviderResponse::Final { text: t, usage }),
            None => Err(AskError::EmptyResponse),
        };
    }

    let calls: Vec<ToolCall> = raw_calls
        .into_iter()
        .map(|c| {
            // If the model emitted invalid JSON for arguments we forward the
            // raw string so the tool sees it and can return is_error=true
            // rather than panicking the loop.
            let arguments = serde_json::from_str::<Value>(&c.function.arguments)
                .unwrap_or_else(|_| json!({ "_raw": c.function.arguments }));
            ToolCall {
                id: c.id,
                name: c.function.name,
                arguments,
            }
        })
        .collect();

    Ok(ProviderResponse::ToolCalls { text, calls, usage })
}
