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
            // Schema-qualification + efficiency guidance. The single biggest
            // cause of wasted iterations is the model assuming the `public`
            // schema and then hunting for tables it cannot find. The schema
            // dump below already lists every table as `schema.table`; tell
            // the model to trust it and not re-discover the catalog.
            s.push_str(
                "\nIMPORTANT — work efficiently to avoid wasting steps:\n\
                 - The schema below lists every table fully qualified as \
                 `schema.table`. USE THOSE EXACT NAMES. Do NOT assume the \
                 `public` schema and do NOT query pg_catalog / \
                 information_schema to re-discover tables — they are already \
                 listed for you.\n\
                 - Write ONE complete query that answers the question \
                 (JOINs, CTEs, aggregates, window functions are all allowed) \
                 rather than many small exploratory ones.\n\
                 - Only use `sample_table` / `describe_table` if a column's \
                 meaning is genuinely unclear; otherwise go straight to the \
                 answering query.\n\
                 - If a query errors, read the message, fix that specific \
                 issue, and retry — do not restart your exploration from \
                 scratch.\n",
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

    // Final, schema-adjacent reminder. LLMs weight the instruction nearest
    // the data most heavily, and the most expensive recurring mistake is
    // assuming `public`. List the actual schemas in play right after the
    // dump so the model qualifies its first query correctly instead of
    // probing `public.*`, finding nothing, and re-discovering the catalog.
    if matches!(mode, AgentMode::Execute) {
        let schemas = crate::schema::distinct_schemas(schema_text);
        if !schemas.is_empty() {
            s.push_str("\n=== HOW TO REFERENCE TABLES ===\n");
            // The agent loop pins `search_path` to exactly these schemas
            // before each query (see schema::search_path_clause), so BARE
            // table names always resolve. Telling the model to use bare
            // names is the reliable instruction: a qualified `public.x`
            // would bypass the pinned path and fail, whereas a bare `x`
            // resolves no matter which schema it lives in. This removes the
            // single biggest source of wasted iterations.
            s.push_str(
                "The search_path is ALREADY set for you to the correct \
                 schema(s). Use BARE table names exactly as written above \
                 (e.g. `orders`, not `public.orders` and not \
                 `schema.orders`). Do NOT prefix tables with a schema, do \
                 NOT assume `public`, and do NOT query information_schema or \
                 pg_catalog to find tables — they are all listed above.\n",
            );
            if schemas.len() > 1 {
                s.push_str(&format!(
                    "(If two schemas define the same table name, then and \
                     only then qualify it; the active schemas are: {}.)\n",
                    schemas.join(", ")
                ));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_schema_hint_is_emitted() {
        let dump = "TABLE shop.orders\n  id int\n";
        let p = build(dump, AgentMode::Execute, true);
        assert!(p.contains("HOW TO REFERENCE TABLES"));
        assert!(p.contains("BARE table names"));
        assert!(p.contains("search_path is ALREADY set"));
    }

    #[test]
    fn generate_only_mode_skips_schema_hint() {
        let dump = "TABLE shop.orders\n  id int\n";
        let p = build(dump, AgentMode::GenerateOnly, true);
        assert!(!p.contains("HOW TO REFERENCE TABLES"));
    }
}
