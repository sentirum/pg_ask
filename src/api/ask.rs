//! `ask.ask` and `ask.sql` — the two v0.1 entry points.
//!
//! Both make network calls and execute SQL; both are explicitly
//! `volatile` + `parallel_unsafe` so the planner doesn't try to be clever.
//!
//! Each call is bracketed by [`crate::api::trace::with_trace`] so every
//! invocation lands in `ask._traces` with provider, model, iteration
//! count, tool-call detail, and latency.

use crate::agent::{self, AgentMode};
use crate::api::trace::with_trace;
use crate::infra::errors::raise_as_pg_error;
use crate::telemetry::TraceKind;
use pgrx::prelude::*;

/// Streaming version of `ask.ask`. Returns a set-of-text where each
/// row is one event in the agent loop (thinking, tool result, final answer).
/// The caller can `FETCH 1` repeatedly to see progress instead of waiting
/// for the entire loop to finish.
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn ask_stream(question: &str) -> SetOfIterator<'static, String> {
    let result = with_trace(TraceKind::Ask, question, |cfg, rec| {
        let lines = agent::run_stream_with_cfg(cfg, question, AgentMode::Execute)?;
        // Record the final line (if present) as the trace answer.
        if let Some(last) = lines.last() {
            rec.final_text = Some(last.clone());
        }
        Ok(lines)
    });
    match result {
        Ok(lines) => SetOfIterator::new(lines.into_iter()),
        Err(e) => SetOfIterator::once(format!("[error] {e}")),
    }
    // Note: streaming surface returns error as text rather than raising
    // a PG ERROR so the client sees partial output.
}

/// Ask the database a natural-language question. The agent reads the schema,
/// plans SQL, executes it via SPI in the current transaction, and synthesises
/// a textual answer.
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn ask(question: &str) -> String {
    let result = with_trace(TraceKind::Ask, question, |cfg, rec| {
        let outcome = agent::run_with_cfg(cfg, question, Vec::new(), AgentMode::Execute)?;
        rec.iterations = outcome.iterations;
        rec.tool_calls = outcome.tool_calls.clone();
        rec.final_text = Some(outcome.text.clone());
        // P4 fix: propagate token usage to trace record.
        if outcome.prompt_tokens > 0 || outcome.completion_tokens > 0 {
            rec.prompt_tokens = Some(outcome.prompt_tokens);
            rec.completion_tokens = Some(outcome.completion_tokens);
        }
        Ok(outcome.text)
    });
    match result {
        Ok(text) => text,
        Err(e) => raise_as_pg_error(&e),
    }
}

/// Generate SQL for a question without executing it. The agent has no tools;
/// it sees only the schema and is asked to return a single SQL statement.
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn sql(question: &str) -> String {
    let result = with_trace(TraceKind::Sql, question, |cfg, rec| {
        let outcome = agent::run_with_cfg(cfg, question, Vec::new(), AgentMode::GenerateOnly)?;
        rec.iterations = outcome.iterations;
        rec.tool_calls = outcome.tool_calls.clone();
        rec.final_text = Some(outcome.text.clone());
        if outcome.prompt_tokens > 0 || outcome.completion_tokens > 0 {
            rec.prompt_tokens = Some(outcome.prompt_tokens);
            rec.completion_tokens = Some(outcome.completion_tokens);
        }
        Ok(outcome.text)
    });
    match result {
        Ok(text) => text,
        Err(e) => raise_as_pg_error(&e),
    }
}
