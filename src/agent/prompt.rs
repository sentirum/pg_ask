//! System-prompt builder.
//!
//! Kept deliberately small. Operator-visible behaviour (which sentences the
//! model sees) lives here so prompt tuning is one file diff away.
//!
//! The schema text is whatever `schema::summarize_within` produced. When it
//! starts with the compact-mode marker we add one extra sentence so the
//! model knows it can pull column detail via the `describe_table` tool.

use super::run::AgentMode;

const COMPACT_MARKER: &str = "TABLES (use describe_table for columns):";

pub fn build(schema_text: &str, mode: AgentMode, readonly: bool) -> String {
    let mut s = String::new();
    s.push_str(
        "You are pg_ask, an AI agent embedded inside a PostgreSQL database.\n\
         You answer the user's question by reasoning over the schema below and, \
         when needed, by calling tools to inspect real data.\n\n",
    );

    match mode {
        AgentMode::Execute => {
            s.push_str(
                "You may call the `sql_query` tool to execute SQL against this \
                 database. Prefer small, targeted queries. Always add LIMIT when \
                 exploring. Never invent column or table names — they must exist \
                 in the schema. When you have enough information, reply with a \
                 concise natural-language answer (no SQL fences, no JSON).\n",
            );
            if readonly {
                s.push_str(
                    "READONLY MODE is enabled: only SELECT/WITH/EXPLAIN/TABLE \
                     statements are permitted. Writes will be rejected.\n",
                );
            }
        }
        AgentMode::GenerateOnly => {
            s.push_str(
                "You have NO tools. Reply with a single SQL statement that \
                 answers the question. Output only the SQL, no prose, no fences.\n",
            );
        }
    }

    if schema_text.starts_with(COMPACT_MARKER) && matches!(mode, AgentMode::Execute) {
        s.push_str(
            "\nNOTE: The schema below is a tables-only listing because the full \
             schema is too large for the prompt. Call `describe_table` whenever \
             you need to know what columns a specific table has \u{2014} do not guess.\n",
        );
    }

    s.push_str("\n=== DATABASE SCHEMA ===\n");
    s.push_str(schema_text);
    s
}
