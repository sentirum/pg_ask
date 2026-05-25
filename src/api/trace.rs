//! Small helper used by every public entry point to bracket the call with
//! telemetry.
//!
//! Pattern at the call site:
//!
//! ```ignore
//! use crate::api::trace::with_trace;
//! use crate::telemetry::TraceKind;
//!
//! let outcome = with_trace(TraceKind::Ask, question, |rec| {
//!     let cfg = ...;  // already loaded by agent::run today
//!     let out = agent::run(question, mode)?;
//!     rec.iterations = out.iterations;
//!     rec.tool_calls = out.tool_calls.clone();
//!     rec.final_text = Some(out.text.clone());
//!     Ok(out.text)
//! });
//! ```
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
    body: impl FnOnce(&mut TraceRecord) -> Result<T>,
) -> Result<T> {
    // The provider/model fields on the trace come from the runtime config.
    // We load it once here so the row reflects what the agent actually saw —
    // even if the closure fails before agent::run gets that far, the row
    // still records which provider was *supposed* to be called.
    let cfg = RuntimeConfig::load();
    let mut rec = match &cfg {
        Ok(c) => TraceRecord::start(kind, c, question),
        Err(_) => TraceRecord::start(
            kind,
            // Synthetic placeholder when config itself failed to load; we
            // still want the trace row so operators can see misconfiguration.
            &RuntimeConfig {
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
            },
            question,
        ),
    };

    // If config loading itself failed, surface that as the trace error and
    // bail. Most callers will recover via the table-fallback _config so this
    // path is rare.
    if let Err(e) = cfg {
        rec.error = Some(e.to_string());
        telemetry::write(&rec);
        return Err(e);
    }

    let outcome = body(&mut rec);
    if let Err(e) = &outcome {
        rec.error = Some(e.to_string());
    }
    telemetry::write(&rec);
    outcome
}
