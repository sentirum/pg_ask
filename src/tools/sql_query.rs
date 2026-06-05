//! `sql_query` tool — the single way the model gets to touch the database.
//!
//! Layers of defence around the SPI call (top-to-bottom):
//!
//! 1. `sql_guard::validate` rejects writes, multi-statements, banned funcs.
//! 2. `SET LOCAL statement_timeout` bounds each tool invocation.
//! 3. `SET LOCAL transaction_read_only = on` when readonly mode is set —
//!    enforced by Postgres itself, not by our string match.
//! 4. Row + cell caps before the model sees the data.
//!
//! Errors flow back to the model as `is_error = true` tool outputs so it
//! can self-correct (typos, wrong column names, etc.) instead of aborting
//! the entire `ask()` invocation.
//!
//! ## Audit lifecycle (H2 + H3 in the v0.5.2 review)
//!
//! Each call goes through three SPI phases:
//!
//! 1. Pre-query: insert audit row with `row_count = -1` ("in flight"),
//!    capturing the returned uuid. Then `SET LOCAL statement_timeout`
//!    and (if readonly) `SET LOCAL transaction_read_only = on`.
//! 2. Query: delegated to `render::run_to_table` under those GUCs.
//! 3. Post-query: `RESET LOCAL transaction_read_only` to re-enable
//!    writes within the current transaction, then update the audit
//!    row's `row_count` / `error` / `latency_ms` columns. Errors during
//!    update are swallowed (audit must never block the user) but are
//!    logged via pgrx's `warning!()`.
//!
//! Why this works without a SAVEPOINT (the original H2 plan):
//!
//! * `SET LOCAL` keeps GUCs scoped to the *enclosing transaction*, not
//!   to a sub-block. `RESET LOCAL` reverses that scope without needing
//!   subtransaction machinery.
//! * The audit update runs *after* the read-only flag is cleared, so it
//!   doesn't trip the "cannot execute UPDATE in a read-only transaction"
//!   error that motivated the original `row_count = -1` workaround.
//! * Restoring GUCs to their pre-call values is unnecessary because the
//!   pg_ask transaction boundary is the `ask.ask()` / `ask.chat()`
//!   invocation — there are no subsequent user statements that could
//!   observe a leaked read-only flag.

use super::render::{self, RenderedTable};
use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::providers::ToolSpec;
use crate::sql_guard::{self, GuardMode};
use pgrx::prelude::*;
use pgrx::Uuid;
use serde_json::json;
use std::time::Instant;

pub struct SqlQueryTool {
    pub readonly: bool,
    pub max_rows: usize,
    pub statement_timeout_ms: u64,
    pub sensitive_columns: Vec<String>,
    /// Comma-joined `search_path` to pin per query (e.g. `"shop, public"`),
    /// derived from the introspected schema. Lets the model's unqualified
    /// (or `public`-assumed) table references resolve without a round-trip
    /// of catalog discovery. Empty string => leave search_path untouched.
    pub search_path: String,
}

impl Tool for SqlQueryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sql_query".to_string(),
            description: "Execute a read-only SQL query against the current database \
                and return the results as a text table. Use this to look up real \
                values; do not invent data. Prefer adding LIMIT to keep results small."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "A single SQL statement to execute."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput> {
        let raw = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AskError::Tool {
                name: "sql_query".to_string(),
                message: "missing required argument `query`".into(),
            })?;

        let mode = if self.readonly {
            GuardMode::Readonly
        } else {
            GuardMode::Writable
        };

        let validated = match sql_guard::validate(raw, mode) {
            Ok(v) => v,
            Err(e) => return Ok(err(&e.to_string())),
        };

        match run_query_to_text(
            validated.as_str(),
            self.readonly,
            self.max_rows,
            self.statement_timeout_ms,
            &self.sensitive_columns,
            &self.search_path,
        ) {
            Ok(table) => Ok(ok(table)),
            Err(e) => Ok(err(&format!("query failed: {e}"))),
        }
    }
}

fn ok(text: String) -> ToolOutput {
    ToolOutput {
        text,
        is_error: false,
    }
}

fn err(msg: &str) -> ToolOutput {
    ToolOutput {
        text: msg.to_string(),
        is_error: true,
    }
}

/// Execute `query` under timeouts + readonly + row cap, render as a text
/// table.
///
/// ## H2 (v0.5.2 review): subtransaction isolation
///
/// The user query runs inside an internal subtransaction (see
/// `src/infra/subtxn.rs`). If it ERRORs — typo, missing column,
/// permission denied, divide-by-zero, statement_timeout, anything —
/// the subtxn is rolled back and the outer `ask()` transaction stays
/// usable. Before this wrapper, a single failed model-emitted query
/// would poison the rest of the agent loop with "current transaction
/// is aborted, commands ignored" on every subsequent SPI call,
/// including audit_finish and the next tool invocation.
///
/// The subtxn does NOT relax `transaction_read_only` — Postgres
/// rejects that mid-transaction even from inside a subtxn (the flag
/// is transaction-wide, not subtxn-scoped). Readonly enforcement is
/// still done via the parent's `SET LOCAL transaction_read_only =
/// on` set up in `audit_begin`.
///
/// Datum extraction is delegated to `super::render::run_to_table`
/// which wraps the query in `row_to_json(...)::text` to get native
/// PG → text conversion for every column type. See that module's
/// docs for why.
fn run_query_to_text(
    query: &str,
    readonly: bool,
    max_rows: usize,
    statement_timeout_ms: u64,
    sensitive: &[String],
    search_path: &str,
) -> std::result::Result<String, String> {
    // Phase 1: audit insert (captures uuid). Stays in the parent
    // transaction — we WANT the in-flight row visible even if the
    // subtxn below aborts, otherwise readonly-mode failures would
    // leave no audit trace at all.
    let audit_id = audit_insert_only(query, readonly)?;

    // Phase 2: the actual query, wrapped in a subtxn so:
    //  (a) any ERROR stays contained (H2: poisoned-parent fix), and
    //  (b) the per-call `SET LOCAL statement_timeout` /
    //      `SET LOCAL transaction_read_only = on` GUCs are scoped to
    //      the subtxn. Without (b) those `SET LOCAL`s persist for
    //      the rest of the OUTER transaction — every subsequent
    //      INSERT in the same `ask.ask()` call (telemetry::write,
    //      session::record_turn, the next sql_query's audit row,
    //      ...) would fail with "25006 cannot execute INSERT in a
    //      read-only transaction". End-to-end reproducer: any
    //      `SELECT ask.ask('...')` in readonly mode with telemetry
    //      enabled (the default). Same root cause as the EXPLAIN
    //      bug fixed in `src/planner/explain.rs`; both call sites
    //      need the subtxn wrapper for the same reason.
    let started = Instant::now();
    let sensitive_clone: Vec<String> = sensitive.to_vec();
    let query_owned = query.to_string();
    let max_rows_copy = max_rows;
    let timeout_ms = statement_timeout_ms;
    let search_path_owned = search_path.to_string();
    let subtxn_result =
        crate::infra::subtxn::run_in_subtransaction(Some("pg_ask_sql_query"), move || {
            apply_per_call_gucs(readonly, timeout_ms, &search_path_owned)?;
            render::run_to_table(&query_owned, max_rows_copy, &sensitive_clone)
                .map_err(crate::infra::errors::AskError::Sql)
        });
    // Flatten AskError -> String so the rest of the function (audit
    // + caller) keeps the same shape as before.
    let result: std::result::Result<RenderedTable, String> =
        subtxn_result.map_err(|e| e.to_string());
    let latency_ms = started.elapsed().as_millis() as i64;

    // Phase 3: update the audit row with the outcome. Best-effort;
    // failures are logged via `warning!()` but never propagated.
    // Readonly mode skips the update entirely — see audit_finish for
    // the unresolved-limitation note (H3 in the v0.5.2 review).
    audit_finish(audit_id, readonly, latency_ms, result.as_ref().err());

    result.map(|RenderedTable { text, .. }| text)
}

/// Insert the in-flight audit row in the parent transaction and
/// return its uuid so `audit_finish` can update it later. Does NOT
/// touch `transaction_read_only` or `statement_timeout` — those are
/// scoped to the subtxn that runs the user query (see
/// `apply_per_call_gucs` and the call-site comment in
/// `run_query_to_text`).
fn audit_insert_only(query: &str, readonly: bool) -> std::result::Result<Option<Uuid>, String> {
    Spi::connect_mut(|client| -> std::result::Result<Option<Uuid>, String> {
        // SECURITY DEFINER helper so PUBLIC doesn't need INSERT on
        // _sql_audit (C3); returns the uuid via RETURNING.
        let tup = client
            .select(
                "SELECT ask._sql_audit_insert($1, $2, $3, 'sql_query')",
                None,
                &[query.into(), (-1i32).into(), readonly.into()],
            )
            .map_err(|e| e.to_string())?;
        let audit_id = tup.into_iter().next().and_then(|row| {
            row.get_datum_by_ordinal(1)
                .ok()?
                .value::<Uuid>()
                .ok()
                .flatten()
        });
        Ok(audit_id)
    })
}

/// Apply per-call GUCs from INSIDE the query subtransaction. The
/// `SET LOCAL` scope is the enclosing transaction; calling this from
/// inside a subtxn means the GUCs auto-revert when the subtxn
/// releases, instead of leaking into the parent `ask.ask()` call.
fn apply_per_call_gucs(
    readonly: bool,
    statement_timeout_ms: u64,
    search_path: &str,
) -> crate::infra::errors::Result<()> {
    let timeout_sql = format!("SET LOCAL statement_timeout = {statement_timeout_ms}");
    Spi::connect_mut(|client| {
        client
            .update(timeout_sql.as_str(), None, &[])
            .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))?;
        // Pin search_path so the model's queries resolve against the real
        // schemas even when it forgets to qualify or assumes `public`. The
        // value is built from introspected schema names via
        // `schema::search_path_clause`, which quotes each identifier, so it
        // is safe to interpolate here. Empty => skip (don't override the
        // session default).
        if !search_path.is_empty() {
            let sp_sql = format!("SET LOCAL search_path = {search_path}");
            client
                .update(sp_sql.as_str(), None, &[])
                .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))?;
        }
        if readonly {
            client
                .update("SET LOCAL transaction_read_only = on", None, &[])
                .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))?;
        }
        Ok(())
    })
}

/// Update the audit row with the query outcome. Best-effort: any failure
/// inside this fn is logged with `warning!()` rather than propagated
/// because the user's `ask()` call already has a result.
fn audit_finish(audit_id: Option<Uuid>, readonly: bool, latency_ms: i64, error: Option<&String>) {
    let Some(id) = audit_id else {
        // Phase 1 didn't get an id back — nothing to update. The insert
        // itself may still have happened (returning rows in pgrx can
        // silently produce None for genuinely-null returns); the row
        // will simply stay at row_count = -1.
        return;
    };
    // H3 (v0.5.2 review) — unresolved limitation:
    //
    // We tried wrapping this UPDATE in an internal subtransaction so
    // we could `SET LOCAL transaction_read_only = off` inside the
    // subtxn (see src/infra/subtxn.rs for the wrapper, kept for the
    // H2 isolation use case below). It does not work: `XactReadOnly`
    // is a transaction-wide flag that subtransactions inherit, and
    // `check_transaction_read_only` in guc.c specifically rejects
    // flipping it back off inside a subtxn ("cannot set transaction
    // read-write mode inside a read-only transaction",
    // ERRCODE_ACTIVE_SQL_TRANSACTION).
    //
    // The remaining clean options are:
    //   * dblink back to the same DB (extra dependency + network
    //     handshake per audit row — too costly for a hot agent loop),
    //   * a background worker that drains an audit queue
    //     (heavyweight, separate process model),
    //   * an autonomous-transaction extension (not in core).
    //
    // Until one of those lands, readonly-mode audit rows stay at
    // row_count = -1 ("in flight"). This is documented in the
    // ask._sql_audit table comment as the readonly-mode tombstone.
    if readonly {
        return;
    }
    let error_owned = error.cloned();
    let r: std::result::Result<(), String> = Spi::connect_mut(|client| {
        // Route through a SECURITY DEFINER helper so we don't need
        // PUBLIC to hold UPDATE on _sql_audit (C3 grant policy).
        client
            .update(
                "SELECT ask._sql_audit_finish($1, $2, $3)",
                None,
                &[id.into(), latency_ms.into(), error_owned.as_deref().into()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    });
    if let Err(e) = r {
        pgrx::warning!("sql_query audit finish failed for {id:?}: {e}");
    }
}
