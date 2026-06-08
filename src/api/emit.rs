//! `ask.emit()` — deposit an event for external listeners (ADR-0017).
//!
//! Thin `#[pg_extern]` wrapper over [`crate::infra::events::emit`]. Appends
//! a durable row to `ask._outbox` and fires `pg_notify('pg_ask_events', id)`
//! so any process holding a `LISTEN pg_ask_events` connection can react.
//! No-op (returns NULL) unless `pg_ask.events_enabled = on`, and also a
//! no-op when an emit is suppressed by the optional rate-limit / dedup
//! guards (see [`crate::infra::events`]).
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
/// id, or NULL when the emit was a no-op (events disabled, or suppressed by
/// the rate-limit / dedup window). Raises only on caller bugs: an invalid
/// event name, or a payload/summary over the configured size ceilings.
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

/// Prune already-delivered outbox rows older than `older_than` (a Postgres
/// interval literal, e.g. `'7 days'`), in batches of `batch_size` (default
/// 10000; pass `0` for a single unbounded DELETE). Pending (undelivered)
/// rows are never touched. Returns the number of rows removed.
///
/// Maintenance helper for operators — the outbox is otherwise append-only
/// and grows unbounded as events are processed. Batching keeps the first
/// prune of a long-neglected outbox from running as one giant transaction
/// (huge WAL, long locks, replication stall). Not granted to PUBLIC by
/// default (see finalize.sql); grant it to a maintenance role explicitly.
///
/// ```sql
/// SELECT ask.prune_events('30 days');          -- default batch size
/// SELECT ask.prune_events('30 days', 5000);    -- custom batch size
/// ```
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn prune_events(older_than: &str, batch_size: default!(i32, 10000)) -> i64 {
    match events::prune(older_than, batch_size) {
        Ok(n) => n,
        Err(e) => raise_as_pg_error(&e),
    }
}
