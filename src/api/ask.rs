//! `pg_ask.ask` and `pg_ask.sql` — the two v0.1 entry points.
//!
//! Both make network calls and execute SQL; both are explicitly
//! `volatile` + `parallel_unsafe` so the planner doesn't try to be clever.

use crate::agent::{self, AgentMode};
use pgrx::prelude::*;

/// Ask the database a natural-language question. The agent reads the schema,
/// plans SQL, executes it via SPI in the current transaction, and synthesises
/// a textual answer.
#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn ask(question: &str) -> String {
    match agent::run(question, AgentMode::Execute) {
        Ok(outcome) => outcome.text,
        Err(e) => error!("pg_ask.ask: {e}"),
    }
}

/// Generate SQL for a question without executing it. The agent has no tools;
/// it sees only the schema and is asked to return a single SQL statement.
#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn sql(question: &str) -> String {
    match agent::run(question, AgentMode::GenerateOnly) {
        Ok(outcome) => outcome.text,
        Err(e) => error!("pg_ask.sql: {e}"),
    }
}
