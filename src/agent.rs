//! Agent loop.
//!
//! Flow: build system prompt (instructions + schema) → seed history with user
//! question → call provider → if it asks for tools, invoke them and feed
//! results back → repeat until provider returns a final text or we hit
//! `max_iterations`.

use crate::config;
use crate::error::{AskError, Result};
use crate::providers::{self, Message, MessageContent, Provider, ProviderResponse, Role, ToolSpec};
use crate::schema;
use crate::tools::{self, Tool};
use pgrx::prelude::*;

const DEFAULT_MAX_ITERATIONS: u32 = 16;

/// Ask the database a natural-language question. The agent reads the schema,
/// plans SQL, executes it via SPI in the current transaction, and synthesises
/// a textual answer.
#[pg_extern(schema = "pg_ask")]
fn ask(question: &str) -> String {
    match run_agent(question, /*execute=*/ true) {
        Ok(answer) => answer,
        Err(e) => error!("pg_ask.ask: {e}"),
    }
}

/// Generate SQL for a question without executing it. The agent has no tools;
/// it sees only the schema and is asked to return a single SQL statement.
#[pg_extern(schema = "pg_ask")]
fn sql(question: &str) -> String {
    match run_agent(question, /*execute=*/ false) {
        Ok(sql) => sql,
        Err(e) => error!("pg_ask.sql: {e}"),
    }
}

fn run_agent(question: &str, execute: bool) -> Result<String> {
    let provider = providers::from_config()?;
    let readonly = config::bool_flag("readonly", true);
    let max_iter = config::optional("max_iterations")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_ITERATIONS);

    let schema_text = schema::summarize()?;
    let system_prompt = build_system_prompt(&schema_text, execute, readonly);

    let tools = if execute {
        tools::default_toolset(readonly)
    } else {
        Vec::new()
    };
    let specs: Vec<ToolSpec> = tools.iter().map(|t| t.spec()).collect();

    let mut history: Vec<Message> = vec![Message {
        role: Role::User,
        content: MessageContent::Text(question.to_string()),
    }];

    for iteration in 0..max_iter {
        // Cooperative cancellation — lets `pg_cancel_backend` interrupt long loops.
        check_for_interrupts!();

        let resp = provider.complete(&system_prompt, &history, &specs)?;
        match resp {
            ProviderResponse::Final { text } => return Ok(text),

            ProviderResponse::ToolCalls { text, calls } => {
                // Record the assistant turn (text + the tool_use blocks).
                history.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::AssistantWithTools {
                        text,
                        tool_calls: calls.clone(),
                    },
                });

                if calls.is_empty() {
                    // Provider stopped without text or tool calls; treat as final-empty.
                    return Err(AskError::EmptyResponse);
                }

                for call in calls {
                    let output = dispatch_tool(&tools, &call.name, &call.arguments);
                    history.push(Message {
                        role: Role::Tool,
                        content: MessageContent::ToolResult {
                            tool_call_id: call.id,
                            output: output.text,
                            is_error: output.is_error,
                        },
                    });
                }
                let _ = iteration; // currently unused; kept for future tracing
            }
        }
    }

    Err(AskError::MaxIterations { max: max_iter })
}

fn dispatch_tool(
    tools: &[Box<dyn Tool>],
    name: &str,
    args: &serde_json::Value,
) -> crate::tools::ToolOutput {
    match tools.iter().find(|t| t.spec().name == name) {
        Some(t) => match t.invoke(args) {
            Ok(o) => o,
            Err(e) => crate::tools::ToolOutput {
                text: format!("tool error: {e}"),
                is_error: true,
            },
        },
        None => crate::tools::ToolOutput {
            text: format!("unknown tool `{name}`"),
            is_error: true,
        },
    }
}

fn build_system_prompt(schema_text: &str, execute: bool, readonly: bool) -> String {
    let mut s = String::new();
    s.push_str(
        "You are pg_ask, an AI agent embedded inside a PostgreSQL database.\n\
         You answer the user's question by reasoning over the schema below and, \
         when needed, by calling tools to inspect real data.\n\n",
    );

    if execute {
        s.push_str(
            "You may call the `sql_query` tool to execute SQL against this database. \
             Prefer small, targeted queries. Always add LIMIT when exploring. \
             Never invent column or table names — they must exist in the schema. \
             When you have enough information, reply with a concise natural-language \
             answer (no SQL fences, no JSON).\n",
        );
        if readonly {
            s.push_str(
                "READONLY MODE is enabled: only SELECT/WITH/EXPLAIN statements are permitted.\n",
            );
        }
    } else {
        s.push_str(
            "You have NO tools. Reply with a single SQL statement that answers \
             the question. Output only the SQL, no prose, no fences.\n",
        );
    }

    s.push_str("\n=== DATABASE SCHEMA ===\n");
    s.push_str(schema_text);
    s
}
