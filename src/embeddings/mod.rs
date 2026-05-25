//! Embedding provider abstraction.
//!
//! Mirrors the chat-provider stack in `crate::providers` but kept on its
//! own axis so operators can mix (e.g. OpenAI embeddings + Anthropic chat,
//! Voyage embeddings + Gemini chat).
//!
//! Implementations:
//!
//! * [`openai`] — `POST /v1/embeddings` (works with Together, Voyage's
//!   OpenAI-compat shim, vLLM, llama.cpp's `/v1/embeddings`).
//! * [`voyage`] — Voyage AI native API (slightly different request shape).
//! * [`gemini`] — `embedContent` v1beta endpoint.
//!
//! All providers go through the shared [`crate::infra::http::HttpClient`]
//! so timeouts are enforced centrally.

pub mod openai;

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;

/// One vector per input string.
pub trait EmbeddingProvider {
    /// Embed a batch of texts. Order of the output matches the input.
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>>;

    /// Expected vector width. The memory layer cross-checks this against
    /// the `_memories.embedding` column width so a misconfigured model
    /// fails loudly at `remember()` time, not silently on the first query.
    #[allow(dead_code)] // consumed by the memory-layer width audit (next milestone)
    fn dimensions(&self) -> usize;
}

/// Build an embedding provider from a runtime config snapshot. The
/// returned trait object borrows the [`HttpClient`].
pub fn build(cfg: &RuntimeConfig, http: HttpClient) -> Result<Box<dyn EmbeddingProvider>> {
    let provider = cfg
        .embedding_provider
        .as_deref()
        .ok_or(AskError::MissingConfig("embedding_provider"))?
        .trim()
        .to_ascii_lowercase();

    let api_key = cfg
        .embedding_api_key
        .clone()
        .ok_or(AskError::MissingConfig("embedding_api_key"))?;

    match provider.as_str() {
        // OpenAI + every OpenAI-compatible host. Operators pick the host
        // by setting `pg_ask.embedding_base_url`.
        "openai" | "openai-compat" | "together" | "voyage-openai" | "vllm" | "lmstudio"
        | "ollama" => Ok(Box::new(openai::OpenAiEmbeddings::new(cfg, http, api_key))),

        other => Err(AskError::UnsupportedProvider(other.to_string())),
    }
}
