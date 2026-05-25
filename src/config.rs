//! Configuration storage. Persisted in `pg_ask._config` so settings survive across
//! sessions and backends. For per-session overrides we can later add a GUC layer.

use crate::error::{AskError, Result};
use pgrx::prelude::*;

/// Set or update a config key.
///
/// ```sql
/// SELECT pg_ask.config('provider', 'anthropic');
/// SELECT pg_ask.config('api_key',  'sk-ant-...');
/// SELECT pg_ask.config('model',    'claude-sonnet-4-5');
/// SELECT pg_ask.config('readonly', 'true');
/// ```
#[pg_extern(schema = "pg_ask", security_definer)]
fn config(key: &str, value: &str) -> bool {
    Spi::run_with_args(
        "INSERT INTO pg_ask._config(key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        &[key.into(), value.into()],
    )
    .unwrap_or_else(|e| error!("pg_ask.config: {e}"));
    true
}

/// Read a single config value. NULL if unset.
#[pg_extern(schema = "pg_ask")]
fn get_config(key: &str) -> Option<String> {
    Spi::get_one_with_args::<String>(
        "SELECT value FROM pg_ask._config WHERE key = $1",
        &[key.into()],
    )
    .ok()
    .flatten()
}

/// Internal helper — fetch a required config value or fail with a typed error.
pub(crate) fn require(key: &'static str) -> Result<String> {
    Spi::get_one_with_args::<String>(
        "SELECT value FROM pg_ask._config WHERE key = $1",
        &[key.into()],
    )
    .map_err(|e| AskError::Sql(e.to_string()))?
    .ok_or(AskError::MissingConfig(key))
}

/// Internal helper — optional config value.
pub(crate) fn optional(key: &str) -> Option<String> {
    Spi::get_one_with_args::<String>(
        "SELECT value FROM pg_ask._config WHERE key = $1",
        &[key.into()],
    )
    .ok()
    .flatten()
}

/// Bool helper. Treats "true"/"1"/"yes"/"on" (case-insensitive) as true.
pub(crate) fn bool_flag(key: &str, default: bool) -> bool {
    match optional(key) {
        None => default,
        Some(v) => matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "true" | "1" | "yes" | "on"
        ),
    }
}
