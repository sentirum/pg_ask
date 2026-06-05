//! Public SQL surface.
//!
//! Every `#[pg_extern]` lives here. Each function is a thin wrapper that:
//!
//! 1. forwards arguments to a single call into another layer, and
//! 2. converts `Result<_, AskError>` into a Postgres `ERROR` via
//!    [`pgrx::error!`].
//!
//! No business logic. No SQL strings. No HTTP. No prompt building. If you
//! find yourself reaching for `serde_json` here, you are in the wrong file.

pub mod ask;
pub mod chat;
pub mod config;
pub mod memory;
pub mod preview;
pub mod status;
pub mod tools;
pub mod trace;
pub mod version;
