//! Crate-wide error type. Surfaces as PG `ERROR` via `pgrx::error!` at the boundary.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AskError {
    #[error("missing config key: {0}")]
    MissingConfig(&'static str),

    #[error("unsupported provider: {0}")]
    UnsupportedProvider(String),

    #[error("provider returned no content")]
    EmptyResponse,

    #[error("provider HTTP error: {status} — {body}")]
    ProviderHttp { status: u16, body: String },

    #[error("provider request failed: {0}")]
    Transport(String),

    #[error("provider returned malformed JSON: {0}")]
    BadJson(#[from] serde_json::Error),

    #[error("tool `{name}` failed: {message}")]
    Tool { name: String, message: String },

    #[error("agent exceeded max iterations ({max})")]
    MaxIterations { max: u32 },

    #[error("SQL error: {0}")]
    Sql(String),
}

pub type Result<T> = std::result::Result<T, AskError>;
