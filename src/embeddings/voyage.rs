//! Voyage AI native embeddings — `POST https://api.voyageai.com/v1/embeddings`.
//!
//! Voyage *also* ships an OpenAI-compatible shim that works through
//! `embeddings::openai` with `pg_ask.embedding_provider = 'voyage-openai'`
//! and `pg_ask.embedding_base_url = 'https://api.voyageai.com'`. The
//! reason we keep a dedicated module is that the native endpoint accepts
//! an `input_type` ("query" vs "document") parameter that materially
//! changes embedding quality for retrieval — and the memory layer needs
//! to call it both ways (with `"document"` at `remember()` time and
//! `"query"` at `recall()` time).
//!
//! Today the trait does not yet carry that distinction, so we default to
//! `"document"` (closer to a "store this for later" semantic). When the
//! `EmbeddingProvider` trait gains a `kind: EmbedKind` argument we will
//! wire it through here.
//!
//! Request shape:
//!
//! ```json
//! {
//!   "model": "voyage-3",
//!   "input": ["...", "..."],
//!   "input_type": "document"
//! }
//! ```
//!
//! Response shape (same as OpenAI, sorted by `index` defensively):
//!
//! ```json
//! { "data": [ { "index": 0, "embedding": [..] }, ... ] }
//! ```

use super::EmbeddingProvider;
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use serde::Deserialize;
use serde_json::json;

const DEFAULT_BASE_URL: &str = "https://api.voyageai.com";
const DEFAULT_MODEL: &str = "voyage-3";

pub struct VoyageEmbeddings {
    http: HttpClient,
    api_key: String,
    model: String,
    base_url: String,
    dimensions: usize,
}

impl VoyageEmbeddings {
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

impl EmbeddingProvider for VoyageEmbeddings {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let url = format!("{}/v1/embeddings", self.base_url.trim_end_matches('/'));
        let bearer = format!("Bearer {}", self.api_key);
        let headers: [(&str, &str); 1] = [("authorization", bearer.as_str())];

        // input_type defaulted to "document" — see module comment for why.
        let body = json!({
            "model": self.model,
            "input": texts,
            "input_type": "document",
        });

        let parsed: EmbedResponse = self.http.post_json(&url, &headers, &body)?;
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);

        if let Some(first) = data.first() {
            let actual = first.embedding.len();
            if actual != self.dimensions {
                return Err(AskError::InvalidConfig {
                    key: "embedding_dimensions",
                    message: format!(
                        "voyage model `{}` returned {actual}-D vectors but \
                         pg_ask.embedding_dimensions = {} — set the GUC to match, \
                         or recreate ask._memories with the right width \
                         (voyage-3 = 1024, voyage-3-large = 1024, voyage-code-3 = 1024)",
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
