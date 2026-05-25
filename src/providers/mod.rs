//! Provider abstraction.
//!
//! A `Provider` knows how to take a system prompt + message history + tool
//! specs and return either a final assistant message or one or more tool-call
//! requests. The agent loop in `crate::agent` is wire-format agnostic;
//! provider implementations translate to and from the canonical types
//! defined in `wire.rs`.
//!
//! v0.2 adds OpenAI and Gemini. Anything that speaks the OpenAI Chat
//! Completions wire format (Groq, Together, Ollama, vLLM, …) will work
//! through the OpenAI provider by overriding `base_url`.

pub mod anthropic;
pub mod wire;

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;

#[allow(unused_imports)]
pub use wire::{Message, MessageContent, ProviderResponse, Role, ToolCall, ToolSpec};

/// Wire-format-agnostic chat provider.
pub trait Provider {
    fn complete(
        &self,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ProviderResponse>;
}

/// Build a provider from a runtime config snapshot. The provider borrows
/// the [`HttpClient`] so every call shares connection pools and timeouts.
pub fn build(cfg: &RuntimeConfig, http: HttpClient) -> Result<Box<dyn Provider>> {
    match cfg.provider.as_str() {
        "anthropic" => Ok(Box::new(anthropic::AnthropicProvider::new(cfg, http))),
        other => Err(AskError::UnsupportedProvider(other.to_string())),
    }
}
