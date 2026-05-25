//! Run `EXPLAIN (FORMAT JSON, VERBOSE)` against a validated SELECT-shaped
//! statement, inside a real subtransaction.
//!
//! `EXPLAIN` without `ANALYZE` does not execute the underlying query — it
//! only asks the planner — so there is no scan / no write. The readonly
//! sub-transaction is belt-and-braces: even if an EXPLAIN side effect
//! existed in some extension, it would still bounce off the GUC.
//!
//! ## Why a real subtransaction (and not just SET LOCAL)
//!
//! v0.5.2 bug: `SET LOCAL` is scoped to the enclosing *transaction*,
//! not to the `Spi::connect_mut` block. So a previous version of
//! this file flipped `transaction_read_only = on` for the EXPLAIN
//! and that flag survived past the SPI scope into the rest of
//! `ask.preview` — the immediately-following `telemetry::write`
//! INSERT then failed with `25006: cannot execute INSERT in a
//! read-only transaction`. End-to-end reproducer: any successful
//! call of `ask.preview('...')`.
//!
//! Wrapping the whole thing in `run_in_subtransaction` cleanly
//! scopes the `SET LOCAL`s: when the subtxn releases, Postgres pops
//! its GUC stack frame and the parent transaction sees the
//! original `transaction_read_only` / `statement_timeout` values
//! again. See `src/infra/subtxn.rs` for the safety discussion
//! behind the FFI boundary.

use crate::infra::errors::{AskError, Result};
use pgrx::prelude::*;
use serde_json::Value;

/// Returns the first plan in the JSON array Postgres emits (always 1 elem
/// in practice for a single statement). Caller decides how to consume.
pub fn run(sql: &str) -> Result<Value> {
    // EXPLAIN options. JSON for machine parsing; VERBOSE so the output
    // includes `Output` / `Schema` fields used by the analyser.
    let explain_sql = format!("EXPLAIN (FORMAT JSON, VERBOSE) {sql}");

    crate::infra::subtxn::run_in_subtransaction(Some("pg_ask_explain"), move || {
    Spi::connect_mut(|client| -> Result<Value> {
        // Belt-and-braces readonly. SET LOCAL auto-resets at txn end;
        // inside a subtxn that's the subtxn boundary, so the flag
        // doesn't leak back into the parent. See module docs.
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

        // EXPLAIN (FORMAT JSON) returns one row whose only column is
        // typed as Postgres `json` (oid 114), NOT `text`. pgrx is
        // strict about Datum type compatibility, so asking for
        // `String` here errors with "Postgres type json is not
        // compatible with the Rust type alloc::string::String". The
        // matching adapter exported from the prelude is `pgrx::Json`,
        // a newtype around `serde_json::Value` whose FromDatum impl
        // accepts the `json` Datum and deserialises it for us — which
        // saves the round-trip through a text representation we'd
        // otherwise have to re-parse with serde_json by hand.
        let parsed: Value = row
            .get::<pgrx::Json>(1)
            .map_err(|e| AskError::Sql(format!("EXPLAIN read failed: {e}")))?
            .map(|j| j.0)
            .ok_or_else(|| AskError::Sql("EXPLAIN row had no payload".into()))?;

        // Postgres returns an array with one element per statement.
        let first = parsed
            .as_array()
            .and_then(|a| a.first().cloned())
            .ok_or_else(|| AskError::Sql("EXPLAIN JSON not an array".into()))?;

        Ok(first)
    })
    })
}
