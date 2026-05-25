//! # pg_ask
//!
//! PostgreSQL extension that runs an LLM-driven agent loop **inside** the
//! database backend.
//!
//! ```sql
//! CREATE EXTENSION pg_ask;
//!
//! -- Either via GUC (recommended):
//! SET LOCAL pg_ask.provider = 'anthropic';
//! SET LOCAL pg_ask.api_key  = 'sk-ant-...';
//! SET LOCAL pg_ask.model    = 'claude-sonnet-4-5';
//!
//! -- Or via table fallback (legacy / dev convenience):
//! SELECT pg_ask.config('provider', 'anthropic');
//! SELECT pg_ask.config('api_key',  'sk-ant-...');
//!
//! SELECT pg_ask.ask('How many orders shipped last week?');
//! SELECT pg_ask.sql('top 5 customers by revenue');
//! ```
//!
//! See `docs/ARCHITECTURE.md` for the module layout and `docs/SECURITY.md`
//! for the threat model.

use pgrx::guc::{GucContext, GucFlags, GucRegistry};
use pgrx::prelude::*;

::pgrx::pg_module_magic!();

mod agent;
mod api;
mod infra;
mod providers;
mod schema;
mod session;
mod sql_guard;
mod telemetry;
mod tools;

// `#[pg_extern]` invocations live in `api/*`. The SQL entity-graph the
// macro builds is what the pgrx schema generator walks, so we don't need
// to re-export anything here — declaring the modules is enough.

::pgrx::extension_sql_file!("../sql/bootstrap.sql", name = "bootstrap", bootstrap);

// ---------- GUC registration ----------

/// Called by Postgres at module load. Registers every `pg_ask.*` GUC so
/// users can `SET LOCAL pg_ask.api_key = '…'` from day one.
///
/// `SUPERUSER_ONLY | NO_SHOW_ALL` on `api_key` makes `SHOW ALL` and
/// `pg_settings` redact it for non-superuser roles.
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    use infra::config::*;

    let secret_flags = GucFlags::SUPERUSER_ONLY | GucFlags::NO_SHOW_ALL;

    GucRegistry::define_string_guc(
        c"pg_ask.provider",
        c"Active provider name (e.g. anthropic, openai, gemini)",
        c"Looked up first; falls back to pg_ask._config table.",
        &PROVIDER,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pg_ask.api_key",
        c"API key for the active provider",
        c"Prefer SET LOCAL or ALTER ROLE over the _config table. Redacted in SHOW ALL.",
        &API_KEY,
        GucContext::Userset,
        secret_flags,
    );
    GucRegistry::define_string_guc(
        c"pg_ask.model",
        c"Model identifier passed to the active provider",
        c"Provider-specific default if unset.",
        &MODEL,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pg_ask.base_url",
        c"Override the provider base URL (for OpenAI-compatible endpoints, proxies, etc.)",
        c"",
        &BASE_URL,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_ask.max_tokens",
        c"Max output tokens per provider call",
        c"",
        &MAX_TOKENS,
        1,
        1_048_576,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_ask.max_iterations",
        c"Hard ceiling on agent loop iterations",
        c"",
        &MAX_ITERATIONS,
        1,
        1_024,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_ask.readonly",
        c"Reject non-SELECT statements from the model",
        c"Defaults to on. Disable only after auditing pg_ask._traces.",
        &READONLY,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_ask.http_connect_timeout_ms",
        c"Provider HTTP connect timeout, milliseconds",
        c"",
        &HTTP_CONNECT_TIMEOUT_MS,
        100,
        600_000,
        GucContext::Userset,
        GucFlags::UNIT_MS,
    );
    GucRegistry::define_int_guc(
        c"pg_ask.http_total_timeout_ms",
        c"Provider HTTP total request timeout, milliseconds",
        c"",
        &HTTP_TOTAL_TIMEOUT_MS,
        100,
        600_000,
        GucContext::Userset,
        GucFlags::UNIT_MS,
    );
    GucRegistry::define_int_guc(
        c"pg_ask.tool_statement_timeout_ms",
        c"statement_timeout wrapped around every sql_query tool call",
        c"Applied via SET LOCAL so it auto-resets at end of transaction.",
        &TOOL_STATEMENT_TIMEOUT_MS,
        100,
        3_600_000,
        GucContext::Userset,
        GucFlags::UNIT_MS,
    );
    GucRegistry::define_int_guc(
        c"pg_ask.tool_max_rows",
        c"Maximum rows the sql_query tool feeds back to the model",
        c"",
        &TOOL_MAX_ROWS,
        1,
        1_000_000,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_ask.trace_enabled",
        c"Write a row to pg_ask._traces for every ask() / chat() call (v0.2)",
        c"",
        &TRACE_ENABLED,
        GucContext::Userset,
        GucFlags::default(),
    );

}

// ---------- Tests ----------

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn smoke_extension_loads() {
        assert_eq!(Spi::get_one::<i32>("SELECT 1").unwrap(), Some(1));
    }

    #[pg_test]
    fn version_string_matches_cargo() {
        let v: Option<String> = Spi::get_one("SELECT pg_ask.version()").unwrap();
        assert_eq!(v.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }
}

/// pgrx test harness entry point.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    pub fn postgresql_conf_options() -> Vec<&'static str> {
        // Load the extension at backend start so `_PG_init` runs and our GUCs
        // appear in `pg_settings` for every #[pg_test].
        vec!["shared_preload_libraries='pg_ask'"]
    }
}
