//! `describe_table` tool.
//!
//! Cheap counterpart to `sql_query`: returns the column list of a single
//! table from `pg_catalog`, without running any user-facing SQL. Surfaced
//! to the model alongside `sql_query` whenever the schema render goes
//! into compact mode (i.e. the full schema would have blown the prompt
//! budget) — at that point the model needs an on-demand way to learn what
//! columns a table actually has.
//!
//! Privilege check: introspection is filtered by `has_table_privilege(...,
//! 'SELECT')` in the SQL. Tables the caller cannot select from come back
//! as "table not found", never as a column listing — same NotFound /
//! Unauthorized collapse we use elsewhere.
//!
//! Accepts `{ "table": "public.users" }` or `{ "schema": "public",
//! "table": "users" }`. The model usually picks one or the other depending
//! on the wire format quirks of its provider; we accept both.

use super::{Tool, ToolOutput};
use crate::infra::errors::Result;
use crate::providers::ToolSpec;
use crate::schema::{fetch_columns_for, ColumnRow};
use serde_json::json;

pub struct DescribeTableTool;

impl Tool for DescribeTableTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "describe_table",
            description: "List the columns of a single table in the current database, \
                with their types and any comments. Use this when the system-prompt \
                schema is too large to include every table and you need to look up a \
                specific one. Accepts either a fully-qualified `table` (e.g. \
                \"public.orders\") or separate `schema` and `table` fields.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": {
                        "type": "string",
                        "description": "Schema name (e.g. \"public\"). Optional if `table` is qualified."
                    },
                    "table":  {
                        "type": "string",
                        "description": "Table name, optionally schema-qualified (e.g. \"public.orders\")."
                    }
                },
                "required": ["table"]
            }),
        }
    }

    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput> {
        let (schema, table) = match parse_args(args) {
            Ok(v) => v,
            Err(msg) => return Ok(err(&msg)),
        };

        let rows = fetch_columns_for(&schema, &table)?;
        if rows.is_empty() {
            return Ok(err(&format!(
                "table `{schema}.{table}` not found (or not selectable by current_user)"
            )));
        }
        Ok(ok(render(&schema, &table, &rows)))
    }
}

fn parse_args(args: &serde_json::Value) -> std::result::Result<(String, String), String> {
    let raw_table = args
        .get("table")
        .and_then(|v| v.as_str())
        .ok_or_else(|| "missing required argument `table`".to_string())?
        .trim();

    if raw_table.is_empty() {
        return Err("`table` is empty".into());
    }

    // If `table` is dotted, split it. Otherwise pick up `schema` (default
    // `public`) — the same fallback psql's `\d` uses.
    if let Some((schema, table)) = raw_table.split_once('.') {
        if schema.is_empty() || table.is_empty() {
            return Err(format!("malformed qualified name `{raw_table}`"));
        }
        return Ok((schema.to_string(), table.to_string()));
    }

    let schema = args
        .get("schema")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("public")
        .to_string();

    Ok((schema, raw_table.to_string()))
}

fn render(schema: &str, table: &str, rows: &[ColumnRow]) -> String {
    let mut out = String::new();
    out.push_str("TABLE ");
    out.push_str(schema);
    out.push('.');
    out.push_str(table);
    out.push('\n');
    for r in rows {
        out.push_str("  ");
        out.push_str(&r.column);
        out.push(' ');
        out.push_str(&r.data_type);
        if r.not_null {
            out.push_str(" NOT NULL");
        }
        if !r.comment.is_empty() {
            out.push_str("  -- ");
            out.push_str(&r.comment);
        }
        out.push('\n');
    }
    out
}

fn ok(text: String) -> ToolOutput {
    ToolOutput { text, is_error: false }
}
fn err(msg: &str) -> ToolOutput {
    ToolOutput { text: msg.to_string(), is_error: true }
}

