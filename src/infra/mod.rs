//! Infrastructure layer: cross-cutting primitives every other module depends on.
//!
//! Contains nothing Postgres-specific in its public API beyond the SPI helpers,
//! and never reaches up into agent / provider / tool logic.

pub mod config;
pub mod errors;
pub mod http;
pub mod spi;
