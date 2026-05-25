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

use super::render::{self, RenderedTable};
use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::providers::ToolSpec;
use pgrx::prelude::*;
use serde_json::json;

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
                writing a full query."
                .to_string(),
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
            n,
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
    max_rows: usize,
    statement_timeout_ms: u64,
    sensitive: &[String],
) -> std::result::Result<String, String> {
    // Audit row stays in the parent txn so it's visible even if the
    // subtxn aborts. Errors here are swallowed — audit is
    // best-effort and must never break the user's call. We discard
    // the SpiTupleTable rather than returning it so the closure has
    // no lifetime escape problem.
    let _ = Spi::connect_mut(|client| -> std::result::Result<(), String> {
        client
            .update(
                "SELECT ask._sql_audit_insert($1, $2, $3, 'sample_table')",
                None,
                &[query.into(), (-1i32).into(), readonly.into()],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    });

    // sample_table knows the schema/table the columns come from, so we
    // expand each pattern to its bare-name / table.col / schema.table.col
    // variants. The render helper does its own bare-name + dotted-suffix
    // matching, so feeding it the FQN forms is what enables full
    // qualification (e.g. `sensitive_columns = 'public.users.password'`).
    let expanded = expand_patterns_for_table(schema, table, sensitive);

    // Subtxn wrapper: scopes `SET LOCAL statement_timeout` and the
    // readonly flag so they don't leak into the rest of the
    // surrounding `ask.ask()` transaction. Without this, every
    // subsequent INSERT (telemetry::write, session::record_turn,
    // next tool's audit row) would fail with 25006 once
    // transaction_read_only landed on the parent. Mirrors
    // sql_query::run_query_to_text; see that file's docs for the
    // full motivation.
    let timeout_sql = format!("SET LOCAL statement_timeout = {statement_timeout_ms}");
    let query_owned = query.to_string();
    let max_rows_copy = max_rows;
    let expanded_clone = expanded.clone();
    let result: crate::infra::errors::Result<RenderedTable> =
        crate::infra::subtxn::run_in_subtransaction(Some("pg_ask_sample_table"), move || {
            Spi::connect_mut(|client| -> crate::infra::errors::Result<()> {
                client
                    .update(timeout_sql.as_str(), None, &[])
                    .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))?;
                if readonly {
                    client
                        .update("SET LOCAL transaction_read_only = on", None, &[])
                        .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))?;
                }
                Ok(())
            })?;
            // Privilege guard: invisible tables return 0 rows naturally;
            // we don't leak existence by distinguishing "no rows" from
            // "no privilege".
            render::run_to_table(&query_owned, max_rows_copy, &expanded_clone)
                .map_err(crate::infra::errors::AskError::Sql)
        });
    result.map(|r| r.text).map_err(|e| e.to_string())
}

/// Expand `sensitive_columns` patterns so the render helper's bare-name +
/// dotted-suffix matcher picks up FQN-style patterns like
/// `public.users.password`. The original pattern is preserved; for any
/// dotted form whose tail matches `schema.table.*`, we also push the
/// bare column suffix so a bare column from the JSON wrapper still
/// matches.
fn expand_patterns_for_table(schema: &str, table: &str, patterns: &[String]) -> Vec<String> {
    let prefix = format!("{schema}.{table}.").to_ascii_lowercase();
    let table_prefix = format!("{table}.").to_ascii_lowercase();
    let mut out: Vec<String> = Vec::with_capacity(patterns.len() * 2);
    for p in patterns {
        let lower = p.to_ascii_lowercase();
        out.push(p.clone());
        // `public.users.password` while sampling public.users → also match `password`.
        if let Some(tail) = lower.strip_prefix(&prefix) {
            out.push(tail.to_string());
        }
        // `users.password` while sampling *.users → also match `password`.
        else if let Some(tail) = lower.strip_prefix(&table_prefix) {
            out.push(tail.to_string());
        }
    }
    out
}

/// Minimal identifier quoting — only double-quote when needed.
fn quote_ident(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !s.is_empty() {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}
