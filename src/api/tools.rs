//! SQL surface for user-defined tools.
//!
//! ```sql
//! SELECT ask.register_tool(
//!   'active_users',
//!   '{"description":"List active users","input_schema":{"type":"object","properties":{"min_age":{"type":"integer"}}}}'::jsonb,
//!   'SELECT * FROM users WHERE age >= {{min_age}}'
//! );
//!
//! SELECT ask.unregister_tool('active_users');
//! SELECT * FROM ask.list_tools();
//! ```

use crate::infra::errors::AskError;
use pgrx::prelude::*;

/// Register a new user-defined tool. The body may contain `{{key}}`
/// placeholders that are replaced from jsonb arguments at invocation time.
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn register_tool(name: &str, spec: pgrx::Json, body: &str) -> bool {
    if name.is_empty() || body.is_empty() {
        error!("ask.register_tool: name and body are required");
    }
    match do_register(name, spec.0, body) {
        Ok(_) => true,
        Err(e) => error!("ask.register_tool: {e}"),
    }
}

fn do_register(name: &str, spec: serde_json::Value, body: &str) -> Result<(), AskError> {
    let spec_text = spec.to_string();
    // Route through the SECURITY DEFINER helper (C3): _tools no longer
    // accepts direct INSERT/UPDATE from PUBLIC, and the helper enforces
    // that an existing row with a different owner cannot be overwritten
    // (raises insufficient_privilege instead of silently stealing it).
    pgrx::Spi::run_with_args(
        "SELECT ask._tool_register($1, $2::jsonb, $3)",
        &[name.into(), spec_text.as_str().into(), body.into()],
    )?;
    Ok(())
}

/// Remove a user-defined tool. Ownership-checked — an attempt to delete
/// someone else's tool returns `false` (collapsed with "not found").
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn unregister_tool(name: &str) -> bool {
    match do_unregister(name) {
        Ok(b) => b,
        Err(e) => error!("ask.unregister_tool: {e}"),
    }
}

fn do_unregister(name: &str) -> Result<bool, AskError> {
    // Helper-routed for C3; semantics unchanged — ownership check
    // collapses with "not found" so we don't leak existence.
    let deleted: Option<bool> =
        Spi::get_one_with_args("SELECT ask._tool_unregister($1)", &[name.into()])?;
    Ok(deleted.unwrap_or(false))
}

/// List user-defined tools for the current role.
#[pg_extern(schema = "ask", stable, parallel_safe)]
fn list_tools() -> TableIterator<
    'static,
    (name!(name, String), name!(spec, pgrx::Json)),
> {
    let rows = match do_list() {
        Ok(r) => r,
        Err(e) => error!("ask.list_tools: {e}"),
    };
    let materialised: Vec<_> = rows
        .into_iter()
        .map(|(n, s)| (n, pgrx::Json(s)))
        .collect();
    TableIterator::new(materialised.into_iter())
}

fn do_list() -> Result<Vec<(String, serde_json::Value)>, AskError> {
    let mut out: Vec<(String, serde_json::Value)> = Vec::new();
    Spi::connect(|client| -> Result<(), AskError> {
        let rows = client.select(
            "SELECT name, spec::text FROM ask._tools WHERE owner = current_user",
            None,
            &[],
        )?;
        for row in rows {
            let name: String = row
                .get_datum_by_ordinal(1)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let spec_text: String = row
                .get_datum_by_ordinal(2)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            if name.is_empty() {
                continue;
            }
            let spec: serde_json::Value =
                serde_json::from_str(&spec_text).unwrap_or(serde_json::json!({}));
            out.push((name, spec));
        }
        Ok(())
    })?;
    Ok(out)
}
