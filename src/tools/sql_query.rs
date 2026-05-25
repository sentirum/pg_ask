//! `sql_query` tool — the single way the model gets to touch the database.
//!
//! Layers of defence around the SPI call (top-to-bottom):
//!
//! 1. `sql_guard::validate` rejects writes, multi-statements, banned funcs.
//! 2. `SET LOCAL statement_timeout` bounds each tool invocation.
//! 3. `SET LOCAL transaction_read_only = on` when readonly mode is set —
//!    enforced by Postgres itself, not by our string match.
//! 4. Row + cell caps before the model sees the data.
//!
//! Errors flow back to the model as `is_error = true` tool outputs so it
//! can self-correct (typos, wrong column names, etc.) instead of aborting
//! the entire `ask()` invocation.

use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::providers::ToolSpec;
use crate::sql_guard::{self, GuardMode};
use pgrx::prelude::*;
use serde_json::json;

const MAX_CELL_CHARS: usize = 500;

pub struct SqlQueryTool {
    pub readonly: bool,
    pub max_rows: usize,
    pub statement_timeout_ms: u64,
}

impl Tool for SqlQueryTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sql_query",
            description: "Execute a read-only SQL query against the current database \
                and return the results as a text table. Use this to look up real \
                values; do not invent data. Prefer adding LIMIT to keep results small.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "A single SQL statement to execute."
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput> {
        let raw = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AskError::Tool {
                name: "sql_query".into(),
                message: "missing required argument `query`".into(),
            })?;

        let mode = if self.readonly {
            GuardMode::Readonly
        } else {
            GuardMode::Writable
        };

        let validated = match sql_guard::validate(raw, mode) {
            Ok(v) => v,
            Err(e) => return Ok(err(&e.to_string())),
        };

        match run_query_to_text(
            validated.as_str(),
            self.readonly,
            self.max_rows,
            self.statement_timeout_ms,
        ) {
            Ok(table) => Ok(ok(table)),
            Err(e) => Ok(err(&format!("query failed: {e}"))),
        }
    }
}

fn ok(text: String) -> ToolOutput {
    ToolOutput {
        text,
        is_error: false,
    }
}

fn err(msg: &str) -> ToolOutput {
    ToolOutput {
        text: msg.to_string(),
        is_error: true,
    }
}

/// Execute `query` under timeouts + readonly + row cap, render as a text
/// table. We do **not** wrap this in `catch_unwind` — pgrx already converts
/// Postgres longjmp errors into Rust panics that propagate as `Result::Err`
/// through `SpiResult`, and wrapping `catch_unwind` over a SPI boundary
/// risks leaving Postgres' error stack in an inconsistent state.
fn run_query_to_text(
    query: &str,
    readonly: bool,
    max_rows: usize,
    statement_timeout_ms: u64,
) -> std::result::Result<String, String> {
    let timeout_sql = format!("SET LOCAL statement_timeout = {statement_timeout_ms}");
    let readonly_sql = "SET LOCAL transaction_read_only = on";

    Spi::connect_mut(|client| -> std::result::Result<String, String> {
        // GUC scope: SET LOCAL is automatically rolled back at end of the
        // outer transaction. Inside the same transaction these stay in
        // effect for every subsequent statement, including the user's
        // remaining work — that's why we restore them in the cleanup at the
        // bottom. (Worth noting: in the typical pg_ask.ask() call the only
        // statement that runs *after* ours is the tool result feed-back,
        // which doesn't hit SPI again, so restoring is belt-and-braces.)
        client
            .update(timeout_sql.as_str(), None, &[])
            .map_err(|e| e.to_string())?;
        if readonly {
            client
                .update(readonly_sql, None, &[])
                .map_err(|e| e.to_string())?;
        }

        let tuptable = client
            .select(query, None, &[])
            .map_err(|e| e.to_string())?;

        let columns = tuptable.columns().map_err(|e| e.to_string())?;
        let col_names: Vec<String> = (1..=columns)
            .map(|i| {
                tuptable
                    .column_name(i)
                    .unwrap_or_else(|_| format!("col{i}"))
            })
            .collect();

        let mut rows: Vec<Vec<String>> = Vec::new();
        let mut truncated = false;

        for (idx, row) in tuptable.into_iter().enumerate() {
            if idx >= max_rows {
                truncated = true;
                break;
            }
            let mut cells: Vec<String> = Vec::with_capacity(col_names.len());
            for c in 1..=col_names.len() {
                let val: Option<String> = row
                    .get_datum_by_ordinal(c)
                    .ok()
                    .and_then(|d| d.value::<String>().ok().flatten());
                let s = val.unwrap_or_else(|| "NULL".to_string());
                cells.push(truncate_cell(&s));
            }
            rows.push(cells);
        }

        Ok(render_table(&col_names, &rows, truncated, max_rows))
    })
}

fn truncate_cell(s: &str) -> String {
    if s.chars().count() > MAX_CELL_CHARS {
        let cut: String = s.chars().take(MAX_CELL_CHARS).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

fn render_table(cols: &[String], rows: &[Vec<String>], truncated: bool, cap: usize) -> String {
    if rows.is_empty() {
        return format!("(0 rows)\ncolumns: {}", cols.join(", "));
    }
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let max_cell = rows
                .iter()
                .map(|r| r.get(i).map(|s| s.chars().count()).unwrap_or(0))
                .max()
                .unwrap_or(0);
            name.chars().count().max(max_cell)
        })
        .collect();

    let mut out = String::new();
    fmt_row(&mut out, cols.iter().map(String::as_str), &widths);
    fmt_sep(&mut out, &widths);
    for row in rows {
        fmt_row(&mut out, row.iter().map(String::as_str), &widths);
    }
    out.push_str(&format!("\n({} rows)", rows.len()));
    if truncated {
        out.push_str(&format!(" — truncated at {cap}"));
    }
    out
}

fn fmt_row<'a, I: Iterator<Item = &'a str>>(out: &mut String, cells: I, widths: &[usize]) {
    let parts: Vec<String> = cells
        .zip(widths.iter())
        .map(|(c, w)| format!("{:<width$}", c, width = w))
        .collect();
    out.push_str(&parts.join(" | "));
    out.push('\n');
}

fn fmt_sep(out: &mut String, widths: &[usize]) {
    let parts: Vec<String> = widths.iter().map(|w| "-".repeat(*w)).collect();
    out.push_str(&parts.join("-+-"));
    out.push('\n');
}
