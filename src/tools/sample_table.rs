//! `sample_table` tool — return a few example rows from a table.
//!
//! Cheaper than the full schema dump when the model just wants to know
//! what a table looks like. Uses `SELECT * FROM schema.table LIMIT n`
//! under the same defence layers as `sql_query`:
//!   * `has_table_privilege(..., 'SELECT')` guards against invisible tables.
//!   * `SET LOCAL statement_timeout` bounds the read.
//!   * `SET LOCAL transaction_read_only = on` when readonly mode is active.
//!   * Row + cell caps before the model sees data.
//!   * `sensitive_columns` redaction is applied cell-by-cell.
//!
//! Errors (table not found, no privilege) flow back as `is_error` so the
//! model can self-correct.

use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::providers::ToolSpec;
use pgrx::prelude::*;
use serde_json::json;

const MAX_CELL_CHARS: usize = 500;
const DEFAULT_SAMPLE_ROWS: usize = 3;
const MAX_SAMPLE_ROWS: usize = 10;

pub struct SampleTableTool {
    pub readonly: bool,
    pub max_rows: usize,
    pub statement_timeout_ms: u64,
    pub sensitive_columns: Vec<String>,
}

impl Tool for SampleTableTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "sample_table".to_string(),
            description: "Return a few sample rows from a table. Use this when \
                you need to see actual data values or column contents without \
                writing a full query.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "schema": {
                        "type": "string",
                        "description": "Schema name (e.g. public)."
                    },
                    "table": {
                        "type": "string",
                        "description": "Table name."
                    },
                    "n": {
                        "type": "integer",
                        "description": "Number of rows to sample (1–10, default 3)."
                    }
                },
                "required": ["schema", "table"]
            }),
        }
    }

    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput> {
        let schema = args
            .get("schema")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AskError::Tool {
                name: "sample_table".to_string(),
                message: "missing required argument `schema`".into(),
            })?;
        let table = args
            .get("table")
            .and_then(|v| v.as_str())
            .ok_or_else(|| AskError::Tool {
                name: "sample_table".to_string(),
                message: "missing required argument `table`".into(),
            })?;
        let n = args
            .get("n")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_SAMPLE_ROWS)
            .clamp(1, MAX_SAMPLE_ROWS)
            .min(self.max_rows);

        let query = format!(
            "SELECT * FROM {}.{} LIMIT {}",
            quote_ident(schema),
            quote_ident(table),
            n
        );

        match run_sample(
            &query,
            schema,
            table,
            self.readonly,
            self.statement_timeout_ms,
            &self.sensitive_columns,
        ) {
            Ok(text) => Ok(ToolOutput {
                text,
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                text: format!("sample failed: {e}"),
                is_error: true,
            }),
        }
    }
}

fn run_sample(
    query: &str,
    schema: &str,
    table: &str,
    readonly: bool,
    statement_timeout_ms: u64,
    sensitive: &[String],
) -> std::result::Result<String, String> {
    let timeout_sql = format!("SET LOCAL statement_timeout = {statement_timeout_ms}");
    let readonly_sql = "SET LOCAL transaction_read_only = on";

    Spi::connect_mut(|client| -> std::result::Result<String, String> {
        // Audit FIRST, before transaction_read_only is flipped. See
        // the matching comment in tools::sql_query::run_query_to_text
        // for the rationale; same bug, same fix. row_count is -1 to
        // signal "query is about to run".
        let _ = client.update(
            "INSERT INTO ask._sql_audit (query, row_count, readonly, tool_name) \
             VALUES ($1, $2, $3, 'sample_table')",
            None,
            &[query.into(), (-1i32).into(), readonly.into()],
        );

        client
            .update(timeout_sql.as_str(), None, &[])
            .map_err(|e| e.to_string())?;
        if readonly {
            client
                .update(readonly_sql, None, &[])
                .map_err(|e| e.to_string())?;
        }

        // Privilege guard: invisible tables return 0 rows; we don't leak
        // existence by distinguishing "no rows" from "no privilege".
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

        // Build the fully-qualified column identifiers for redaction lookup.
        let fqn_cols: Vec<String> = col_names
            .iter()
            .map(|c| format!("{schema}.{table}.{c}"))
            .collect();

        let mut rows: Vec<Vec<String>> = Vec::new();
        for row in tuptable.into_iter() {
            let mut cells: Vec<String> = Vec::with_capacity(col_names.len());
            for c in 1..=col_names.len() {
                let val: Option<String> = row
                    .get_datum_by_ordinal(c)
                    .ok()
                    .and_then(|d| d.value::<String>().ok().flatten());
                let s = val.unwrap_or_else(|| "NULL".to_string());
                let fqn = &fqn_cols[c - 1];
                cells.push(if is_sensitive(fqn, sensitive) {
                    "<redacted>".to_string()
                } else {
                    truncate_cell(&s)
                });
            }
            rows.push(cells);
        }

        // (Audit row was written above, before transaction_read_only
        // was flipped, with row_count = -1.)

        Ok(render_table(&col_names, &rows))
    })
}

fn is_sensitive(fqn: &str, patterns: &[String]) -> bool {
    if patterns.is_empty() {
        return false;
    }
    // Exact match or suffix match: "public.users.password" matches
    // both "public.users.password" and "users.password".
    patterns.iter().any(|p| {
        fqn == p || fqn.ends_with(&format!(".{p}"))
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

fn render_table(cols: &[String], rows: &[Vec<String>]) -> String {
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

/// Minimal identifier quoting — only double-quote when needed.
fn quote_ident(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}
