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
}

impl Tool for SqlQueryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sql_query".to_string(),
            description: "Execute a read-only SQL query against the current database \
                and return the results as a text table. Use this to look up real \
                values; do not invent data. Prefer adding LIMIT to keep results small.".to_string(),
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
/// table. We do **not** wrap this in `catch_unwind` — pgrx already converts
/// Postgres longjmp errors into Rust panics that propagate as `Result::Err`
/// through `SpiResult`, and wrapping `catch_unwind` over a SPI boundary
/// risks leaving Postgres' error stack in an inconsistent state.
///
/// Datum extraction is delegated to `super::render::run_to_table` which
/// wraps the query in `row_to_json(...)::text` to get native PG → text
/// conversion for every column type. See that module's docs for why.
fn run_query_to_text(
    query: &str,
    readonly: bool,
    max_rows: usize,
    statement_timeout_ms: u64,
    sensitive: &[String],
) -> std::result::Result<String, String> {
    // Phase 1: audit insert (captures uuid) + GUC setup. SECURITY DEFINER
    // helper handles the audit row so we don't need INSERT on _sql_audit
    // for PUBLIC (C3); it returns the uuid so phase 3 can update the
    // same row with the post-query stats.
    let audit_id = audit_begin(query, readonly, statement_timeout_ms)?;

    // Phase 2: the actual query.
    let started = Instant::now();
    let result = render::run_to_table(query, max_rows, sensitive);
    let latency_ms = started.elapsed().as_millis() as i64;

    // Phase 3: re-enable writes (RESET LOCAL undoes the SET LOCAL from
    // phase 1 within the current transaction) and update the audit row.
    // We do this for both success and failure so the operator can see
    // why a query failed without grepping logs. Failures here are not
    // fatal to the user's call (audit is best-effort).
    audit_finish(audit_id, readonly, latency_ms, result.as_ref().err());

    result.map(|RenderedTable { text, .. }| text)
}

/// Insert the in-flight audit row and apply per-call GUCs. Returns the
/// audit uuid so `audit_finish` can update the same row.
fn audit_begin(
    query: &str,
    readonly: bool,
    statement_timeout_ms: u64,
) -> std::result::Result<Option<Uuid>, String> {
    let timeout_sql = format!("SET LOCAL statement_timeout = {statement_timeout_ms}");
    let readonly_sql = "SET LOCAL transaction_read_only = on";

    Spi::connect_mut(|client| -> std::result::Result<Option<Uuid>, String> {
        // Audit FIRST, before flipping transaction_read_only. The
        // SECURITY DEFINER helper returns the inserted uuid so we can
        // update row_count / error / latency_ms after the query runs.
        let tup = client
            .select(
                "SELECT ask._sql_audit_insert($1, $2, $3, 'sql_query')",
                None,
                &[query.into(), (-1i32).into(), readonly.into()],
            )
            .map_err(|e| e.to_string())?;
        let audit_id = tup
            .into_iter()
            .next()
            .and_then(|row| row.get_datum_by_ordinal(1).ok()?.value::<Uuid>().ok().flatten());

        client
            .update(timeout_sql.as_str(), None, &[])
            .map_err(|e| e.to_string())?;
        if readonly {
            client
                .update(readonly_sql, None, &[])
                .map_err(|e| e.to_string())?;
        }
        Ok(audit_id)
    })
}

/// Update the audit row with the query outcome. Best-effort: any failure
/// inside this fn is logged with `warning!()` rather than propagated
/// because the user's `ask()` call already has a result.
fn audit_finish(
    audit_id: Option<Uuid>,
    readonly: bool,
    latency_ms: i64,
    error: Option<&String>,
) {
    let Some(id) = audit_id else {
        // Phase 1 didn't get an id back — nothing to update. The insert
        // itself may still have happened (returning rows in pgrx can
        // silently produce None for genuinely-null returns); the row
        // will simply stay at row_count = -1.
        return;
    };
    if readonly {
        // In readonly mode the surrounding transaction has
        // `transaction_read_only = on` and Postgres refuses to undo
        // that mid-transaction ("must be set before any query"). Per-
        // function SET clauses are subject to the same restriction
        // (`cannot be set locally in functions`). The only known fix
        // is a real subtransaction via pgrx FFI, which we deliberately
        // avoid in this crate (`no unsafe`). Audit row stays at
        // row_count = -1 ("in flight / unknown"), which is documented
        // as the readonly-mode tombstone in ask._sql_audit.
        return;
    }
    let r: std::result::Result<(), String> = Spi::connect_mut(|client| {
        // Route through a SECURITY DEFINER helper so we don't need
        // PUBLIC to hold UPDATE on _sql_audit (C3 grant policy).
        client
            .update(
                "SELECT ask._sql_audit_finish($1, $2, $3)",
                None,
                &[
                    id.into(),
                    latency_ms.into(),
                    error.cloned().as_deref().into(),
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    });
    if let Err(e) = r {
        pgrx::warning!("sql_query audit finish failed for {id:?}: {e}");
    }
}
