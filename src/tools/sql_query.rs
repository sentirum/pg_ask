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

use super::render::{self, RenderedTable};
use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::providers::ToolSpec;
use crate::sql_guard::{self, GuardMode};
use pgrx::prelude::*;
use serde_json::json;

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
    let timeout_sql = format!("SET LOCAL statement_timeout = {statement_timeout_ms}");
    let readonly_sql = "SET LOCAL transaction_read_only = on";

    // We need TWO SPI sessions back-to-back: the first writes the audit
    // row and sets the GUCs, the second executes the (potentially
    // readonly) user query via the shared `render` helper. Splitting
    // them keeps the helper unaware of audit/GUC concerns and lets us
    // share it with `sample_table` / `user_defined` cleanly.
    Spi::connect_mut(|client| -> std::result::Result<(), String> {
        // Audit FIRST, before we flip transaction_read_only. The audit
        // row carries row_count = -1 ("in flight / unknown"); without
        // this ordering, every sql_query call in readonly mode would
        // ERROR with "cannot execute INSERT in a read-only transaction"
        // on its own audit insert, which then poisons the outer
        // transaction. See H2 / H3 in the review for the planned
        // subtransaction-based fix that will let us update row_count
        // after the query runs.
        // Audit via SECURITY DEFINER helper (C3) so PUBLIC doesn't need
        // INSERT on ask._sql_audit. The helper stamps session_user as
        // the caller, which is what we want even from inside ask.ask().
        let _ = client.update(
            "SELECT ask._sql_audit_insert($1, $2, $3, 'sql_query')",
            None,
            &[query.into(), (-1i32).into(), readonly.into()],
        );

        // GUC scope: SET LOCAL is rolled back at end of the outer
        // transaction. Inside the same transaction these stay in effect
        // for every subsequent statement — see H2 in the review for the
        // planned savepoint-based isolation.
        client
            .update(timeout_sql.as_str(), None, &[])
            .map_err(|e| e.to_string())?;
        if readonly {
            client
                .update(readonly_sql, None, &[])
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })?;

    let RenderedTable { text, .. } = render::run_to_table(query, max_rows, sensitive)?;
    Ok(text)
}
