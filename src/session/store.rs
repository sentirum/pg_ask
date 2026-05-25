//! SPI primitives backing `session/mod.rs`.
//!
//! Every read or write is parameterised — no string concatenation with
//! user-supplied values. The `owner` column is treated as authoritative:
//! we never read it from the caller, we always compare against
//! `current_user` inside the SQL itself.
//!
//! All multi-row writes happen inside a single `Spi::connect_mut` scope
//! so a half-appended message list cannot survive a mid-loop error.

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
    // RETURNING id keeps us from having to gen_random_uuid() on the Rust
    // side, which would need an extra dependency.
    let id: Option<Uuid> = if let Some(l) = label {
        Spi::get_one_with_args(
            "INSERT INTO ask._sessions(label) VALUES ($1) RETURNING id",
            &[l.into()],
        )?
    } else {
        Spi::get_one("INSERT INTO ask._sessions DEFAULT VALUES RETURNING id")?
    };
    id.ok_or_else(|| AskError::Sql("INSERT INTO _sessions returned no id".into()))
}

pub fn is_owned_by_current_user(session_id: Uuid) -> Result<bool> {
    let found: Option<bool> = Spi::get_one_with_args(
        "SELECT TRUE
           FROM ask._sessions
          WHERE id = $1 AND owner = current_user",
        &[session_id.into()],
    )?;
    Ok(found.unwrap_or(false))
}

pub fn fetch_messages(session_id: Uuid) -> Result<Vec<MessageRow>> {
    let mut out: Vec<MessageRow> = Vec::new();

    Spi::connect(|client| -> Result<()> {
        let rows = client.select(
            "SELECT role, content, tool_calls::text, tool_call_id, is_error
               FROM ask._messages
              WHERE session_id = $1
              ORDER BY idx",
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
        // Find next idx in a single statement to avoid race-during-append.
        // RETURNING gives us back the row we just wrote so we don't need
        // a second SELECT.
        let next_idx_query =
            "SELECT COALESCE(MAX(idx), -1) + 1 FROM ask._messages WHERE session_id = $1";

        let mut next: i32 = client
            .select(next_idx_query, None, &[session_id.into()])?
            .into_iter()
            .next()
            .and_then(|r| {
                r.get_datum_by_ordinal(1)
                    .ok()
                    .and_then(|d| d.value::<i32>().ok().flatten())
            })
            .unwrap_or(0);

        for msg in messages {
            let (role, content, tool_calls, tool_call_id, is_error) = encode(msg);

            // Build the args slice. We always pass the same 7 positional
            // parameters; null fields go in as Option<&str>::None / etc.
            // pgrx's DatumWithOid blanket From<Option<T>> handles this.
            let tc_owned: Option<String> = tool_calls.map(|v| v.to_string());

            client.update(
                "INSERT INTO ask._messages
                    (session_id, idx, role, content, tool_calls, tool_call_id, is_error)
                 VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7)",
                None,
                &[
                    session_id.into(),
                    next.into(),
                    role.into(),
                    content.into(),
                    tc_owned.as_deref().into(),
                    tool_call_id.as_deref().into(),
                    is_error.into(),
                ],
            )?;
            next += 1;
        }

        // Touch updated_at so listings sort sensibly.
        client.update(
            "UPDATE ask._sessions SET updated_at = now() WHERE id = $1",
            None,
            &[session_id.into()],
        )?;

        Ok(())
    })
}

pub fn clear_messages(session_id: Uuid) -> Result<()> {
    Spi::run_with_args(
        "DELETE FROM ask._messages WHERE session_id = $1",
        &[session_id.into()],
    )?;
    Ok(())
}

// ---------- helpers ----------

fn encode(msg: &Message) -> (&'static str, String, Option<Value>, Option<String>, Option<bool>) {
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
