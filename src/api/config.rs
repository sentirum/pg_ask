//! SQL-callable configuration surface.
//!
//! Writes go into the `ask._config` table fallback. For session-scoped
//! or role-scoped configuration use the GUCs directly:
//!
//! ```sql
//! SET LOCAL pg_ask.api_key = 'sk-ant-...';                -- this txn only
//! ALTER ROLE app_reader SET pg_ask.api_key = 'sk-ant-...'; -- across sessions
//! ```
//!
//! See `docs/SECURITY.md` for the secrets-handling recommendations.

use crate::infra::config;
use pgrx::prelude::*;

/// Persist a config key/value pair in `ask._config`.
///
/// Marked `SECURITY DEFINER` so the table can be locked down for non-owner
/// roles; the function validates the key against an allow-list so this
/// doesn't become a generic write primitive.
#[pg_extern(schema = "ask", security_definer, volatile, parallel_unsafe)]
fn config(key: &str, value: &str) -> bool {
    if let Err(e) = config::upsert_table(key, value) {
        error!("ask.config: {e}");
    }
    true
}

/// Read the table-fallback config value for `key`, or NULL.
///
/// Does **not** consult the GUC layer; for GUC values use
/// `SHOW pg_ask.<key>` from a superuser session.
#[pg_extern(schema = "ask", stable, parallel_restricted)]
fn get_config(key: &str) -> Option<String> {
    match config::read_table(key) {
        Ok(v) => v,
        Err(e) => error!("ask.get_config: {e}"),
    }
}
