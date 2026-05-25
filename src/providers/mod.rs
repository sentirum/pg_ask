//! Provider abstraction.
//!
//! Each provider knows how to take a list of messages + tool specs and return
//! either a final assistant message or a list of tool-call requests.
//!
//! The agent loop in `crate::agent` is wire-format agnostic; provider
//! implementations translate to/from the canonical types defined here.

use crate::error::Result;
use serde::{Deserialize, Serialize};

pub mod anthropic;
// pub mod openai;   // TODO
// pub mod gemini;   // TODO

/// A single message in a conversation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Polymorphic message body. Either plain text, an assistant turn that included
/// tool calls, or a tool result keyed back to a prior tool-call id.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    AssistantWithTools {
        text: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        tool_call_id: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON arguments as the model emitted them.
    pub arguments: serde_json::Value,
}

/// Specification of a tool exposed to the model.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    /// JSON Schema for the tool input.
    pub input_schema: serde_json::Value,
}

/// What a provider returns from a single `complete()` call.
#[derive(Debug)]
pub enum ProviderResponse {
    /// Final answer; no more iterations needed.
    Final { text: String },
    /// Model wants to call one or more tools before continuing.
    ToolCalls {
        /// Optional reasoning text the model emitted alongside the tool calls.
        text: Option<String>,
        calls: Vec<ToolCall>,
    },
}

/// Wire-format-agnostic chat provider.
pub trait Provider {
    fn complete(
        &self,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ProviderResponse>;
}

/// Build a provider from current `_config` settings.
pub fn from_config() -> Result<Box<dyn Provider>> {
    let name = crate::config::require("provider")?;
    match name.as_str() {
        "anthropic" => Ok(Box::new(anthropic::AnthropicProvider::from_config()?)),
        other => Err(crate::error::AskError::UnsupportedProvider(other.into())),
    }
}
