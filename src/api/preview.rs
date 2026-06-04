//! `ask.preview(question)` — generate, explain, do not execute.
//!
//! Returns a single-row table-valued function so the result is composable in
//! SQL (`SELECT * FROM ask.preview('...')`, `\gx`, joins against
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
use crate::api::trace::with_trace;
use crate::infra::errors::raise_as_pg_error;
use crate::planner::{self, PreviewRow};
use crate::telemetry::TraceKind;
use pgrx::prelude::*;

#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
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
    let result = with_trace(TraceKind::Preview, question, |cfg, rec| {
        let outcome = agent::run_with_cfg(cfg, question, Vec::new(), AgentMode::GenerateOnly)?;
        rec.iterations = outcome.iterations;
        rec.tool_calls = outcome.tool_calls.clone();
        if outcome.prompt_tokens > 0 || outcome.completion_tokens > 0 {
            rec.prompt_tokens = Some(outcome.prompt_tokens);
            rec.completion_tokens = Some(outcome.completion_tokens);
        }
        let row: PreviewRow = planner::preview(&outcome.text)?;
        // Record the cleaned SQL we actually previewed (post-EXPLAIN-strip),
        // not the raw model output — that's what operators want to see
        // when auditing.
        rec.final_text = Some(row.generated_sql.clone());
        Ok(row)
    });

    let row = match result {
        Ok(r) => r,
        Err(e) => raise_as_pg_error(&e),
    };

    TableIterator::once((row.generated_sql, row.est_rows, row.tables, row.warnings))
}
