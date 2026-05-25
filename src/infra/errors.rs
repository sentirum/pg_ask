//! Crate-wide error type.
//!
//! Every fallible boundary returns `Result<T, AskError>`. The only place an
//! `AskError` becomes a PostgreSQL `ERROR` is at the `#[pg_extern]` boundary
//! in `crate::api`, via `pgrx::error!`. Keeping panics out of the SPI
//! machinery is what lets us write straightforward `?`-chains everywhere else.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum AskError {
    #[error("missing config key: pg_ask.{0}")]
    MissingConfig(&'static str),

    #[error("invalid config value for pg_ask.{key}: {message}")]
    InvalidConfig { key: &'static str, message: String },

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

    #[error("SQL guard rejected statement: {0}")]
    GuardRejected(String),

    #[error("SQL error: {0}")]
    Sql(String),
}

pub type Result<T> = std::result::Result<T, AskError>;

impl From<pgrx::spi::SpiError> for AskError {
    fn from(e: pgrx::spi::SpiError) -> Self {
        AskError::Sql(e.to_string())
    }
}
