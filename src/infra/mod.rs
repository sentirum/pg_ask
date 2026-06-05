//! Infrastructure layer: cross-cutting primitives every other module depends on.
//!
//! Contains nothing Postgres-specific in its public API beyond the SPI helpers,
//! and never reaches up into agent / provider / tool logic.

pub mod config;
pub mod errors;
pub mod http;
pub mod spi;
pub mod status;
// `subtxn` is the single module allowed to use raw `pgrx_pg_sys` FFI.
// See its module-level docs for the policy exemption rationale and
// the invariant list every change must preserve.
pub mod subtxn;
