//! Canonical message / tool-call types shared by every provider.
//!
//! These are the only types `agent::*` knows about. Concrete providers
//! convert to and from their wire format inside their own module.

use serde::{Deserialize, Serialize};

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

/// Polymorphic message body. Either plain text, an assistant turn that
/// included tool calls, or a tool result keyed back to a prior tool-call id.
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
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool input.
    pub input_schema: serde_json::Value,
}

/// What a provider returns from a single `complete()` call.
#[derive(Debug)]
pub enum ProviderResponse {
    /// Final answer; no more iterations needed.
    Final {
        text: String,
        /// Token usage from this response (P4 fix). Populated
        /// when the provider returns `usage` in its response.
        usage: Option<TokenUsage>,
    },
    /// Model wants to call one or more tools before continuing.
    ToolCalls {
        /// Optional reasoning text the model emitted alongside the tool calls.
        text: Option<String>,
        calls: Vec<ToolCall>,
        /// Token usage from this response.
        usage: Option<TokenUsage>,
    },
}

/// Token usage reported by the provider.
#[derive(Debug, Clone, Copy)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}
