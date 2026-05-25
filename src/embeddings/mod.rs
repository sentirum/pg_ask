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

pub mod gemini;
pub mod openai;
pub mod voyage;

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
        // by setting `pg_ask.embedding_base_url`. `voyage-openai` is
        // Voyage's OpenAI-compat shim (no `input_type` support); for the
        // native Voyage endpoint use `voyage` below.
        "openai" | "openai-compat" | "together" | "voyage-openai" | "vllm" | "lmstudio"
        | "ollama" => Ok(Box::new(openai::OpenAiEmbeddings::new(cfg, http, api_key))),

        // Voyage AI native endpoint. Uses `input_type=document` today;
        // a future `EmbedKind` parameter on the trait will let us flip
        // it to `query` at recall time.
        "voyage" => Ok(Box::new(voyage::VoyageEmbeddings::new(cfg, http, api_key))),

        // Google Gemini `:batchEmbedContents` v1beta. Auth goes in the
        // URL query string; we still route through HttpClient for the
        // shared timeout policy.
        "gemini" | "google" => Ok(Box::new(gemini::GeminiEmbeddings::new(cfg, http, api_key))),

        other => Err(AskError::UnsupportedProvider(other.to_string())),
    }
}
