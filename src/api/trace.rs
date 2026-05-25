//! Small helper used by every public entry point to bracket the call with
//! telemetry.
//!
//! Pattern at the call site:
//!
//! ```ignore
//! use crate::api::trace::with_trace;
//! use crate::telemetry::TraceKind;
//!
//! let outcome = with_trace(TraceKind::Ask, question, |cfg, rec| {
//!     let out = agent::run_with_cfg(cfg, question, mode)?;
//!     rec.iterations = out.iterations;
//!     rec.tool_calls = out.tool_calls.clone();
//!     rec.final_text = Some(out.text.clone());
//!     Ok(out.text)
//! });
//! ```
//!
//! P1 (v0.5.2 review): the closure receives `&RuntimeConfig` so the
//! agent/tool/memory layers downstream don't have to call
//! `RuntimeConfig::load` again. Each public entry used to load the
//! snapshot 2–3 times (`with_trace` + `agent::run` + the first memory
//! tool call); now it's exactly once per `ask()` invocation.
//!
//! Errors flowing out of the closure are recorded in `rec.error` before the
//! row is written, then re-raised to the caller so the `#[pg_extern]`
//! wrapper can still `error!()`.

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::Result;
use crate::telemetry::{self, TraceKind, TraceRecord};

pub fn with_trace<T>(
    kind: TraceKind,
    question: &str,
    body: impl FnOnce(&RuntimeConfig, &mut TraceRecord) -> Result<T>,
) -> Result<T> {
    // Load the runtime snapshot exactly once per call. The closure
    // receives a reference so downstream layers (agent, tools, memory)
    // can use the same view without re-reading GUCs / the _config
    // table. See P1 in the v0.5.2 review.
    let cfg = match RuntimeConfig::load() {
        Ok(c) => c,
        Err(e) => {
            // Config itself failed to load — still record the trace row
            // so operators can see the misconfiguration in ask._traces.
            // We synthesize a placeholder snapshot just for the row's
            // provider/model columns; nothing else uses it.
            let placeholder = unconfigured_placeholder();
            let mut rec = TraceRecord::start(kind, &placeholder, question);
            rec.error = Some(e.to_string());
            telemetry::write(&rec);
            return Err(e);
        }
    };

    let mut rec = TraceRecord::start(kind, &cfg, question);
    let outcome = body(&cfg, &mut rec);
    if let Err(e) = &outcome {
        rec.error = Some(e.to_string());
    }
    telemetry::write(&rec);
    outcome
}

/// Synthetic snapshot used only when `RuntimeConfig::load` itself fails
/// — we still want a trace row so operators can see misconfiguration.
fn unconfigured_placeholder() -> RuntimeConfig {
    RuntimeConfig {
        provider: "<unconfigured>".into(),
        api_key: String::new(),
        model: None,
        base_url: None,
        max_tokens: 0,
        max_iterations: 0,
        readonly: true,
        http_connect_timeout_ms: 0,
        http_total_timeout_ms: 0,
        tool_statement_timeout_ms: 0,
        tool_max_rows: 0,
        trace_enabled: true,
        schema_char_budget: 0,
        embedding_provider: None,
        embedding_api_key: None,
        embedding_model: None,
        embedding_base_url: None,
        embedding_dimensions: 0,
        memory_search_alpha: 0.0,
        memory_enabled: false,
        allow_http: false,
        http_allow_list: Vec::new(),
        allow_private_hosts: false,
        sensitive_columns: Vec::new(),
    }
}
