//! Google Gemini embeddings — `:batchEmbedContents` v1beta endpoint.
//!
//! Endpoint URL pattern:
//!
//! ```text
//! POST {base_url}/v1beta/models/{model}:batchEmbedContents?key={api_key}
//! ```
//!
//! Request shape (note: each input is a *separate* `embedContentRequest`
//! and Gemini requires the `model` field to be **repeated inside each**
//! one — that is not a typo on our part):
//!
//! ```json
//! {
//!   "requests": [
//!     {
//!       "model": "models/text-embedding-004",
//!       "content": { "parts": [ { "text": "..." } ] }
//!     },
//!     ...
//!   ]
//! }
//! ```
//!
//! Response shape:
//!
//! ```json
//! {
//!   "embeddings": [
//!     { "values": [0.1, 0.2, ...] },
//!     ...
//!   ]
//! }
//! ```
//!
//! Order of `embeddings` matches order of `requests` — no sort needed.

use super::EmbeddingProvider;
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use serde::Deserialize;
use serde_json::json;
use std::thread;
use std::time::Duration;

const DEFAULT_BASE_URL: &str = "https://generativelanguage.googleapis.com";
const DEFAULT_MODEL: &str = "text-embedding-004";

const MAX_RETRIES: u32 = 3;
const BACKOFF_BASE_MS: u64 = 200;

pub struct GeminiEmbeddings {
    http: HttpClient,
    api_key: String,
    /// Raw model name as the operator typed it (e.g. "text-embedding-004"
    /// or "models/text-embedding-004"). Normalised at request time.
    model: String,
    base_url: String,
    dimensions: usize,
}

impl GeminiEmbeddings {
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

    /// Gemini accepts either `"text-embedding-004"` or the prefixed form
    /// `"models/text-embedding-004"`. The URL path and the `model` field
    /// inside each request both want different things, so normalise once:
    /// returns `(url_segment_without_prefix, request_field_with_prefix)`.
    fn split_model(&self) -> (String, String) {
        let bare = self
            .model
            .strip_prefix("models/")
            .unwrap_or(&self.model)
            .to_string();
        let prefixed = format!("models/{bare}");
        (bare, prefixed)
    }
}

impl EmbeddingProvider for GeminiEmbeddings {
    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let (bare_model, prefixed_model) = self.split_model();
        // Gemini puts the API key in the URL query string, NOT the header.
        let url = format!(
            "{}/v1beta/models/{}:batchEmbedContents?key={}",
            self.base_url.trim_end_matches('/'),
            bare_model,
            self.api_key,
        );

        let requests: Vec<_> = texts
            .iter()
            .map(|t| {
                json!({
                    "model": prefixed_model,
                    "content": { "parts": [ { "text": t } ] },
                })
            })
            .collect();

        let body = json!({ "requests": requests });

        // Empty header slice — auth is in the query string. We still
        // route through `HttpClient` for the central timeout policy.
        let headers: [(&str, &str); 0] = [];
        // P2 fix: retry with exponential backoff for transient failures.
        let parsed: BatchResponse = self.embed_with_retry(&url, &headers, &body)?;

        if let Some(first) = parsed.embeddings.first() {
            let actual = first.values.len();
            if actual != self.dimensions {
                return Err(AskError::InvalidConfig {
                    key: "embedding_dimensions",
                    message: format!(
                        "gemini model `{}` returned {actual}-D vectors but \
                         pg_ask.embedding_dimensions = {} — set the GUC to match, \
                         or recreate ask._memories with the right width \
                         (text-embedding-004 = 768)",
                        self.model, self.dimensions
                    ),
                });
            }
        }

        Ok(parsed.embeddings.into_iter().map(|e| e.values).collect())
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }
}

impl GeminiEmbeddings {
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
                    let delay = Duration::from_millis(
                        BACKOFF_BASE_MS * 2u64.saturating_pow(attempt)
                    );
                    pgrx::warning!(
                        "pg_ask gemini embedding: attempt {}/{} failed ({e}), retrying in {}ms",
                        attempt + 1, MAX_RETRIES + 1, delay.as_millis()
                    );
                    thread::sleep(delay);
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_err.unwrap_or_else(|| AskError::Transport("all retry attempts exhausted".into())))
    }
}

// ---------- Response shape ----------

#[derive(Debug, Deserialize)]
struct BatchResponse {
    embeddings: Vec<EmbedItem>,
}

#[derive(Debug, Deserialize)]
struct EmbedItem {
    values: Vec<f32>,
}
