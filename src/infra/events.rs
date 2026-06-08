//! Event outbox emission (ADR-0017: in-database reverse notifications).
//!
//! `ask.emit(event, payload, summary)` appends a durable row to
//! `ask._outbox` and fires `pg_notify('pg_ask_events', <id>)`. An external
//! orchestrator (any process holding a `LISTEN pg_ask_events` connection)
//! reads the row and routes it onward. The durable table is the source of
//! truth; the NOTIFY is only a low-latency wake-up, so nothing is lost if
//! the listener is offline (it drains the backlog on reconnect).
//!
//! pg_ask itself knows nothing about who consumes these events — it only
//! deposits a row in its own database. Who listens (and therefore which
//! tenant/agent owns the event) is decided entirely on the consumer side;
//! see ADR-0017 for why that makes multi-tenant isolation automatic.
//!
//! ## Single authority: `ask._outbox_emit`
//!
//! The SECURITY DEFINER `ask._outbox_emit` SQL function is the *one* place
//! that decides whether and how an event is written. It re-checks
//! `events_enabled`, validates the input (event-name charset/length, summary
//! length, payload byte ceiling), enforces the rate-limit / dedup guards,
//! writes the durable row, AND fires the NOTIFY — all atomically. That
//! helper is GRANTed to PUBLIC, so a caller could invoke it directly;
//! keeping every rule in SQL means both entry points (`ask.emit` and a
//! direct `ask._outbox_emit` call) enforce exactly the same contract.
//!
//! This Rust layer is a thin adapter. It deliberately does **no** size or
//! charset validation of its own: duplicating those checks here once caused
//! silent drift (Rust counts bytes via `str::len`; SQL `length()` counts
//! characters; `serde_json` emits compact JSON while `jsonb::text` inserts
//! spaces — so the two layers disagreed on multi-byte summaries and on
//! payloads near the byte ceiling). The only thing Rust does is a cheap
//! `events_enabled` short-circuit to avoid an SPI round-trip on the common
//! disabled path; correctness lives entirely in SQL.
//!
//! ## Why suppression never raises
//!
//! `emit` is meant to be called from triggers and scheduled jobs, often on
//! the hot path of an application write. When an emit is suppressed by the
//! rate-limit or dedup window (or because events are disabled) the SQL
//! authority returns NULL and we surface it as `Ok(None)` — a silent no-op.
//! Raising there would roll back the surrounding INSERT/UPDATE that fired
//! the trigger, a cure far worse than a dropped duplicate alert. Only caller
//! bugs (invalid event name, oversized payload/summary) raise, and that
//! `RAISE` originates in `ask._outbox_emit` with a precise SQLSTATE.

use crate::infra::config::EVENTS_ENABLED;
use crate::infra::errors::Result;
use pgrx::prelude::*;
use pgrx::{JsonB, Uuid};

/// NOTIFY channel name. Fixed (not configurable) so listeners have a stable
/// contract; the per-event detail lives in the outbox row, not the channel.
/// The actual NOTIFY is fired inside `ask._outbox_emit` (see the SQL writer);
/// this constant documents the channel-name contract and is asserted by
/// `event_channel_constant_is_stable` so a rename is caught at test time.
#[cfg_attr(not(any(test, feature = "pg_test")), allow(dead_code))]
pub const EVENT_CHANNEL: &str = "pg_ask_events";

/// Append an event to `ask._outbox` and notify listeners.
///
/// Returns `Some(id)` of the new row, or `None` when the emit was a no-op
/// (events disabled, or suppressed by the rate-limit / dedup window). All of
/// that — plus input validation and the NOTIFY — is decided by the SQL
/// authority `ask._outbox_emit`; this function only short-circuits the
/// disabled case and forwards the call.
///
/// Hard errors (`Err`, surfaced from a SQL `RAISE`) are reserved for caller
/// bugs: an invalid event name, or a payload/summary that exceeds the
/// configured size ceilings.
///
/// `payload` defaults to `{}` when `None`. `summary` is an optional
/// human-readable line (e.g. an `ask.ask()` result) kept separate from the
/// JSON payload so a listener can surface it without parsing.
pub fn emit(
    event: &str,
    payload: Option<serde_json::Value>,
    summary: Option<&str>,
) -> Result<Option<Uuid>> {
    // Cheap short-circuit: skip the SPI round-trip on the common disabled
    // path. The SQL authority re-checks this flag, so skipping here is a pure
    // optimization, not the gate.
    if !EVENTS_ENABLED.get() {
        return Ok(None);
    }

    let payload_json = JsonB(payload.unwrap_or_else(|| serde_json::json!({})));

    // The SECURITY DEFINER writer is the single authority: it validates,
    // re-checks the enabled flag, enforces the rate-limit / dedup guards,
    // INSERTs the durable row, and fires pg_notify('pg_ask_events', id) —
    // all atomically. It returns the new id, or NULL when the emit was a
    // no-op (disabled / suppressed). We surface NULL as Ok(None): a silent
    // no-op, never an error, so a trigger's transaction is never aborted.
    let id: Option<Uuid> = Spi::get_one_with_args(
        "SELECT ask._outbox_emit($1, $2, $3)",
        &[event.into(), payload_json.into(), summary.into()],
    )?;

    Ok(id)
}

/// Delete already-delivered outbox rows older than `older_than` (a Postgres
/// interval literal such as `'7 days'`), in batches of `batch_size`.
/// Pending (undelivered) rows are never removed. Returns the number of rows
/// pruned.
///
/// Thin wrapper over the SECURITY DEFINER `ask._outbox_prune`. The interval
/// is passed as text and cast in SQL so we don't have to bind pgrx's
/// `Interval` type here; an invalid literal surfaces as a normal SQL error.
pub fn prune(older_than: &str, batch_size: i32) -> Result<i64> {
    let n: Option<i64> = Spi::get_one_with_args(
        "SELECT ask._outbox_prune($1::interval, $2)",
        &[older_than.into(), batch_size.into()],
    )?;
    Ok(n.unwrap_or(0))
}
