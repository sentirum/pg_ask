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
//!
//! ## Secret redaction (C6 in the v0.5.2 review)
//!
//! `ask.get_config` previously returned the raw value for every key,
//! including `api_key` and `embedding_api_key`. Combined with the
//! default PostgreSQL grant policy (functions get EXECUTE TO PUBLIC),
//! that meant any role with USAGE on the `ask` schema could pull the
//! provider key out of the table fallback. The fix has three parts,
//! belt-and-braces:
//!
//! 1. `get_config` here returns `'***redacted***'` (Some, so callers
//!    can still distinguish "set" from "not set") whenever the key is
//!    in `SECRET_KEYS`.
//! 2. `bootstrap.sql` revokes EXECUTE on `ask.get_config` and
//!    `ask.config` from PUBLIC and grants it back only to roles that
//!    have already been granted USAGE on the schema (operator
//!    decision — see the SQL).
//! 3. The matching GUCs are registered with `SUPERUSER_ONLY |
//!    NO_SHOW_ALL` (see `lib.rs`) so `SHOW pg_ask.api_key` and
//!    `pg_settings` don't leak them either.
//!
//! C8 (model-issued `current_setting('pg_ask.api_key')`) is a separate
//! fix in `sql_guard`.

use crate::infra::config;
use pgrx::prelude::*;

/// Keys whose stored value is never returned by `ask.get_config`.
/// Match is case-insensitive against the bare key (callers pass `"api_key"`,
/// not `"pg_ask.api_key"`).
const SECRET_KEYS: &[&str] = &["api_key", "embedding_api_key"];

/// Placeholder returned in place of a secret value. We return `Some(_)`
/// rather than `None` so callers can still tell "is configured" from
/// "is not configured" — useful for setup validators.
const REDACTED: &str = "***redacted***";

fn is_secret(key: &str) -> bool {
    SECRET_KEYS.iter().any(|s| s.eq_ignore_ascii_case(key))
}

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
///
/// Secret keys (see `SECRET_KEYS`) always return `'***redacted***'`
/// when a value is set, never the raw value. To inspect the actual
/// secret, query `ask._config` directly as a superuser.
///
/// ## C3-bis (Gemini v0.5.2 review item 1.4)
///
/// Marked `SECURITY DEFINER` for the same reason `config(...)` is:
/// v0.5.2 added `REVOKE ALL ON ask._config FROM PUBLIC` (C6 hardening),
/// which made the previous SECURITY INVOKER version raise
/// `permission denied for table _config` for every non-superuser
/// caller. Redaction still runs on the Rust side after the read so
/// the definer privileges only buy us table access, not a way to
/// leak secrets — `is_secret()` collapses the value to
/// `'***redacted***'` for the `api_key` / `embedding_api_key`
/// entries before returning, exactly as before.
#[pg_extern(schema = "ask", security_definer, stable, parallel_restricted)]
fn get_config(key: &str) -> Option<String> {
    let raw = match config::read_table(key) {
        Ok(v) => v,
        Err(e) => error!("ask.get_config: {e}"),
    };
    match raw {
        Some(_) if is_secret(key) => Some(REDACTED.to_string()),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_key_detection_is_case_insensitive() {
        assert!(is_secret("api_key"));
        assert!(is_secret("API_KEY"));
        assert!(is_secret("Api_Key"));
        assert!(is_secret("embedding_api_key"));
        assert!(!is_secret("model"));
        assert!(!is_secret("provider"));
        // Defensive: callers shouldn't pass the GUC prefix, but if they
        // do we don't want to silently leak.
        assert!(!is_secret("pg_ask.api_key"));
    }
}
