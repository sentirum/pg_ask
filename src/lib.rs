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
//! SELECT ask.config('provider', 'anthropic');
//! SELECT ask.config('api_key',  'sk-ant-...');
//!
//! SELECT ask.ask('How many orders shipped last week?');
//! SELECT ask.sql('top 5 customers by revenue');
//! ```
//!
//! See `docs/ARCHITECTURE.md` for the module layout and `docs/SECURITY.md`
//! for the threat model.

// Rust 1.95 / clippy lints that fire across the existing codebase
// without indicating actual bugs. We allow them at the crate root so
// CI's `-D warnings` policy can keep catching genuinely new issues
// without churning every doc comment.
//
// * `doc_overindented_list_items` — stylistic only; the indentation
//   we use lines up rendered prose with the marker.
// * `collapsible_match` / `default_constructed_unit_structs` /
//   `type_complexity` — pre-existing patterns; refactoring them is
//   out of Wave 4's regression-fix scope.
// * `useless_conversion` (`.into_iter()` feeding pgrx's
//   `TableIterator::new(impl IntoIterator<Item=Row>)`) — the
//   explicit form documents intent at the SETOF return sites and
//   the conversion is a no-op anyway.
#![allow(
    clippy::doc_overindented_list_items,
    clippy::collapsible_match,
    clippy::default_constructed_unit_structs,
    clippy::type_complexity,
    clippy::useless_conversion
)]

use pgrx::guc::{GucContext, GucFlags, GucRegistry};
use pgrx::prelude::*;

::pgrx::pg_module_magic!();

mod agent;
mod api;
mod bgworker;
mod embeddings;
mod infra;
mod memory;
mod planner;
mod providers;
mod schema;
mod session;
mod sql_guard;
mod telemetry;
mod tools;

// Declare the schema so pgrx SQL entity-graph generation emits
// CREATE SCHEMA pg_ask before any #[pg_extern(schema = "ask")]
// function definitions.
#[pg_schema]
mod ask {}

// `#[pg_extern]` invocations live in `api/*`. The SQL entity-graph the
// macro builds is what the pgrx schema generator walks, so we don't need
// to re-export anything here — declaring the modules is enough.

::pgrx::extension_sql_file!("../sql/bootstrap.sql", name = "bootstrap", bootstrap);
// Finalize runs AFTER pgrx emits the schema for #[pg_extern] functions,
// so it can reference user-facing entry points like `ask.config` /
// `ask.get_config` by name (C6 lockdown).
::pgrx::extension_sql_file!("../sql/finalize.sql", name = "finalize", finalize);

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
        c"Looked up first; falls back to ask._config table.",
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
        c"Defaults to on. Disable only after auditing ask._traces.",
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
        c"Write a row to ask._traces for every ask() / chat() call",
        c"",
        &TRACE_ENABLED,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_ask.events_enabled",
        c"Enable ask.emit(): append to ask._outbox + pg_notify('pg_ask_events')",
        c"",
        &EVENTS_ENABLED,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_ask.schema_char_budget",
        c"Soft cap on schema dump injected into the system prompt (characters).",
        c"When the full render exceeds this, falls back to a tables-only listing \
          and exposes describe_table for on-demand column detail.",
        &SCHEMA_CHAR_BUDGET,
        512,
        1_000_000,
        GucContext::Userset,
        GucFlags::default(),
    );

    // ---------- Background worker (v0.5) ----------
    bgworker::register();

    // ---------- Memory / embedding (v0.3) ----------
    GucRegistry::define_string_guc(
        c"pg_ask.embedding_provider",
        c"Embedding provider (openai | voyage | gemini). Independent of chat provider.",
        c"",
        &EMBEDDING_PROVIDER,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pg_ask.embedding_api_key",
        c"API key for the embedding provider. Redacted in SHOW ALL.",
        c"Kept separate from pg_ask.api_key so operators can mix providers.",
        &EMBEDDING_API_KEY,
        GucContext::Userset,
        secret_flags,
    );
    GucRegistry::define_string_guc(
        c"pg_ask.embedding_model",
        c"Embedding model identifier (e.g. text-embedding-3-small).",
        c"Provider-specific default if unset.",
        &EMBEDDING_MODEL,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pg_ask.embedding_base_url",
        c"Override the embedding provider base URL (OpenAI-compatible endpoints).",
        c"",
        &EMBEDDING_BASE_URL,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_int_guc(
        c"pg_ask.embedding_dimensions",
        c"Embedding vector dimensions; must match the _memories column width.",
        c"Default 1536 (OpenAI text-embedding-3-small, Gemini text-embedding-004).",
        &EMBEDDING_DIMENSIONS,
        8,
        16_384,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_float_guc(
        c"pg_ask.memory_search_alpha",
        c"Blend weight in [0,1]: cosine vs full-text rank for recall.",
        c"1.0 = pure vector, 0.0 = pure full-text. Default 0.7.",
        &MEMORY_SEARCH_ALPHA,
        0.0,
        1.0,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_ask.memory_enabled",
        c"Master switch for the memory layer (remember / recall / forget).",
        c"Functionally off regardless of this flag if pgvector is not installed.",
        &MEMORY_ENABLED,
        GucContext::Userset,
        GucFlags::default(),
    );

    // ---------- v0.4 — Tooling expansion ----------
    GucRegistry::define_bool_guc(
        c"pg_ask.allow_http",
        c"Enable the http_fetch tool (off by default).",
        c"When false the tool is not registered and any invocation returns an error.",
        &ALLOW_HTTP,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pg_ask.http_allow_list",
        c"Comma-separated allow-list entries for http_fetch.",
        c"Each entry is either a bare host (api.example.com) or a full URL with optional path prefix (https://api.example.com/v1). Empty = deny all.",
        &HTTP_ALLOW_LIST,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_bool_guc(
        c"pg_ask.allow_private_hosts",
        c"Allow http_fetch to call private/loopback IP addresses (off by default).",
        c"Opt-in for self-hosted setups. Without this, literal IPs in 10/8, 172.16/12, 192.168/16, 127/8, 169.254/16, 100.64/10, ::1, fc00::/7, fe80::/10 are rejected even if allow-listed.",
        &ALLOW_PRIVATE_HOSTS,
        GucContext::Userset,
        GucFlags::default(),
    );
    GucRegistry::define_string_guc(
        c"pg_ask.sensitive_columns",
        c"Comma-separated schema.table.column patterns to redact in sql_query output.",
        c"Matched cells are replaced with <redacted>. The column name stays visible.",
        &SENSITIVE_COLUMNS,
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
        let v: Option<String> = Spi::get_one("SELECT ask.version()").unwrap();
        assert_eq!(v.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[pg_test]
    fn status_handshake_reports_shape_without_secrets() {
        // Unconfigured install: must NOT raise, must report not-ready.
        let raw: Option<pgrx::Json> = Spi::get_one("SELECT ask.status()").unwrap();
        let doc = raw.expect("status() returns a row").0;

        assert_eq!(doc["extension"], "pg_ask");
        assert_eq!(doc["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(doc["api_level"], 1);
        // No provider yet → not configured → not ready, health needs_config.
        assert_eq!(doc["provider_configured"], false);
        assert_eq!(doc["ready"], false);
        assert_eq!(doc["health"], "needs_config");
        // Secret never surfaces, in any form.
        let flat = doc.to_string();
        assert!(!flat.contains("api_key"));
        assert!(doc.get("capabilities").unwrap().is_array());

        // After configuring the fixture provider, it flips to ready.
        Spi::run("SET pg_ask.provider = 'fixture'").unwrap();
        let raw2: Option<pgrx::Json> = Spi::get_one("SELECT ask.status()").unwrap();
        let doc2 = raw2.unwrap().0;
        assert_eq!(doc2["provider_configured"], true);
        assert_eq!(doc2["provider"], "fixture");
        assert_eq!(doc2["ready"], true);
    }

    #[pg_test]
    fn status_api_level_matches_constant() {
        let n: Option<i32> = Spi::get_one("SELECT ask.status_api_level()").unwrap();
        assert_eq!(n, Some(1));
    }

    #[pg_test]
    fn emit_is_noop_when_events_disabled() {
        // Default: events_enabled = off -> ask.emit returns NULL and writes
        // nothing to the outbox.
        Spi::run("SET pg_ask.events_enabled = off").unwrap();
        let id: Option<pgrx::Uuid> =
            Spi::get_one("SELECT ask.emit('test.event', '{}'::jsonb)").unwrap();
        assert!(id.is_none(), "emit should return NULL when disabled");
        let n: Option<i64> = Spi::get_one("SELECT count(*) FROM ask._outbox").unwrap();
        assert_eq!(n, Some(0), "no outbox row when disabled");
    }

    #[pg_test]
    fn emit_writes_outbox_row_when_enabled() {
        Spi::run("SET pg_ask.events_enabled = on").unwrap();
        let id: Option<pgrx::Uuid> = Spi::get_one(
            "SELECT ask.emit('inventory.critical', '{\"product_id\": 57}'::jsonb, 'stock low')",
        )
        .unwrap();
        assert!(id.is_some(), "emit returns the new row id when enabled");

        // The durable row is present, pending, with payload + summary intact.
        let event: Option<String> =
            Spi::get_one("SELECT event FROM ask._outbox ORDER BY ts DESC LIMIT 1").unwrap();
        assert_eq!(event.as_deref(), Some("inventory.critical"));
        let summary: Option<String> =
            Spi::get_one("SELECT summary FROM ask._outbox ORDER BY ts DESC LIMIT 1").unwrap();
        assert_eq!(summary.as_deref(), Some("stock low"));
        let pid: Option<i32> = Spi::get_one(
            "SELECT (payload->>'product_id')::int FROM ask._outbox ORDER BY ts DESC LIMIT 1",
        )
        .unwrap();
        assert_eq!(pid, Some(57));
        let pending: Option<bool> =
            Spi::get_one("SELECT processed_at IS NULL FROM ask._outbox ORDER BY ts DESC LIMIT 1")
                .unwrap();
        assert_eq!(pending, Some(true), "new row starts pending");
    }

    #[pg_test]
    fn emit_rejects_empty_event_name() {
        Spi::run("SET pg_ask.events_enabled = on").unwrap();
        let res = std::panic::catch_unwind(|| {
            Spi::get_one::<pgrx::Uuid>("SELECT ask.emit('', '{}'::jsonb)")
        });
        assert!(res.is_err(), "empty event name must raise");
    }

    #[pg_test]
    fn outbox_mark_processed_is_idempotent() {
        Spi::run("SET pg_ask.events_enabled = on").unwrap();
        let id: pgrx::Uuid = Spi::get_one("SELECT ask.emit('x.y', '{}'::jsonb)")
            .unwrap()
            .unwrap();
        // First mark flips pending -> processed and returns true.
        let first: Option<bool> =
            Spi::get_one_with_args("SELECT ask._outbox_mark_processed($1)", &[id.into()]).unwrap();
        assert_eq!(first, Some(true));
        // Second mark is a no-op (already processed) and returns false.
        let second: Option<bool> =
            Spi::get_one_with_args("SELECT ask._outbox_mark_processed($1)", &[id.into()]).unwrap();
        assert_eq!(second, Some(false));
    }

    /// Configure every fixture-driven test the same way: pick the
    /// fixture provider, point at a scenario, and turn telemetry off
    /// because the SECURITY DEFINER writer assumes the extension
    /// owner and pgrx's test bootstrap role isn't that.
    fn use_fixture(scenario: &str) {
        use crate::providers::fixture::reset_cursor;
        reset_cursor(scenario);
        Spi::run("SET pg_ask.provider = 'fixture'").unwrap();
        Spi::run(&format!("SET pg_ask.model = 'fixture:{scenario}'")).unwrap();
        Spi::run("SET pg_ask.trace_enabled = off").unwrap();
    }

    /// End-to-end smoke of the agent loop with zero network traffic.
    ///
    /// Drives `ask.ask(…)` through the fixture provider so the test
    /// exercises: provider dispatch, sql_guard, SPI tool execution,
    /// result feedback, telemetry insert into `ask._traces`. If any of
    /// those wire up the wrong way, this test catches it without ever
    /// touching an upstream API.
    #[pg_test]
    fn agent_loop_runs_against_fixture_provider() {
        use_fixture("smoke_sql_query");

        let answer: Option<String> =
            Spi::get_one("SELECT ask.ask('count the relations in pg_class')").unwrap();

        assert!(
            answer
                .as_deref()
                .map(|s| s.contains("pg_class lists the relation count"))
                .unwrap_or(false),
            "fixture-scripted final answer not echoed back, got: {answer:?}"
        );
    }

    /// H2 (v0.5.2 review) regression: a sql_query that ERRORs at
    /// the Postgres layer (typo, missing column, divide-by-zero,
    /// statement_timeout, ...) must NOT poison the outer ask()
    /// transaction. Before the subtxn wrapper, the next SPI call
    /// in the agent loop would fail with "current transaction is
    /// aborted, commands ignored" and the entire ask() call would
    /// crash. After the fix, the failure is contained: the tool
    /// returns is_error=true to the model, audit row carries the
    /// errmsg, and subsequent statements (including the final
    /// SELECT we make here) keep working.
    ///
    /// We trigger the failure by aiming sql_query at a relation
    /// that doesn't exist. The fixture replays a sql_query call
    /// followed by a final text turn; the assertion verifies the
    /// final turn was actually emitted (it cannot be if the txn
    /// was poisoned by the failed query).
    #[pg_test]
    fn sql_query_failure_does_not_poison_outer_transaction() {
        use_fixture("sql_query_targets_missing_table");

        let answer: Option<String> =
            Spi::get_one("SELECT ask.ask('show me the missing table')").unwrap();

        // The fixture's final turn is "recovered after error";
        // we only get here if the subtxn-isolated failure didn't
        // poison the outer ask() transaction.
        assert!(
            answer
                .as_deref()
                .map(|s| s.to_lowercase().contains("recovered"))
                .unwrap_or(false),
            "agent loop should have recovered from sql_query failure, got: {answer:?}"
        );

        // And the outer transaction must still be usable for
        // arbitrary SQL after ask() returned — prove it by
        // hitting pg_class.
        // pg_class.relname is `name`, not `text`, so cast to keep
        // pgrx's typed Datum extraction happy.
        let relname: Option<String> =
            Spi::get_one("SELECT relname::text FROM pg_class WHERE relname = 'pg_class' LIMIT 1")
                .unwrap();
        assert_eq!(relname.as_deref(), Some("pg_class"));
    }

    /// sql_guard must reject a model-emitted DROP, *through SPI*, not
    /// just in the standalone unit tests. We script the agent to emit
    /// `DROP TABLE pg_class`, then check the audit row records the
    /// attempt and the final answer mentions the refusal — i.e. the
    /// tool result that came back to the agent was an error string,
    /// not a successful execution.
    #[pg_test]
    fn sql_guard_blocks_ddl_through_spi() {
        use_fixture("agent_emits_drop");

        let answer: Option<String> = Spi::get_one("SELECT ask.ask('drop everything')").unwrap();

        // The agent's final turn echoes "blocked by sql_guard"; that
        // string came from the fixture script unconditionally, so this
        // assertion only proves the loop completed. The next two
        // assertions prove the guard actually fired.
        assert!(answer.is_some());

        // pg_class still exists — the most direct proof the DDL was
        // rejected before SPI ever saw it.
        let exists: Option<bool> =
            Spi::get_one("SELECT EXISTS (SELECT 1 FROM pg_class WHERE relname = 'pg_class')")
                .unwrap();
        assert_eq!(exists, Some(true), "pg_class should survive a DROP attempt");

        // The audit table should not contain a row for the rejected
        // statement because sql_query bails before writing the audit
        // row when validate() returns Err. (If we ever decide to log
        // rejected attempts too, flip this assertion.)
        let audited: Option<i64> = Spi::get_one(
            "SELECT count(*) FROM ask._sql_audit WHERE query ILIKE 'DROP TABLE pg_class'",
        )
        .unwrap();
        assert_eq!(audited, Some(0));
    }

    /// Same shape, but the model emits two statements separated by a
    /// semicolon. Even in writable mode the guard rejects this; here
    /// the default readonly mode rejects it twice over.
    #[pg_test]
    fn sql_guard_blocks_multi_statement_through_spi() {
        use_fixture("agent_emits_multi_statement");

        let answer: Option<String> = Spi::get_one("SELECT ask.ask('two queries please')").unwrap();
        assert!(answer.is_some());

        // Nothing should have been audited.
        let audited: Option<i64> =
            Spi::get_one("SELECT count(*) FROM ask._sql_audit WHERE query ILIKE '%SELECT 2%'")
                .unwrap();
        assert_eq!(audited, Some(0));
    }

    /// `ask.sql` is the generate-only path — no tools, no execution.
    /// Fixture returns a single Final containing a SELECT; we check
    /// the string round-trips out of the agent.
    #[pg_test]
    fn ask_sql_returns_fixture_text() {
        use_fixture("sql_only");
        let sql: Option<String> = Spi::get_one("SELECT ask.sql('count pg_class rows')").unwrap();
        assert_eq!(sql.as_deref(), Some("SELECT count(*) FROM pg_class"));
    }

    /// Regression for the v0.5.2 "readonly GUCs leak past the SPI
    /// block" bug found by manual smoke against DeepInfra. The
    /// failure mode was: `ask.ask()` in readonly mode (the default)
    /// errored with `25006 cannot execute INSERT in a read-only
    /// transaction` as soon as `telemetry::write` tried to land the
    /// trace row. Root cause: `sql_query::audit_begin` /
    /// `sample_table::run_sample` / `planner::explain::run` each
    /// issued `SET LOCAL transaction_read_only = on`, which is
    /// scoped to the whole enclosing transaction, not to the
    /// `Spi::connect_mut` block. The fix moves those GUCs inside
    /// the subtxn that already wraps the user query so the flag
    /// auto-reverts on subtxn release.
    ///
    /// We assert the call returns the fixture's scripted final turn
    /// (proves agent loop completed) AND that the trace row
    /// actually landed in `ask._traces` (proves the post-query
    /// INSERT succeeded — the original symptom).
    #[pg_test]
    fn readonly_ask_does_not_leak_transaction_read_only() {
        use_fixture("smoke_sql_query");
        // use_fixture() turns telemetry off to keep most tests fast;
        // this regression specifically needs the trace INSERT to fire
        // so we can prove it succeeds, so turn it back on.
        Spi::run("SET pg_ask.trace_enabled = on").unwrap();

        let traces_before: Option<i64> =
            Spi::get_one("SELECT count(*) FROM ask._traces WHERE kind = 'ask'").unwrap();

        let answer: Option<String> =
            Spi::get_one("SELECT ask.ask('count the relations in pg_class')").unwrap();
        assert!(
            answer.is_some(),
            "ask.ask should have returned a final text"
        );

        let traces_after: Option<i64> =
            Spi::get_one("SELECT count(*) FROM ask._traces WHERE kind = 'ask'").unwrap();
        assert_eq!(
            traces_after.unwrap_or(0),
            traces_before.unwrap_or(0) + 1,
            "telemetry::write INSERT must succeed after the readonly query \
             — if this fails the SET LOCAL transaction_read_only = on \
             from sql_query is leaking back into the parent transaction"
        );

        // Belt-and-braces: a manual INSERT in the same backend
        // after ask.ask() must also work. This catches the bug at
        // the user-visible level (any post-ask SQL in the same
        // session was failing, not just our own telemetry write).
        Spi::run("CREATE TEMP TABLE _post_ask_probe (n int)").unwrap();
        Spi::run("INSERT INTO _post_ask_probe VALUES (1)").unwrap();
        let n: Option<i64> = Spi::get_one("SELECT count(*) FROM _post_ask_probe").unwrap();
        assert_eq!(n, Some(1));
    }

    /// `ask.preview` runs sql_guard + EXPLAIN against the model's SQL
    /// without executing it. We hand it a known-good SELECT and assert
    /// the planner returned at least one row and one referenced table.
    /// This exercises planner::analysis end-to-end (EXPLAIN JSON parse,
    /// est_rows extraction, tables list) inside the real backend.
    #[pg_test]
    fn preview_returns_explain_for_select() {
        use_fixture("sql_only");

        // ask.preview is a SETOF (generated_sql, est_rows, tables, warnings).
        let row = Spi::get_three::<String, i64, Vec<String>>(
            "SELECT generated_sql, est_rows, tables \
             FROM ask.preview('count pg_class rows')",
        )
        .unwrap();

        let (sql, est_rows, tables) = row;
        assert_eq!(sql.as_deref(), Some("SELECT count(*) FROM pg_class"));
        // count(*) plan estimates a single output row.
        assert!(
            est_rows.unwrap_or(-1) >= 1,
            "EXPLAIN should estimate at least one row for count(*), got {est_rows:?}"
        );
        // The referenced table list must include pg_class.
        let tables = tables.unwrap_or_default();
        assert!(
            tables.iter().any(|t| t.ends_with("pg_class")),
            "preview should report pg_class as a referenced table, got {tables:?}"
        );
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
