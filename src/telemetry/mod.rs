//! Telemetry / audit log — best-effort writer for `pg_ask._traces`.
//!
//! One row per public entry-point call (`ask`, `sql`, `preview`, `chat`).
//! The writer is fire-and-forget from the agent's perspective: a failure
//! here MUST NOT fail the user's `ask()`. Hence every error path becomes
//! a `pgrx::warning!` and the call returns `Ok(())`.
//!
//! Wire format: we hand the SQL helper a single `jsonb` payload so the
//! Rust side never has to know the column order — schema changes in
//! `bootstrap.sql` don't ripple here.

use crate::infra::config::{RuntimeConfig, TRACE_ENABLED};
use crate::providers::ToolCall;
use pgrx::prelude::*;
use serde::Serialize;
use serde_json::{json, Value};
use std::time::Instant;

/// What kind of public entry point produced this trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceKind {
    Ask,
    Sql,
    Preview,
    /// Used by `pg_ask.chat()` once sessions land later in v0.2.
    #[allow(dead_code)]
    Chat,
}

impl TraceKind {
    fn as_str(self) -> &'static str {
        match self {
            TraceKind::Ask => "ask",
            TraceKind::Sql => "sql",
            TraceKind::Preview => "preview",
            TraceKind::Chat => "chat",
        }
    }
}

/// One tool-call entry as stored in the `tool_calls` jsonb column.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallTrace {
    pub name: String,
    pub arguments: Value,
    pub is_error: bool,
    pub output_preview: String,
    pub elapsed_ms: u64,
}

impl ToolCallTrace {
    /// Build from a model-issued tool call + the resulting output. Truncates
    /// the output so a runaway query doesn't bloat the audit row.
    pub fn from_call(call: &ToolCall, output: &str, is_error: bool, elapsed_ms: u64) -> Self {
        const PREVIEW_CHARS: usize = 2_000;
        let preview = if output.chars().count() > PREVIEW_CHARS {
            let cut: String = output.chars().take(PREVIEW_CHARS).collect();
            format!("{cut}…")
        } else {
            output.to_string()
        };
        Self {
            name: call.name.clone(),
            arguments: call.arguments.clone(),
            is_error,
            output_preview: preview,
            elapsed_ms,
        }
    }
}

/// Mutable accumulator the agent loop fills as it runs.
#[derive(Debug)]
pub struct TraceRecord {
    pub kind: TraceKind,
    pub question: String,
    pub iterations: u32,
    pub tool_calls: Vec<ToolCallTrace>,
    pub final_text: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub started: Instant,
    pub error: Option<String>,
}

impl TraceRecord {
    pub fn start(kind: TraceKind, cfg: &RuntimeConfig, question: &str) -> Self {
        Self {
            kind,
            question: question.to_string(),
            iterations: 0,
            tool_calls: Vec::new(),
            final_text: None,
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            started: Instant::now(),
            error: None,
        }
    }

    fn to_payload(&self) -> Value {
        json!({
            "kind":         self.kind.as_str(),
            "question":     self.question,
            "iterations":   self.iterations,
            "tool_calls":   self.tool_calls,
            "final_text":   self.final_text,
            "provider":     self.provider,
            "model":        self.model,
            "latency_ms":   self.started.elapsed().as_millis() as i64,
            "error":        self.error,
        })
    }
}

/// Write a trace row, swallowing every error so telemetry can never fail
/// the underlying user call. Honours the `pg_ask.trace_enabled` GUC.
pub fn write(rec: &TraceRecord) {
    if !TRACE_ENABLED.get() {
        return;
    }
    let payload = rec.to_payload();
    let payload_text = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => {
            pgrx::warning!("pg_ask telemetry: failed to serialise trace: {e}");
            return;
        }
    };

    let result = Spi::run_with_args(
        "SELECT pg_ask._write_trace($1::jsonb)",
        &[payload_text.into()],
    );
    if let Err(e) = result {
        pgrx::warning!("pg_ask telemetry: failed to insert trace row: {e}");
    }
}
