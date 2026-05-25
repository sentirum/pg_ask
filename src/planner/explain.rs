//! Run `EXPLAIN (FORMAT JSON, VERBOSE)` against a validated SELECT-shaped
//! statement, inside a readonly sub-transaction.
//!
//! `EXPLAIN` without `ANALYZE` does not execute the underlying query — it
//! only asks the planner — so there is no scan / no write. The readonly
//! sub-transaction is belt-and-braces: even if an EXPLAIN side effect
//! existed in some extension, it would still bounce off the GUC.

use crate::infra::errors::{AskError, Result};
use pgrx::prelude::*;
use serde_json::Value;

/// Returns the first plan in the JSON array Postgres emits (always 1 elem
/// in practice for a single statement). Caller decides how to consume.
pub fn run(sql: &str) -> Result<Value> {
    // EXPLAIN options. JSON for machine parsing; VERBOSE so the output
    // includes `Output` / `Schema` fields used by the analyser.
    let explain_sql = format!("EXPLAIN (FORMAT JSON, VERBOSE) {sql}");

    Spi::connect_mut(|client| -> Result<Value> {
        // Belt-and-braces readonly. SET LOCAL auto-resets at txn end.
        client.update("SET LOCAL transaction_read_only = on", None, &[])?;
        // Tight planner timeout. Even EXPLAIN can be slow on a pathological
        // statement with many partitions / inheritance children.
        client.update("SET LOCAL statement_timeout = 5000", None, &[])?;

        // The planner output column is named "QUERY PLAN" by convention; we
        // ignore the name and pull ordinal 1.
        let table = client.select(&explain_sql, None, &[])?;
        let mut iter = table.into_iter();
        let row = iter
            .next()
            .ok_or_else(|| AskError::Sql("EXPLAIN returned no rows".into()))?;

        // The cell value is JSON serialised as text — both `json` and `jsonb`
        // output paths come back as String through SPI when we ask for it
        // that way. Keep it simple.
        let text: String = row
            .get_datum_by_ordinal(1)
            .ok()
            .and_then(|d| d.value::<String>().ok().flatten())
            .ok_or_else(|| AskError::Sql("EXPLAIN row had no payload".into()))?;

        let parsed: Value = serde_json::from_str(&text)
            .map_err(|e| AskError::Sql(format!("EXPLAIN JSON parse failed: {e}")))?;

        // Postgres returns an array with one element per statement.
        let first = parsed
            .as_array()
            .and_then(|a| a.first().cloned())
            .ok_or_else(|| AskError::Sql("EXPLAIN JSON not an array".into()))?;

        Ok(first)
    })
}
