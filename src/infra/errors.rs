//! Crate-wide error type.
//!
//! Every fallible boundary returns `Result<T, AskError>`. The only place an
//! `AskError` becomes a PostgreSQL `ERROR` is at the `#[pg_extern]` boundary
//! in `crate::api`, via [`raise_as_pg_error`]. Keeping panics out of the SPI
//! machinery is what lets us write straightforward `?`-chains everywhere else.
//!
//! ## SQLSTATE mapping (S2 fix)
//!
//! Each variant maps to a PostgreSQL SQLSTATE via [`AskError::sql_error_code`].
//! The API layer calls [`raise_as_pg_error`] instead of `pgrx::error!` so
//! monitoring tools and `WHEN ... SQLSTATE ...` handlers can discriminate
//! pg_ask errors by category rather than treating everything as
//! `ERRCODE_INTERNAL_ERROR` (XX000).

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

impl Clone for AskError {
    fn clone(&self) -> Self {
        match self {
            Self::MissingConfig(k) => Self::MissingConfig(k),
            Self::InvalidConfig { key, message } => Self::InvalidConfig {
                key: *key,
                message: message.clone(),
            },
            Self::UnsupportedProvider(p) => Self::UnsupportedProvider(p.clone()),
            Self::EmptyResponse => Self::EmptyResponse,
            Self::ProviderHttp { status, body } => Self::ProviderHttp {
                status: *status,
                body: body.clone(),
            },
            Self::Transport(t) => Self::Transport(t.clone()),
            // serde_json::Error doesn't impl Clone; stringify it.
            Self::BadJson(e) => Self::Transport(e.to_string()),
            Self::Tool { name, message } => Self::Tool {
                name: name.clone(),
                message: message.clone(),
            },
            Self::MaxIterations { max } => Self::MaxIterations { max: *max },
            Self::GuardRejected(s) => Self::GuardRejected(s.clone()),
            Self::Sql(s) => Self::Sql(s.clone()),
        }
    }
}

pub type Result<T> = std::result::Result<T, AskError>;

impl AskError {
    /// Return the PostgreSQL SQLSTATE (via pgrx's `PgSqlErrorCode`) that
    /// best describes this error category.
    ///
    /// Mapping rationale:
    ///
    /// | Variant                | SQLSTATE  | Rationale                                    |
    /// |------------------------|-----------|----------------------------------------------|
    /// | `MissingConfig`        | 42601     | syntax / invalid parameter — operator fixable|
    /// | `InvalidConfig`        | 22023     | invalid parameter value                      |
    /// | `UnsupportedProvider`  | 42601     | invalid parameter — wrong provider name      |
    /// | `EmptyResponse`        | 0A000     | feature not supported (provider glitch)      |
    /// | `ProviderHttp`         | 58000     | external server error (5xx) / client (4xx)   |
    /// | `Transport`            | 58000     | network / I/O failure                        |
    /// | `BadJson`              | 22P02     | invalid text representation (type mismatch)  |
    /// | `Tool`                 | 38000     | external routine exception                   |
    /// | `MaxIterations`        | 54000     | program limit exceeded                       |
    /// | `GuardRejected`        | 42501     | insufficient privilege (policy violation)    |
    /// | `Sql`                  | 38000     | external routine exception (SPI)             |
    pub fn sql_error_code(&self) -> pgrx::PgSqlErrorCode {
        use pgrx::PgSqlErrorCode;
        match self {
            Self::MissingConfig(_) | Self::UnsupportedProvider(_) => {
                PgSqlErrorCode::ERRCODE_SYNTAX_ERROR
            }
            Self::InvalidConfig { .. } => PgSqlErrorCode::ERRCODE_INVALID_PARAMETER_VALUE,
            Self::EmptyResponse => PgSqlErrorCode::ERRCODE_FEATURE_NOT_SUPPORTED,
            Self::ProviderHttp { .. } => PgSqlErrorCode::ERRCODE_EXTERNAL_ROUTINE_EXCEPTION,
            Self::Transport(_) => PgSqlErrorCode::ERRCODE_CONNECTION_EXCEPTION,
            Self::BadJson(_) => {
                PgSqlErrorCode::ERRCODE_INVALID_TEXT_REPRESENTATION
            }
            Self::Tool { .. } | Self::Sql(_) => {
                PgSqlErrorCode::ERRCODE_EXTERNAL_ROUTINE_EXCEPTION
            }
            Self::MaxIterations { .. } => {
                PgSqlErrorCode::ERRCODE_PROGRAM_LIMIT_EXCEEDED
            }
            Self::GuardRejected(_) => PgSqlErrorCode::ERRCODE_INSUFFICIENT_PRIVILEGE,
        }
    }
}

/// Raise `err` as a PostgreSQL ERROR with the appropriate SQLSTATE.
///
/// This is the single point where an [`AskError`] becomes visible to
/// the database client. Every `#[pg_extern]` entry point should call
/// this instead of `pgrx::error!("...", e)` so the error carries a
/// meaningful SQLSTATE for monitoring / `WHEN` handlers.
///
/// ```ignore
/// match result {
///     Ok(val) => val,
///     Err(e) => raise_as_pg_error(&e),
/// }
/// ```
#[cold]
pub fn raise_as_pg_error(err: &AskError) -> ! {
    pgrx::ereport!(
        pgrx::PgLogLevel::ERROR,
        err.sql_error_code(),
        format_args!("{err}")
    );
    unreachable!("ereport(ERROR, ...) does not return")
}

impl From<pgrx::spi::SpiError> for AskError {
    fn from(e: pgrx::spi::SpiError) -> Self {
        AskError::Sql(e.to_string())
    }
}
