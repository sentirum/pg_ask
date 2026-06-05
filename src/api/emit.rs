//! `ask.emit()` — deposit an event for external listeners (ADR-0017).
//!
//! Thin `#[pg_extern]` wrapper over [`crate::infra::events::emit`]. Appends
//! a durable row to `ask._outbox` and fires `pg_notify('pg_ask_events', id)`
//! so an orchestrator (senti) can react. No-op (returns NULL) unless
//! `pg_ask.events_enabled = on`.
//!
//! Intended to be called from triggers or scheduled jobs that have already
//! decided a condition is worth reporting. Keep the *threshold* logic in
//! SQL; use `summary := ask.ask('...')` only when you want a human-readable
//! line, since each ask() call is an LLM round-trip.

use crate::infra::errors::raise_as_pg_error;
use crate::infra::events;
use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};

/// Emit an event to the outbox and notify listeners. Returns the new row's
/// id, or NULL when the events layer is disabled.
///
/// ```sql
/// SELECT ask.emit('inventory.critical',
///                 '{"product_id": 57, "stock": 3}'::jsonb,
///                 ask.ask('Why is product 57 critical right now?'));
///
/// -- Cheap, LLM-free signal:
/// SELECT ask.emit('disk.warning', '{"usage": 91}'::jsonb);
/// ```
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn emit(
    event: &str,
    payload: default!(Option<JsonB>, "NULL"),
    summary: default!(Option<String>, "NULL"),
) -> Option<Uuid> {
    let payload_value = payload.map(|j| j.0);
    match events::emit(event, payload_value, summary.as_deref()) {
        Ok(id) => id,
        Err(e) => raise_as_pg_error(&e),
    }
}
