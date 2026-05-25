//! Multi-turn session storage.
//!
//! A session is just a `uuid` + an `owner` + an ordered list of messages.
//! Persistence lives in `ask._sessions` / `ask._messages` (see
//! `sql/bootstrap.sql`). This module exposes a small set of safe operations;
//! the SQL surface in `api::chat` consumes them.
//!
//! Ownership rule, enforced on **every** read or mutation: a session is
//! only visible to `current_user`. We do not distinguish "you do not own
//! this session" from "no such session" — both surface as
//! [`SessionError::NotFound`] so an attacker cannot probe id space for
//! existence.

mod store;

use crate::infra::errors::{AskError, Result};
use crate::providers::{Message, MessageContent, Role};
use pgrx::Uuid;

#[allow(unused_imports)] // re-exported for API-layer convenience
pub use store::SessionId;

/// Public-facing error variants for session ops. Wrapped into [`AskError::Sql`]
/// at the API boundary to keep the rest of the codebase on one error type.
#[derive(Debug)]
pub enum SessionError {
    NotFound,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::NotFound => {
                f.write_str("no such session for current_user")
            }
        }
    }
}

impl From<SessionError> for AskError {
    fn from(e: SessionError) -> Self {
        AskError::Sql(e.to_string())
    }
}

/// Create a session owned by `current_user`. Returns its id.
pub fn create(label: Option<&str>) -> Result<Uuid> {
    store::insert_session(label)
}

/// Verify the session exists *and* is owned by current_user. Same surface
/// for both cases — see module docs.
pub fn assert_owned(session_id: Uuid) -> Result<()> {
    if store::is_owned_by_current_user(session_id)? {
        Ok(())
    } else {
        Err(SessionError::NotFound.into())
    }
}

/// Reconstruct the conversation as the agent expects it. Ownership is
/// re-checked here so callers cannot bypass via raw SPI.
pub fn load_history(session_id: Uuid) -> Result<Vec<Message>> {
    assert_owned(session_id)?;
    let rows = store::fetch_messages(session_id)?;

    let mut history: Vec<Message> = Vec::with_capacity(rows.len());
    for row in rows {
        let msg = match row.role.as_str() {
            "user" => Message {
                role: Role::User,
                content: MessageContent::Text(row.content),
            },
            "assistant" => {
                if let Some(calls_json) = row.tool_calls {
                    let tool_calls: Vec<crate::providers::ToolCall> =
                        serde_json::from_value(calls_json).unwrap_or_default();
                    Message {
                        role: Role::Assistant,
                        content: MessageContent::AssistantWithTools {
                            text: Some(row.content).filter(|s| !s.is_empty()),
                            tool_calls,
                        },
                    }
                } else {
                    Message {
                        role: Role::Assistant,
                        content: MessageContent::Text(row.content),
                    }
                }
            }
            "tool" => Message {
                role: Role::Tool,
                content: MessageContent::ToolResult {
                    tool_call_id: row.tool_call_id.unwrap_or_default(),
                    output: row.content,
                    is_error: row.is_error.unwrap_or(false),
                },
            },
            // "system" rows are not currently persisted; ignore defensively.
            _ => continue,
        };
        history.push(msg);
    }
    Ok(history)
}

/// Append a slice of new messages to the end of a session in one transaction
/// so partial chats never land. Ownership is re-checked.
pub fn append_messages(session_id: Uuid, messages: &[Message]) -> Result<()> {
    assert_owned(session_id)?;
    store::append(session_id, messages)
}

/// Drop every message from a session but keep the session row itself.
pub fn clear(session_id: Uuid) -> Result<()> {
    assert_owned(session_id)?;
    store::clear_messages(session_id)
}
