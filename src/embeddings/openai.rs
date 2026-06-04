//! OpenAI `/v1/embeddings` provider, plus every OpenAI-compatible host.
//!
//! Request shape:
//!
//! ```json
//! {
//!   "model": "text-embedding-3-small",
//!   "input": ["text one", "text two", ...]
//! }
//! ```
//!
//! Response shape:
//!
//! ```json
//! {
//!   "data": [
//!     { "index": 0, "embedding": [0.1, ...] },
//!     { "index": 1, "embedding": [0.2, ...] }
//!   ],
//!   "model": "text-embedding-3-small",
//!   "usage": { ... }
//! }
//! ```
//!
//! We re-sort by `index` to be defensive even though the API returns the
//! `data` array in input order.

use super::EmbeddingProvider;
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use serde::Deserialize;
use serde_json::json;
use std::thread;
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MODEL: &str = "text-embedding-3-small";

/// Maximum number of retry attempts for transient embedding failures.
const MAX_RETRIES: u32 = 3;
/// Base delay for exponential backoff (milliseconds).
const BACKOFF_BASE_MS: u64 = 200;
/// Maximum jitter factor (0.0–0.5) added to avoid thundering herd.
const JITTER_FRACTION: f64 = 0.25;

pub struct OpenAiEmbeddings {
    http: HttpClient,
    api_key: String,
    model: String,
    base_url: String,
    dimensions: usize,
}

impl OpenAiEmbeddings {
    pub fn new(cfg: &RuntimeConfig, http: HttpClient, api_key: String) -> Self {
        Self {
            http,
            api_key,
            model: cfg
                .embedding_model
                .clone()
                .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            base_url: cfg
                .embedding_base_url
                .clone()
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
            dimensions: cfg.embedding_dimensions,
        }
    }
}

impl EmbeddingProvider for OpenAiEmbeddings {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'));
        let bearer = format!("Bearer {}", self.api_key);
        let headers: [(&str, &str); 1] = [("authorization", bearer.as_str())];

        let body = json!({
            "model": self.model,
            "input": texts,
        });

        // P2 fix: exponential backoff with jitter for transient failures.
        // Retries on 429 (rate limit) and 5xx (server error). All other
        // errors (4xx, transport, parse) surface immediately.
        let parsed: EmbedResponse = self.embed_with_retry(&url, &headers, &body)?;
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);

        // Cross-check the model's actual width against our declared one.
        // A mismatch here would silently corrupt _memories at INSERT time
        // (pgvector throws but the error message is opaque) — better to
        // surface the operator-actionable form.
        if let Some(first) = data.first() {
            let actual = first.embedding.len();
            if actual != self.dimensions {
                return Err(AskError::InvalidConfig {
                    key: "embedding_dimensions",
                    message: format!(
                        "model `{}` returned {actual}-D vectors but pg_ask.embedding_dimensions = {} \
                         — set the GUC to match, or recreate ask._memories with the right width",
                        self.model, self.dimensions
                    ),
                });
            }
        }

        Ok(data.into_iter().map(|d| d.embedding).collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

impl OpenAiEmbeddings {
    /// Execute the embedding HTTP call with exponential backoff + jitter.
    ///
    /// Retries are only attempted for retriable errors:
    /// - HTTP 429 (Too Many Requests)
    /// - HTTP 5xx (server-side failures)
    /// - Transport errors (network timeout, connection reset)
    ///
    /// Non-retriable errors (4xx client errors other than 429, JSON parse
    /// failures) surface immediately.
    fn embed_with_retry<T: serde::de::DeserializeOwned>(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> Result<T> {
        let mut last_err: Option<AskError> = None;

        for attempt in 0..=MAX_RETRIES {
            match self.http.post_json::<T>(url, headers, body) {
                Ok(resp) => return Ok(resp),
                Err(
                    ref e @ AskError::ProviderHttp { .. }
                    | ref e @ AskError::Transport(_),
                ) => {
                    let retriable = match e {
                        AskError::ProviderHttp { status, .. } => {
                            *status == 429 || (500..600).contains(status)
                        }
                        AskError::Transport(_) => true,
                        _ => false,
                    };

                    if !retriable || attempt == MAX_RETRIES {
                        return Err(e.clone());
                    }

                    last_err = Some(e.clone());
                    let delay = Self::backoff_delay(attempt);
                    pgrx::warning!(
                        "pg_ask embedding: attempt {}/{} failed ({e}), retrying in {}ms",
                        attempt + 1,
                        MAX_RETRIES + 1,
                        delay.as_millis()
                    );
                    thread::sleep(delay);
                }
                Err(e) => return Err(e),
            }
        }

        // Unreachable in practice — the loop always returns via the
        // Ok or non-retriable Err arms. But the compiler doesn't know that.
        Err(last_err.unwrap_or_else(|| AskError::Transport("all retry attempts exhausted".into())))
    }

    /// Compute exponential backoff delay: `BACKOFF_BASE_MS * 2^attempt`
    /// with ±JITTER_FRACTION randomness.
    fn backoff_delay(attempt: u32) -> Duration {
        let base = BACKOFF_BASE_MS * 2u64.saturating_pow(attempt);
        // Simple jitter: use the attempt counter as a cheap pseudo-random
        // seed. We don't need cryptographic randomness — just enough to
        // desynchronise concurrent retries from the same backend.
        let jitter = ((attempt as f64 * 0.618) % 1.0) * (base as f64) * JITTER_FRACTION;
        let delay_ms = base + jitter as u64;
        Duration::from_millis(delay_ms)
    }
}

// ---------- Response shape ----------

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedItem>,
}

#[derive(Debug, Deserialize)]
struct EmbedItem {
    index: usize,
    embedding: Vec<f32>,
}
