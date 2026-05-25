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

const DEFAULT_BASE_URL: &str = "https://api.openai.com";
const DEFAULT_MODEL: &str = "text-embedding-3-small";

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

        let parsed: EmbedResponse = self.http.post_json(&url, &headers, &body)?;
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
