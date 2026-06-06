//! Event outbox emission (ADR-0017: pg_ask -> senti reverse notifications).
//!
//! `ask.emit(event, payload, summary)` appends a durable row to
//! `ask._outbox` and fires `pg_notify('pg_ask_events', <id>)`. An external
//! orchestrator (senti) LISTENs on that channel, reads the row, and routes
//! it to the owning agent. The durable table is the source of truth; the
//! NOTIFY is only a low-latency wake-up, so nothing is lost if the listener
//! is offline (it drains the backlog on reconnect).
//!
//! pg_ask itself knows nothing about senti — it only deposits an event in
//! its own database. Who listens (and therefore which tenant/agent owns the
//! event) is decided entirely on the consumer side; see the ADR for why
//! that makes multi-tenant isolation automatic.

use crate::infra::config::EVENTS_ENABLED;
use crate::infra::errors::{AskError, Result};
use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};

/// NOTIFY channel name. Fixed (not configurable) so listeners have a stable
/// contract; the per-event detail lives in the outbox row, not the channel.
pub const EVENT_CHANNEL: &str = "pg_ask_events";

/// Append an event to `ask._outbox` and notify listeners.
///
/// Returns `Some(id)` of the new row, or `None` when the events layer is
/// disabled (`pg_ask.events_enabled = off`, the default) — a no-op so an
/// install that doesn't use reverse notifications pays nothing.
///
/// `payload` defaults to `{}` when `None`. `summary` is an optional
/// human-readable line (e.g. an `ask.ask()` result) kept separate from the
/// JSON payload so a listener can surface it without parsing.
pub fn emit(
    event: &str,
    payload: Option<serde_json::Value>,
    summary: Option<&str>,
) -> Result<Option<Uuid>> {
    if !EVENTS_ENABLED.get() {
        return Ok(None);
    }
    if event.trim().is_empty() {
        return Err(AskError::InvalidConfig {
            key: "emit",
            message: "event name must not be empty".to_string(),
        });
    }

    let payload_json = JsonB(payload.unwrap_or_else(|| serde_json::json!({})));

    // Phase 1: durable append via the SECURITY DEFINER writer, capturing the
    // id. Routed through the helper (not a direct INSERT) for the same
    // reason as _sql_audit: PUBLIC has no INSERT on ask._outbox.
    let id: Option<Uuid> = Spi::get_one_with_args(
        "SELECT ask._outbox_emit($1, $2, $3)",
        &[event.into(), payload_json.into(), summary.into()],
    )?;
    let id = id.ok_or_else(|| AskError::Sql("ask._outbox_emit returned no id".to_string()))?;

    // Phase 2: low-latency wake-up. pg_notify's payload is capped at 8 KB,
    // so we send ONLY the id — the listener reads the full row from the
    // outbox. This is the correct pattern regardless of payload size.
    Spi::run_with_args(
        "SELECT pg_notify($1, $2)",
        &[EVENT_CHANNEL.into(), id.to_string().into()],
    )?;

    Ok(Some(id))
}
