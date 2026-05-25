//! Multi-turn chat surface.
//!
//! ```sql
//! SELECT ask.create_session('weekly analytics');
//! SELECT ask.chat(<id>, 'how many orders shipped last week?');
//! SELECT ask.chat(<id>, 'and the week before?');
//! SELECT ask.clear_session(<id>);
//! ```
//!
//! Ownership is enforced inside `session::*` — both reads and writes go
//! through `WHERE owner = current_user`. We never echo back the existence
//! of someone else's session.

use crate::agent::{self, AgentMode};
use crate::api::trace::with_trace;
use crate::session;
use crate::telemetry::TraceKind;
use pgrx::prelude::*;
use pgrx::Uuid;

/// Create a new conversation. Returns its id.
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn create_session(label: Option<String>) -> Uuid {
    match session::create(label.as_deref()) {
        Ok(id) => id,
        Err(e) => error!("ask.create_session: {e}"),
    }
}

/// Append a user message to an existing session, run the agent with the
/// reconstructed history, persist the new turns, and return the final text.
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn chat(session_id: Uuid, message: &str) -> String {
    let result = with_trace(TraceKind::Chat, message, |cfg, rec| {
        let prior = session::load_history(session_id)?;
        let outcome = agent::run_with_cfg(cfg, message, prior, AgentMode::Execute)?;

        rec.iterations = outcome.iterations;
        rec.tool_calls = outcome.tool_calls.clone();
        rec.final_text = Some(outcome.text.clone());

        // Persist *after* the agent succeeds so a failed turn doesn't leave
        // a half-written tool-result chain.
        session::append_messages(session_id, &outcome.new_turns)?;

        Ok(outcome.text)
    });
    match result {
        Ok(text) => text,
        Err(e) => error!("ask.chat: {e}"),
    }
}

/// Wipe message history for a session (the session row itself stays).
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn clear_session(session_id: Uuid) -> bool {
    if let Err(e) = session::clear(session_id) {
        error!("ask.clear_session: {e}");
    }
    true
}
