//! `pg_ask.preview(question)` — generate, explain, do not execute.
//!
//! Returns a single-row table-valued function so the result is composable in
//! SQL (`SELECT * FROM pg_ask.preview('...')`, `\gx`, joins against
//! `unnest(tables)`, etc.) without having to JSON-decode anything on the
//! client side.
//!
//! Flow:
//! 1. Ask the model for SQL (no tools, deterministic prompt) via
//!    `agent::run(_, GenerateOnly)`.
//! 2. Strip any leading EXPLAIN the model emitted.
//! 3. Validate against the standard readonly guard.
//! 4. `EXPLAIN (FORMAT JSON, VERBOSE)` it inside a readonly sub-tx.
//! 5. Distil to (generated_sql, est_rows, tables[], warnings[]).
//!
//! The underlying query is never executed.

use crate::agent::{self, AgentMode};
use crate::planner;
use pgrx::prelude::*;

#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn preview(
    question: &str,
) -> TableIterator<
    'static,
    (
        name!(generated_sql, String),
        name!(est_rows, i64),
        name!(tables, Vec<String>),
        name!(warnings, Vec<String>),
    ),
> {
    let outcome = match agent::run(question, AgentMode::GenerateOnly) {
        Ok(o) => o,
        Err(e) => error!("pg_ask.preview: {e}"),
    };

    let row = match planner::preview(&outcome.text) {
        Ok(r) => r,
        Err(e) => error!("pg_ask.preview: {e}"),
    };

    TableIterator::once((row.generated_sql, row.est_rows, row.tables, row.warnings))
}
