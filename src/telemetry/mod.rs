//! Telemetry / audit log — best-effort writer for `ask._traces`.
//!
//! One row per public entry-point call (`ask`, `sql`, `preview`, `chat`).
//! The writer is fire-and-forget from the agent's perspective: a failure
//! here MUST NOT fail the user's `ask()`. Hence every error path becomes
//! a `pgrx::warning!` and the call returns `Ok(())`.
//!
//! Wire format: we hand the SQL helper a single `jsonb` payload so the
//! Rust side never has to know the column order — schema changes in
//! `bootstrap.sql` don't ripple here.

use crate::infra::config::RuntimeConfig;
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
    /// Used by `ask.chat()` once sessions land later in v0.2.
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

/// Cap on the number of characters of tool output we ever propagate
/// out of the agent loop, either into the persisted trace row or
/// into the streaming surface (`ask.ask_stream`). Sized so a
/// 100-row `sql_query` table fits without truncation while a
/// pathological 500k-row dump cannot blow up a backend's reply
/// buffer. Shared between [`ToolCallTrace::from_call`] and
/// [`truncate_tool_output`] so both surfaces are bounded the same
/// way — see v0.5.2 review item #11.
pub const TOOL_OUTPUT_PREVIEW_CHARS: usize = 2_000;

/// Truncate `output` to [`TOOL_OUTPUT_PREVIEW_CHARS`] characters,
/// appending an ellipsis when a cut happens. Char-boundary safe
/// (we iterate over `chars()`, not bytes, so the cut never lands
/// in the middle of a UTF-8 code point).
pub fn truncate_tool_output(output: &str) -> String {
    if output.chars().count() > TOOL_OUTPUT_PREVIEW_CHARS {
        let cut: String = output.chars().take(TOOL_OUTPUT_PREVIEW_CHARS).collect();
        format!("{cut}…")
    } else {
        output.to_string()
    }
}

impl ToolCallTrace {
    /// Build from a model-issued tool call + the resulting output. Truncates
    /// the output so a runaway query doesn't bloat the audit row.
    pub fn from_call(call: &ToolCall, output: &str, is_error: bool, elapsed_ms: u64) -> Self {
        let preview = truncate_tool_output(output);
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
///
/// P5 (v0.5.2 review): `trace_enabled` is snapshotted from the
/// `RuntimeConfig` at `start()` time and consulted at `write()` time
/// from the record itself — not from the live GUC. This keeps the
/// row consistent with the rest of the call: with the previous code,
/// a `SET LOCAL pg_ask.trace_enabled = off` issued by a tool mid-call
/// would suppress the trace even though everything else in the call
/// used the original snapshot, and a `SET ... = on` could conjure a
/// row for a call where every other component thought tracing was
/// off. Tying it to the cfg snapshot makes the on/off decision
/// transactional with the rest of the runtime view.
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
    /// Snapshot of `cfg.trace_enabled` taken when `with_trace` loaded
    /// the runtime config. See type-level comment for rationale.
    trace_enabled: bool,
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
            trace_enabled: cfg.trace_enabled,
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
/// the underlying user call. Honours the `pg_ask.trace_enabled` value
/// captured at `TraceRecord::start` time (P5, v0.5.2 review).
pub fn write(rec: &TraceRecord) {
    if !rec.trace_enabled {
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

    let result = Spi::run_with_args("SELECT ask._write_trace($1::jsonb)", &[payload_text.into()]);
    if let Err(e) = result {
        pgrx::warning!("pg_ask telemetry: failed to insert trace row: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::{truncate_tool_output, TOOL_OUTPUT_PREVIEW_CHARS};

    /// Short input passes through unchanged — no ellipsis, no
    /// allocation overhead beyond a single `to_string`. Catches
    /// off-by-one regressions on the boundary check.
    #[test]
    fn short_output_unchanged() {
        let s = "hello world";
        assert_eq!(truncate_tool_output(s), s);
    }

    /// Long input is cut exactly at the boundary and the ellipsis
    /// is appended. Char-count (not byte-count) is the unit so
    /// multibyte input doesn't double-trim. The streaming surface
    /// relies on this bound; see agent/stream.rs.
    #[test]
    fn long_output_truncated_with_ellipsis() {
        let s: String = "x".repeat(TOOL_OUTPUT_PREVIEW_CHARS + 500);
        let out = truncate_tool_output(&s);
        assert_eq!(out.chars().count(), TOOL_OUTPUT_PREVIEW_CHARS + 1);
        assert!(out.ends_with('…'));
    }

    /// UTF-8 safety: a string padded to over the cap with a
    /// multibyte glyph at the cut boundary must not panic and must
    /// not return invalid UTF-8. Reproduces the byte-vs-char
    /// confusion that would crop up if the helper switched to
    /// `&s[..N]` slicing.
    #[test]
    fn truncation_respects_char_boundaries() {
        let s: String = "ç".repeat(TOOL_OUTPUT_PREVIEW_CHARS + 10);
        let out = truncate_tool_output(&s);
        // The cut must happen between `ç` chars, never inside one.
        assert!(out.starts_with('ç'));
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), TOOL_OUTPUT_PREVIEW_CHARS + 1);
    }
}
