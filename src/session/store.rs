//! SPI primitives backing `session/mod.rs`.
//!
//! Wave 4 / C2-bis (Gemini v0.5.2 review): every read and write goes
//! through a SECURITY DEFINER helper in `ask._session_*`. The previous
//! implementation issued direct `INSERT INTO ask._sessions / _messages`
//! through SPI, which started failing in v0.5.2 once
//! `REVOKE ALL ON ask._sessions FROM PUBLIC` was added — non-superuser
//! callers got `permission denied for table _sessions` on the very
//! first `ask.create_session(...)` call.
//!
//! Each helper enforces session_user ownership inside its body, so the
//! Rust side can call them without first proving caller identity. We
//! still re-check ownership at the Rust layer (`session::assert_owned`)
//! so the higher-level API surfaces the same `SessionError::NotFound`
//! regardless of which read path tripped.
//!
//! Every read or write is parameterised — no string concatenation with
//! user-supplied values.

use crate::infra::errors::{AskError, Result};
use crate::providers::{Message, MessageContent, Role};
use pgrx::prelude::*;
use pgrx::Uuid;
use serde_json::Value;

/// Re-exported for the public API layer. Aliased for self-documentation;
/// `pgrx::Uuid` is what hits the wire.
#[allow(dead_code)]
pub type SessionId = Uuid;

#[derive(Debug, Clone)]
pub struct MessageRow {
    pub role: String,
    pub content: String,
    pub tool_calls: Option<Value>,
    pub tool_call_id: Option<String>,
    pub is_error: Option<bool>,
}

pub fn insert_session(label: Option<&str>) -> Result<Uuid> {
    // The helper accepts NULL for an unlabeled session — pgrx maps
    // Option::<&str>::None to a NULL text datum.
    let id: Option<Uuid> =
        Spi::get_one_with_args("SELECT ask._session_create($1)", &[label.into()])?;
    id.ok_or_else(|| AskError::Sql("ask._session_create returned no id".into()))
}

pub fn is_owned_by_current_user(session_id: Uuid) -> Result<bool> {
    let found: Option<bool> =
        Spi::get_one_with_args("SELECT ask._session_is_owned($1)", &[session_id.into()])?;
    Ok(found.unwrap_or(false))
}

pub fn fetch_messages(session_id: Uuid) -> Result<Vec<MessageRow>> {
    let mut out: Vec<MessageRow> = Vec::new();

    Spi::connect(|client| -> Result<()> {
        // The helper joins to `_sessions` and filters by session_user
        // owner, so a caller who doesn't own this session simply sees
        // an empty result — matching the "NotFound == Unauthorized"
        // contract the higher-level session::assert_owned relies on.
        let rows = client.select(
            "SELECT role, content, tool_calls, tool_call_id, is_error
               FROM ask._session_fetch_messages($1)",
            None,
            &[session_id.into()],
        )?;

        for row in rows {
            let role: String = row
                .get_datum_by_ordinal(1)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let content: String = row
                .get_datum_by_ordinal(2)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let tool_calls = row
                .get_datum_by_ordinal(3)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .and_then(|s| serde_json::from_str::<Value>(&s).ok());
            let tool_call_id = row
                .get_datum_by_ordinal(4)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten());
            let is_error = row
                .get_datum_by_ordinal(5)
                .ok()
                .and_then(|d| d.value::<bool>().ok().flatten());

            out.push(MessageRow {
                role,
                content,
                tool_calls,
                tool_call_id,
                is_error,
            });
        }
        Ok(())
    })?;
    Ok(out)
}

pub fn append(session_id: Uuid, messages: &[Message]) -> Result<()> {
    if messages.is_empty() {
        return Ok(());
    }

    Spi::connect_mut(|client| -> Result<()> {
        // C7 (v0.5.2 review): take a session-scoped transactional
        // advisory lock BEFORE any read, so two concurrent ask.chat()
        // calls against the same session can't both compute the same
        // next idx and trip the (session_id, idx) primary key. The
        // lock is released at end of (sub)transaction.
        //
        // The append helper *also* derives the next idx atomically
        // inside its INSERT-SELECT, so even without the lock each
        // individual write is consistent against its own snapshot.
        // The lock is belt-and-braces for the loop-of-inserts case
        // where two appenders could otherwise interleave.
        client.update(
            "SELECT ask._session_lock_for_append($1)",
            None,
            &[session_id.into()],
        )?;

        for msg in messages {
            let (role, content, tool_calls, tool_call_id, is_error) = encode(msg);
            // The helper takes the jsonb tool_calls as text and casts
            // inside its body (NULLIF empty string → NULL jsonb), so
            // we never have to thread a jsonb datum through pgrx for
            // the empty-call case.
            let tc_text: Option<String> = tool_calls.map(|v| v.to_string());

            client.update(
                "SELECT ask._session_append_message($1, $2, $3, $4, $5, $6)",
                None,
                &[
                    session_id.into(),
                    role.into(),
                    content.into(),
                    tc_text.as_deref().into(),
                    tool_call_id.as_deref().into(),
                    is_error.into(),
                ],
            )?;
        }

        // Touch updated_at so listings sort sensibly. Pulled out of
        // the append loop so listing performance doesn't pay for
        // every message in a multi-message turn.
        client.update("SELECT ask._session_touch($1)", None, &[session_id.into()])?;

        Ok(())
    })
}

pub fn clear_messages(session_id: Uuid) -> Result<()> {
    // The helper filters on `s.owner = session_user` inside its USING
    // join, so a non-owner who guesses an id simply deletes nothing —
    // same observable outcome as a real miss, matching the
    // NotFound == Unauthorized convention.
    Spi::run_with_args(
        "SELECT ask._session_clear_messages($1)",
        &[session_id.into()],
    )?;
    Ok(())
}

// ---------- helpers ----------

fn encode(
    msg: &Message,
) -> (
    &'static str,
    String,
    Option<Value>,
    Option<String>,
    Option<bool>,
) {
    match (&msg.role, &msg.content) {
        (Role::User, MessageContent::Text(t)) => ("user", t.clone(), None, None, None),
        (Role::Assistant, MessageContent::Text(t)) => ("assistant", t.clone(), None, None, None),
        (Role::Assistant, MessageContent::AssistantWithTools { text, tool_calls }) => {
            let calls = serde_json::to_value(tool_calls).unwrap_or(Value::Null);
            (
                "assistant",
                text.clone().unwrap_or_default(),
                Some(calls),
                None,
                None,
            )
        }
        (
            Role::Tool,
            MessageContent::ToolResult {
                tool_call_id,
                output,
                is_error,
            },
        ) => (
            "tool",
            output.clone(),
            None,
            Some(tool_call_id.clone()),
            Some(*is_error),
        ),
        // System messages aren't persisted; the system prompt is rebuilt
        // every turn from schema+config. If one reaches here, persist it as
        // assistant text rather than dropping silently.
        (Role::System, MessageContent::Text(t)) => ("assistant", t.clone(), None, None, None),
        _ => ("assistant", String::new(), None, None, None),
    }
}
