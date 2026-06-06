//! Capability handshake for external orchestrators (e.g. senti-ai).
//!
//! `ask.status()` is the single self-describing entry point an outside
//! agent calls to learn, in one round-trip, whether this database can
//! answer questions and how it is configured — *without* ever leaking a
//! secret and *without* raising when the extension is only half-set-up.
//!
//! Unlike [`crate::infra::config::RuntimeConfig::load`], this never errors
//! on a missing `provider` / `api_key`: a not-yet-configured install must
//! still be able to report `ready = false` so the caller can guide the
//! operator through setup.
//!
//! ## API level
//!
//! [`API_LEVEL`] is an integer contract version, bumped only when the
//! *shape* of the status document changes in a backward-incompatible way.
//! Adding a new field is backward-compatible and does NOT bump it; an
//! external consumer keys off `api_level` to decide how to parse.

use crate::infra::config::{
    self, API_KEY, MAX_ITERATIONS, MODEL, PROVIDER, READONLY, TOOL_MAX_ROWS,
};
use pgrx::guc::GucSetting;
use serde_json::{json, Value};
use std::ffi::CString;

/// Status-document contract version. Bump only on breaking shape changes.
pub const API_LEVEL: i32 = 1;

/// Read a string GUC, falling back to the `ask._config` table. Returns
/// `None` for unset or empty. Never raises (table read errors collapse to
/// `None` so a locked-down `_config` can't break the handshake).
fn read_string(key: &str, guc: &GucSetting<Option<CString>>) -> Option<String> {
    guc.get()
        .and_then(|c| c.into_string().ok())
        .filter(|s| !s.is_empty())
        .or_else(|| config::read_table(key).ok().flatten())
        .filter(|s| !s.is_empty())
}

/// Does the current role have USAGE on the `ask` schema? Drives the
/// `forbidden` state on the caller side. Defaults to `true` if the probe
/// itself fails (we'd rather attempt and surface a real error than
/// falsely claim "no access").
fn can_use_schema() -> bool {
    pgrx::Spi::get_one::<bool>("SELECT has_schema_privilege(current_user, 'ask', 'USAGE')")
        .ok()
        .flatten()
        .unwrap_or(true)
}

/// Is the optional pgvector-backed memory layer actually usable right now?
/// Requires both the `vector` extension and the `ask._memories` table.
fn memory_available() -> bool {
    crate::memory::store::pgvector_installed().unwrap_or(false)
        && pgrx::Spi::get_one::<bool>("SELECT to_regclass('ask._memories') IS NOT NULL")
            .ok()
            .flatten()
            .unwrap_or(false)
}

/// Build the `ask.status()` JSON document.
pub fn snapshot() -> Value {
    let provider = read_string("provider", &PROVIDER);
    let api_key_set = read_string("api_key", &API_KEY).is_some();
    let model = read_string("model", &MODEL);

    // `fixture` is the offline test provider and needs no key.
    let is_fixture = provider
        .as_deref()
        .map(|p| p.trim().eq_ignore_ascii_case("fixture"))
        .unwrap_or(false);
    let provider_configured = provider.is_some() && (api_key_set || is_fixture);

    let can_use = can_use_schema();
    let mem = memory_available();

    let mut capabilities = vec!["ask", "sql", "chat", "preview", "register_tool"];
    if mem {
        capabilities.push("memory");
    }

    let health = if !provider_configured {
        "needs_config"
    } else {
        "ok"
    };

    json!({
        "extension": "pg_ask",
        "version": env!("CARGO_PKG_VERSION"),
        "api_level": API_LEVEL,
        "ready": provider_configured && can_use,
        "can_use": can_use,
        "provider_configured": provider_configured,
        "provider": provider,
        "model": model,
        "readonly": READONLY.get(),
        "memory_available": mem,
        "capabilities": capabilities,
        "limits": {
            "max_iterations": MAX_ITERATIONS.get(),
            "tool_max_rows": TOOL_MAX_ROWS.get(),
        },
        "health": health,
    })
}
