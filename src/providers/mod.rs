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
pub mod fixture;
pub mod gemini;
pub mod openai;
pub mod wire;

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;

#[allow(unused_imports)]
pub use wire::{Message, MessageContent, ProviderResponse, Role, TokenUsage, ToolCall, ToolSpec};

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
    // Normalise so users can write 'OpenAI' / 'OPENAI' / 'openai' alike.
    let key = cfg.provider.trim().to_ascii_lowercase();
    match key.as_str() {
        "anthropic" | "claude" => Ok(Box::new(anthropic::AnthropicProvider::new(cfg, http))),

        // Test/CI-only provider: replays a disk-backed scripted
        // conversation. See providers::fixture for the wire format.
        // Lives in the regular registry (not behind a cfg flag) so
        // operators can use it for smoke tests in staging too.
        "fixture" => Ok(Box::new(fixture::FixtureProvider::new(cfg)?)),

        "gemini" | "google" | "google-genai" => {
            Ok(Box::new(gemini::GeminiProvider::new(cfg, http)))
        }

        // The OpenAI provider also handles every OpenAI-compatible
        // endpoint when `pg_ask.base_url` is set. Aliases let users pick a
        // hosting name even though the wire format is the same.
        "openai" | "openai-compat" | "groq" | "together" | "mistral" | "ollama" | "vllm"
        | "lmstudio" => Ok(Box::new(openai::OpenAiProvider::new(cfg, http))),

        _ => Err(AskError::UnsupportedProvider(cfg.provider.clone())),
    }
}
