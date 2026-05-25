//! `sql_query` tool — executes a SELECT and returns a compact textual table
//! the model can reason over. Runs via SPI in the caller's transaction.

use super::{Tool, ToolOutput};
use crate::error::{AskError, Result};
use crate::providers::ToolSpec;
use pgrx::prelude::*;
use serde_json::json;

const MAX_ROWS: usize = 200;
const MAX_CELL_CHARS: usize = 500;

pub struct SqlQueryTool {
    pub readonly: bool,
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
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AskError::Tool {
                name: "sql_query".into(),
                message: "missing required argument `query`".into(),
            })?
            .trim();

        if query.is_empty() {
            return Ok(err("empty query"));
        }

        if self.readonly && !is_pure_select(query) {
            return Ok(err(
                "readonly mode is enabled; only single SELECT/WITH/EXPLAIN statements are allowed",
            ));
        }

        // SPI runs synchronously in the caller's transaction. Errors caught by
        // pgrx surface as Rust Result; we convert them to model-visible text so
        // the agent can recover (e.g. fix a typo and retry) instead of aborting.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_query_to_text(query)
        }));

        match result {
            Ok(Ok(table)) => Ok(ok(table)),
            Ok(Err(e)) => Ok(err(&format!("query failed: {e}"))),
            Err(_) => Ok(err("query aborted (panic in SPI)")),
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

/// Lightweight gate: lower-cased trimmed statement must start with SELECT/WITH/EXPLAIN
/// and must not contain a `;` followed by more tokens. This is *defence in depth*
/// over the readonly transaction we should also be wrapping around the call.
fn is_pure_select(query: &str) -> bool {
    let lower = query.trim_start().to_ascii_lowercase();
    let starts_ok = lower.starts_with("select ")
        || lower.starts_with("with ")
        || lower.starts_with("explain ")
        || lower.starts_with("table ");
    if !starts_ok {
        return false;
    }
    // Reject multi-statement payloads.
    let trimmed = query.trim_end().trim_end_matches(';');
    !trimmed.contains(';')
}

/// Execute `query` via SPI and render the result as a simple text table.
///
/// We keep this deliberately format-agnostic — the model is good at reading
/// space-aligned columns. Truncate at MAX_ROWS / MAX_CELL_CHARS to keep
/// context bounded.
fn run_query_to_text(query: &str) -> std::result::Result<String, String> {
    Spi::connect(|client| -> std::result::Result<String, String> {
        let tuptable = client
            .select(query, None, None)
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
            if idx >= MAX_ROWS {
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

        Ok(render_table(&col_names, &rows, truncated))
    })
    .map_err(|e| e.to_string())?
}

fn truncate_cell(s: &str) -> String {
    if s.chars().count() > MAX_CELL_CHARS {
        let cut: String = s.chars().take(MAX_CELL_CHARS).collect();
        format!("{cut}…")
    } else {
        s.to_string()
    }
}

fn render_table(cols: &[String], rows: &[Vec<String>], truncated: bool) -> String {
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
        out.push_str(&format!(" — truncated at {MAX_ROWS}"));
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
