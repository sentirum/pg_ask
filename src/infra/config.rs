//! Layered configuration.
//!
//! Order of resolution for every key (first hit wins):
//!
//! 1. Session GUC: `SET LOCAL pg_ask.<key> = '…'`
//! 2. Role / database GUC: `ALTER ROLE x SET pg_ask.<key> = '…'`
//! 3. Table fallback: `ask._config(key, value)`
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
pub static MAX_ITERATIONS: GucSetting<i32> = GucSetting::<i32>::new(24);
pub static READONLY: GucSetting<bool> = GucSetting::<bool>::new(true);
pub static HTTP_CONNECT_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(10_000);
pub static HTTP_TOTAL_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(120_000);
pub static TOOL_STATEMENT_TIMEOUT_MS: GucSetting<i32> = GucSetting::<i32>::new(10_000);
pub static TOOL_MAX_ROWS: GucSetting<i32> = GucSetting::<i32>::new(200);
pub static TRACE_ENABLED: GucSetting<bool> = GucSetting::<bool>::new(true);

/// Master switch for the event outbox (ADR-0017). When `false`,
/// `ask.emit()` is a no-op returning NULL, so an install that doesn't use
/// reverse notifications pays nothing and exposes no channel. Default
/// `false` (opt-in): emitting events is only useful when a listener (senti)
/// is actually consuming them.
pub static EVENTS_ENABLED: GucSetting<bool> = GucSetting::<bool>::new(false);

/// Soft cap on the schema dump injected into the system prompt, measured
/// in characters (a rough proxy for tokens at ~4 chars/token).
///
/// When the full schema render exceeds this, `schema::summarize` falls
/// back to a tables-only listing and exposes the `describe_table` tool so
/// the model can pull column detail on demand. Keeps the prompt cheap on
/// real-world (hundreds-of-tables) databases.
pub static SCHEMA_CHAR_BUDGET: GucSetting<i32> = GucSetting::<i32>::new(16_000);

// ---------- Embedding / memory (v0.3) ----------
//
// Kept as a separate provider stack from the chat layer so operators can mix
// (OpenAI embeddings + Anthropic chat, etc.). All keys live in pg_ask.* GUCs;
// the api_key one is SUPERUSER_ONLY | NO_SHOW_ALL.

pub static EMBEDDING_PROVIDER: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);
pub static EMBEDDING_API_KEY: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);
pub static EMBEDDING_MODEL: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);
pub static EMBEDDING_BASE_URL: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);
pub static EMBEDDING_DIMENSIONS: GucSetting<i32> = GucSetting::<i32>::new(1536);

/// Blend weight in [0,1] between cosine similarity and full-text BM25-ish
/// rank for `ask.recall`. 1.0 = pure vector, 0.0 = pure FTS. Default
/// 0.7 leans on the embedding while keeping keyword anchors honest.
pub static MEMORY_SEARCH_ALPHA: GucSetting<f64> = GucSetting::<f64>::new(0.7);

/// Global kill-switch for the memory layer. When `false`, the `recall`
/// tool is not registered and the memory.* SQL surface returns an error.
/// Honours the pgvector-absent case too: the layer is functionally off
/// regardless of this flag if `_memories` does not exist.
pub static MEMORY_ENABLED: GucSetting<bool> = GucSetting::<bool>::new(true);

// ---------- v0.4 — Tooling expansion ----------

/// Master switch for the `http_fetch` tool. Default `false` because
/// calling arbitrary URLs from inside a database backend is a significant
/// expansion of the attack surface. Operators opt-in explicitly.
pub static ALLOW_HTTP: GucSetting<bool> = GucSetting::<bool>::new(false);

/// Comma-separated list of allowed URL prefixes for `http_fetch`.
/// Empty string means "deny everything" (belt-and-suspenders). A
/// request is allowed only if its URL starts with one of these prefixes.
pub static HTTP_ALLOW_LIST: GucSetting<Option<CString>> = GucSetting::<Option<CString>>::new(None);

/// When `on`, `http_fetch` will permit literal-IP hosts in private /
/// loopback / link-local / CGNAT ranges. Off by default to make SSRF
/// through a wrong allow-list entry harder. See C5 in the v0.5.2 review.
pub static ALLOW_PRIVATE_HOSTS: GucSetting<bool> = GucSetting::<bool>::new(false);

/// Comma-separated list of `schema.table.column` patterns that the
/// `sql_query` tool redacts before returning results to the model.
/// The cell text is replaced with `<redacted>`; the column name is
/// still visible in the header so the model knows the column exists.
pub static SENSITIVE_COLUMNS: GucSetting<Option<CString>> =
    GucSetting::<Option<CString>>::new(None);

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
    /// Snapshot of `pg_ask.events_enabled`. The emit path reads the GUC
    /// directly (`EVENTS_ENABLED.get()`); this field exists for parity with
    /// the rest of the config snapshot and future use.
    #[allow(dead_code)]
    pub events_enabled: bool,
    pub schema_char_budget: usize,

    // Embedding / memory snapshot. `embedding_*` are Options because the
    // memory layer is optional — only `remember` / `recall` need a key.
    pub embedding_provider: Option<String>,
    pub embedding_api_key: Option<String>,
    pub embedding_model: Option<String>,
    pub embedding_base_url: Option<String>,
    pub embedding_dimensions: usize,
    pub memory_search_alpha: f64,
    pub memory_enabled: bool,

    // v0.4
    pub allow_http: bool,
    pub http_allow_list: Vec<String>,
    /// When true, the http_fetch tool skips the private/loopback/CGNAT
    /// IP guard. Off by default; opt-in for self-hosted setups talking
    /// to internal services. See C5 in the v0.5.2 review.
    pub allow_private_hosts: bool,
    pub sensitive_columns: Vec<String>,
}

impl RuntimeConfig {
    /// Build a snapshot from current GUC values, falling back to the table for
    /// unset string-valued keys.
    pub fn load() -> Result<Self> {
        // The fixture provider runs off a disk-backed script and never
        // touches the network, so it has no reason to require an
        // api_key. Demanding one would force every pg_test fixture
        // setup to also `SELECT ask.config('api_key', 'unused')`,
        // which is just noise. For every real provider api_key stays
        // mandatory.
        let provider = required_string("provider", &PROVIDER)?;
        let api_key = if provider.trim().eq_ignore_ascii_case("fixture") {
            optional_string("api_key", &API_KEY).unwrap_or_default()
        } else {
            required_string("api_key", &API_KEY)?
        };

        let embedding_provider = optional_string("embedding_provider", &EMBEDDING_PROVIDER);
        let mut embedding_api_key = optional_string("embedding_api_key", &EMBEDDING_API_KEY);
        if embedding_api_key.is_none() {
            if let Some(ref ep) = embedding_provider {
                if ep.trim().eq_ignore_ascii_case(&provider) {
                    embedding_api_key = Some(api_key.clone());
                }
            }
        }

        Ok(Self {
            provider,
            api_key,
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
            events_enabled: EVENTS_ENABLED.get(),
            schema_char_budget: usize::try_from(SCHEMA_CHAR_BUDGET.get().max(512))
                .unwrap_or(16_000),
            embedding_provider,
            embedding_api_key,
            embedding_model: optional_string("embedding_model", &EMBEDDING_MODEL),
            embedding_base_url: optional_string("embedding_base_url", &EMBEDDING_BASE_URL),
            embedding_dimensions: usize::try_from(EMBEDDING_DIMENSIONS.get().max(8))
                .unwrap_or(1536),
            memory_search_alpha: MEMORY_SEARCH_ALPHA.get().clamp(0.0, 1.0),
            memory_enabled: MEMORY_ENABLED.get(),
            allow_http: ALLOW_HTTP.get(),
            http_allow_list: optional_comma_list("http_allow_list", &HTTP_ALLOW_LIST),
            allow_private_hosts: ALLOW_PRIVATE_HOSTS.get(),
            sensitive_columns: optional_comma_list("sensitive_columns", &SENSITIVE_COLUMNS),
        })
    }
}

// ---------- Public API (used by `api/config.rs`) ----------

/// Insert or update a row in `ask._config`. Used by the SQL-callable
/// `ask.config(key, value)`. Validates the key against the allow-list of
/// known config keys so typos surface immediately.
pub fn upsert_table(key: &str, value: &str) -> Result<()> {
    if !is_known_key(key) {
        return Err(AskError::InvalidConfig {
            key: "config",
            message: format!("unknown config key `{key}`"),
        });
    }
    pgrx::Spi::run_with_args(
        "INSERT INTO ask._config(key, value) VALUES ($1, $2)
         ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        &[key.into(), value.into()],
    )?;
    Ok(())
}

/// Read a config value from the table fallback only.
///
/// ## C3-bis follow-up (Gemini v0.5.2 review item 1.4)
///
/// v0.5.2 added `REVOKE ALL ON ask._config FROM PUBLIC` to lock the
/// secrets table down to operators. The public `ask.get_config(key)`
/// SQL function regained access via `security_definer` on the
/// `#[pg_extern]`, but the *internal* call sites —
/// `RuntimeConfig::load` → `required_string` / `optional_string` →
/// `read_table` — still hit the SQL under the caller's invoker
/// privileges. For a non-superuser calling `ask.chat()` or
/// `ask.ask()`, that meant every config lookup failed with
/// `permission denied for table _config` even though the public
/// surface was fixed.
///
/// The fix routes the SELECT through a SECURITY DEFINER helper
/// (`ask._config_get`) which the v0.5.3 migration script and
/// `sql/bootstrap.sql` both ship. The helper does no filtering of
/// its own — redaction still lives in `api::config::get_config`
/// (`is_secret(key)` → `***redacted***`) so this internal path can
/// still see the raw value, which is what `RuntimeConfig` actually
/// needs.
pub fn read_table(key: &str) -> Result<Option<String>> {
    spi::select_one_text_with("SELECT ask._config_get($1)", &[key.into()])
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
    "events_enabled",
    "schema_char_budget",
    "embedding_provider",
    "embedding_api_key",
    "embedding_model",
    "embedding_base_url",
    "embedding_dimensions",
    "memory_search_alpha",
    "memory_enabled",
    "allow_http",
    "http_allow_list",
    "allow_private_hosts",
    "sensitive_columns",
];

fn is_known_key(key: &str) -> bool {
    KNOWN_KEYS.contains(&key)
}

/// Parse a comma-separated config value into a cleaned Vec, with the
/// same GUC-then-`ask._config`-table fallback the scalar settings use.
/// Empty or unset → empty vec. Each item trimmed, deduplicated.
fn optional_comma_list(key: &str, guc: &GucSetting<Option<CString>>) -> Vec<String> {
    let s = guc_string(guc).or_else(|| read_table(key).ok().flatten());
    s.map(|val| {
        val.split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect()
    })
    .unwrap_or_default()
}
