//! `ask.status()` — the capability handshake.
//!
//! One round-trip, JSON out, no secrets, never raises on a half-configured
//! install. External orchestrators (senti-ai) call this to decide whether
//! the database is `ready`, `needs_config`, or lacks schema access, and to
//! discover the version / capabilities / limits without hard-coding
//! pg_catalog probes against pg_ask internals.
//!
//! All business logic lives in [`crate::infra::status`]; this file is the
//! thin `#[pg_extern]` wrapper, per the `api/` module contract.

use crate::infra::status;
use pgrx::prelude::*;

/// Self-describing capability + configuration document for this install.
///
/// `STABLE` (reads config + catalog, no writes). `parallel_unsafe` because
/// `status::snapshot()` reads via SPI (`has_schema_privilege`, `to_regclass`
/// probes); a parallel-safe function may be invoked inside a parallel worker
/// where SPI is forbidden ("cannot start commands during a parallel
/// operation"). Still safe to `GRANT EXECUTE ... TO PUBLIC`: the document
/// reports `provider_configured` as a boolean and never returns the api_key.
#[pg_extern(schema = "ask", stable, parallel_unsafe)]
fn status() -> pgrx::Json {
    pgrx::Json(status::snapshot())
}

/// Integer contract version of the `ask.status()` document. Lets a caller
/// cheaply gate on shape compatibility before parsing the full JSON.
#[pg_extern(schema = "ask", immutable, parallel_safe)]
fn status_api_level() -> i32 {
    status::API_LEVEL
}
