//! `pg_ask.ask` and `pg_ask.sql` — the two v0.1 entry points.
//!
//! Both make network calls and execute SQL; both are explicitly
//! `volatile` + `parallel_unsafe` so the planner doesn't try to be clever.
//!
//! Each call is bracketed by [`crate::api::trace::with_trace`] so every
//! invocation lands in `pg_ask._traces` with provider, model, iteration
//! count, tool-call detail, and latency.

use crate::agent::{self, AgentMode};
use crate::api::trace::with_trace;
use crate::telemetry::TraceKind;
use pgrx::prelude::*;

/// Ask the database a natural-language question. The agent reads the schema,
/// plans SQL, executes it via SPI in the current transaction, and synthesises
/// a textual answer.
#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn ask(question: &str) -> String {
    let result = with_trace(TraceKind::Ask, question, |rec| {
        let outcome = agent::run(question, AgentMode::Execute)?;
        rec.iterations = outcome.iterations;
        rec.tool_calls = outcome.tool_calls.clone();
        rec.final_text = Some(outcome.text.clone());
        Ok(outcome.text)
    });
    match result {
        Ok(text) => text,
        Err(e) => error!("pg_ask.ask: {e}"),
    }
}

/// Generate SQL for a question without executing it. The agent has no tools;
/// it sees only the schema and is asked to return a single SQL statement.
#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn sql(question: &str) -> String {
    let result = with_trace(TraceKind::Sql, question, |rec| {
        let outcome = agent::run(question, AgentMode::GenerateOnly)?;
        rec.iterations = outcome.iterations;
        rec.tool_calls = outcome.tool_calls.clone();
        rec.final_text = Some(outcome.text.clone());
        Ok(outcome.text)
    });
    match result {
        Ok(text) => text,
        Err(e) => error!("pg_ask.sql: {e}"),
    }
}
