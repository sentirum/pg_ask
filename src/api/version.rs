//! `pg_ask.version()` — the cheapest possible smoke test.
//!
//! IMMUTABLE + parallel-safe so it can be inlined / cached anywhere.

use pgrx::prelude::*;

/// Returns the crate version baked in at compile time.
#[pg_extern(schema = "pg_ask", immutable, parallel_safe)]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
