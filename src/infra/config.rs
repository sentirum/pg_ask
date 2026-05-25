//! Layered configuration.
//!
//! Order of resolution for every key (first hit wins):
//!
//! 1. Session GUC: `SET LOCAL pg_ask.<key> = '…'`
//! 2. Role / database GUC: `ALTER ROLE x SET pg_ask.<key> = '…'`
//! 3. Table fallback: `pg_ask._config(key, value)`
//!
//! GUCs are registered in `lib.rs::_PG_init`. Strings storing secrets are
//! flagged `SUPERUSER_ONLY | NO_SHOW_ALL` so `SHOW ALL` / `pg_settings`
//! redact them for non-superusers.
//!
//! Inside the agent loop we never read from here directly — we snapshot a
//! [`RuntimeConfig`] once per request and pass it down.

use crate::infra::errors::{AskError, Result};
use crate::infra::spi;
use pgrx::guc::GucSetting;
use std::ffi::CString;

// ---------- GUC handles (registered in lib.rs::_PG_init) ----------

pub static PROVIDER: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static API_KEY: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static MODEL: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static BASE_URL: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static MAX_TOKENS: GucSetting<i32> = GucSetting::<i32>::new(4096);
pub static MAX_ITERATIONS: GucSetting<i32> = GucSetting::<i32>::new(16);
pub static READONLY: GucSetting<bool> = GucSetting::<bool>::new(true);
pub static HTTP_CONNECT_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(10_000);
pub static HTTP_TOTAL_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(120_000);
pub static TOOL_STATEMENT_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(10_000);
pub static TOOL_MAX_ROWS: GucSetting<i32> = GucSetting::<i32>::new(200);
pub static TRACE_ENABLED: GucSetting<bool> = GucSetting::<bool>::new(true);

// ---------- Snapshot ----------

/// Immutable view of all settings relevant to a single agent run.
///
/// Build one with [`RuntimeConfig::load`] at the top of every public entry
/// point. The agent and tool layers receive this by reference; they never
/// touch globals.
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub provider: String,
    pub api_key: String,
    pub model: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: u32,
    pub max_iterations: u32,
    pub readonly: bool,
    pub http_connect_timeout_ms: u64,
    pub http_total_timeout_ms: u64,
    pub tool_statement_timeout_ms: u64,
    pub tool_max_rows: usize,
    /// Picked up by the telemetry writer (no-op until v0.2).
    #[allow(dead_code)]
    pub trace_enabled: bool,
}

impl RuntimeConfig {
    /// Build a snapshot from current GUC values, falling back to the table for
    /// unset string-valued keys.
    pub fn load() -> Result<Self> {
        Ok(Self {
            provider: required_string("provider", &PROVIDER)?,
            api_key: required_string("api_key", &API_KEY)?,
            model: optional_string("model", &MODEL),
            base_url: optional_string("base_url", &BASE_URL),
            max_tokens: clamp_pos("max_tokens", MAX_TOKENS.get())?,
            max_iterations: clamp_pos("max_iterations", MAX_ITERATIONS.get())?,
            readonly: READONLY.get(),
            http_connect_timeout_ms: clamp_pos_u64(
                "http_connect_timeout_ms",
                HTTP_CONNECT_TIMEOUT_MS.get(),
            )?,
            http_total_timeout_ms: clamp_pos_u64(
                "http_total_timeout_ms",
                HTTP_TOTAL_TIMEOUT_MS.get(),
            )?,
            tool_statement_timeout_ms: clamp_pos_u64(
                "tool_statement_timeout_ms",
                TOOL_STATEMENT_TIMEOUT_MS.get(),
            )?,
            tool_max_rows: usize::try_from(TOOL_MAX_ROWS.get().max(1)).unwrap_or(200),
            trace_enabled: TRACE_ENABLED.get(),
        })
    }
}

// ---------- Public API (used by `api/config.rs`) ----------

/// Insert or update a row in `pg_ask._config`. Used by the SQL-callable
/// `pg_ask.config(key, value)`. Validates the key against the allow-list of
/// known config keys so typos surface immediately.
pub fn upsert_table(key: &str, value: &str) -> Result<()> {
    if !is_known_key(key) {
        return Err(AskError::InvalidConfig {
            key: "config",
            message: format!("unknown config key `{key}`"),
        });
    }
    pgrx::Spi::run_with_args(
        "INSERT INTO pg_ask._config(key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        &[key.into(), value.into()],
    )?;
    Ok(())
}

/// Read a config value from the table fallback only. Used by the SQL-callable
/// `pg_ask.get_config(key)`; for the agent's own resolution use
/// [`RuntimeConfig::load`].
pub fn read_table(key: &str) -> Result<Option<String>> {
    spi::select_one_text_with(
        "SELECT value FROM pg_ask._config WHERE key = $1",
        &[key.into()],
    )
}

// ---------- Internals ----------

fn required_string(key: &'static str, guc: &GucSetting<Option<CString>>) -> Result<String> {
    if let Some(v) = guc_string(guc) {
        return Ok(v);
    }
    read_table(key)?.ok_or(AskError::MissingConfig(key))
}

fn optional_string(key: &str, guc: &GucSetting<Option<CString>>) -> Option<String> {
    guc_string(guc).or_else(|| read_table(key).ok().flatten())
}

fn guc_string(guc: &GucSetting<Option<CString>>) -> Option<String> {
    guc.get()
        .and_then(|c| c.into_string().ok())
        .filter(|s| !s.is_empty())
}

fn clamp_pos(key: &'static str, v: i32) -> Result<u32> {
    if v <= 0 {
        return Err(AskError::InvalidConfig {
            key,
            message: format!("must be > 0, got {v}"),
        });
    }
    Ok(v as u32)
}

fn clamp_pos_u64(key: &'static str, v: i32) -> Result<u64> {
    if v <= 0 {
        return Err(AskError::InvalidConfig {
            key,
            message: format!("must be > 0, got {v}"),
        });
    }
    Ok(v as u64)
}

const KNOWN_KEYS: &[&str] = &[
    "provider",
    "api_key",
    "model",
    "base_url",
    "max_tokens",
    "max_iterations",
    "readonly",
    "http_connect_timeout_ms",
    "http_total_timeout_ms",
    "tool_statement_timeout_ms",
    "tool_max_rows",
    "trace_enabled",
];

fn is_known_key(key: &str) -> bool {
    KNOWN_KEYS.contains(&key)
}
