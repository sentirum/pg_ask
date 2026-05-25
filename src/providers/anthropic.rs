//! Anthropic Messages API provider.
//!
//! Spec: <https://docs.anthropic.com/en/api/messages>

use super::{Message, MessageContent, Provider, ProviderResponse, Role, ToolCall, ToolSpec};
use crate::config;
use crate::error::{AskError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
const DEFAULT_MODEL: &str = "claude-sonnet-4-5";
const DEFAULT_MAX_TOKENS: u32 = 4096;
const API_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    api_key: String,
    model: String,
    base_url: String,
    max_tokens: u32,
}

impl AnthropicProvider {
    pub fn from_config() -> Result<Self> {
        Ok(Self {
            api_key: config::require("api_key")?,
            model: config::optional("model").unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            base_url: config::optional("base_url").unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            max_tokens: config::optional("max_tokens")
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_MAX_TOKENS),
        })
    }
}

impl Provider for AnthropicProvider {
    fn complete(
        &self,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ProviderResponse> {
        let body = build_request(&self.model, self.max_tokens, system, history, tools);

        let url = format!("{}/v1/messages", self.base_url.trim_end_matches('/'));
        let resp = ureq::post(&url)
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", API_VERSION)
            .set("content-type", "application/json")
            .send_json(body);

        let resp = match resp {
            Ok(r) => r,
            Err(ureq::Error::Status(status, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(AskError::ProviderHttp { status, body });
            }
            Err(e) => return Err(AskError::Transport(e.to_string())),
        };

        let parsed: MessagesResponse = resp
            .into_json()
            .map_err(|e| AskError::Transport(e.to_string()))?;

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
    let messages: Vec<Value> = history.iter().map(message_to_wire).collect();

    let mut req = json!({
        "model": model,
        "max_tokens": max_tokens,
        "system": system,
        "messages": messages,
    });

    if !tools.is_empty() {
        let tool_specs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
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
            let mut blocks: Vec<Value> = Vec::with_capacity(tool_calls.len() + 1);
            if let Some(t) = text.as_ref().filter(|s| !s.is_empty()) {
                blocks.push(json!({ "type": "text", "text": t }));
            }
            for call in tool_calls {
                blocks.push(json!({
                    "type": "tool_use",
                    "id": call.id,
                    "name": call.name,
                    "input": call.arguments,
                }));
            }
            json!({ "role": "assistant", "content": blocks })
        }
        (
            Role::Tool,
            MessageContent::ToolResult {
                tool_call_id,
                output,
                is_error,
            },
        ) => json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": tool_call_id,
                "content": output,
                "is_error": is_error,
            }],
        }),
        // System messages go in the top-level `system` field; the agent must not
        // place them in `history`. Anything else is a programming error.
        _ => json!({ "role": "user", "content": "[pg_ask: invalid message]" }),
    }
}

// ---------- Response parsing ----------

#[derive(Debug, Deserialize)]
struct MessagesResponse {
    content: Vec<ContentBlock>,
    stop_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ContentBlock {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Serialize)]
struct _Unused; // placate dead_code; kept for future content block kinds

fn parse_response(resp: MessagesResponse) -> Result<ProviderResponse> {
    let mut text_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for block in resp.content {
        match block {
            ContentBlock::Text { text } => text_parts.push(text),
            ContentBlock::ToolUse { id, name, input } => tool_calls.push(ToolCall {
                id,
                name,
                arguments: input,
            }),
            ContentBlock::Other => {}
        }
    }

    let combined_text = if text_parts.is_empty() {
        None
    } else {
        Some(text_parts.join("\n"))
    };

    if !tool_calls.is_empty() || resp.stop_reason.as_deref() == Some("tool_use") {
        return Ok(ProviderResponse::ToolCalls {
            text: combined_text,
            calls: tool_calls,
        });
    }

    match combined_text {
        Some(text) => Ok(ProviderResponse::Final { text }),
        None => Err(AskError::EmptyResponse),
    }
}
