//! Telemetry / audit log — best-effort writer for `pg_ask._traces`.
//!
//! v0.1 ships the writer as a no-op stub; the `_traces` table itself
//! lands in v0.2 along with the `SECURITY DEFINER` insert helper. This
//! file exists today so the agent loop can grow its trace-emission code
//! against a stable API surface.

use crate::infra::config::RuntimeConfig;
use crate::infra::errors::Result;

#[allow(dead_code)] // populated by agent::run in v0.2
#[derive(Debug, Default)]
pub struct TraceRecord {
    pub question: String,
    pub iterations: u32,
    pub final_text: Option<String>,
    pub provider: String,
    pub model: Option<String>,
    pub latency_ms: u64,
    pub error: Option<String>,
}

#[allow(dead_code)]
impl TraceRecord {
    pub fn from_config(cfg: &RuntimeConfig, question: &str) -> Self {
        Self {
            question: question.to_string(),
            iterations: 0,
            final_text: None,
            provider: cfg.provider.clone(),
            model: cfg.model.clone(),
            latency_ms: 0,
            error: None,
        }
    }
}

/// Persist a trace row. No-op in v0.1 (table doesn't exist yet); never
/// returns an error to the caller — telemetry must not be able to fail
/// an `ask()` call.
#[allow(dead_code)]
pub fn write(_cfg: &RuntimeConfig, _rec: TraceRecord) -> Result<()> {
    // v0.2: SECURITY DEFINER insert into pg_ask._traces.
    Ok(())
}
