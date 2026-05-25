//! User-defined tools — SQL snippets registered by operators via
//! `pg_ask.register_tool(name, spec, body)`.
//!
//! At invocation time the tool's jsonb arguments are interpolated into
//! `{{key}}` placeholders in the body string. The resulting SQL is executed
//! via SPI and the result set is rendered as a text table (same layout as
//! `sql_query`).
//!
//! Security model: only the owner (or a superuser) can delete a registered
//! tool. The body itself is raw SQL — it is the operator's responsibility
//! to validate it before registering. There is no sql_guard on user-defined
//! tools because the operator explicitly opted in to the snippet.

use super::{Tool, ToolOutput};
use crate::infra::errors::{AskError, Result};
use crate::providers::ToolSpec;
use pgrx::prelude::*;
use serde_json::Value;

const MAX_CELL_CHARS: usize = 500;

pub struct UserDefinedTool {
    pub name: String,
    pub body: String,
    pub spec: ToolSpec,
}

impl Tool for UserDefinedTool {
    fn spec(&self) -> ToolSpec {
        self.spec.clone()
    }

    fn invoke(&self, args: &Value) -> Result<ToolOutput> {
        let sql = interpolate(&self.body, args).map_err(|e| AskError::Tool {
            name: self.name.clone(),
            message: e,
        })?;

        match run_sql(&sql) {
            Ok(text) => Ok(ToolOutput {
                text,
                is_error: false,
            }),
            Err(e) => Ok(ToolOutput {
                text: format!("tool `{}` failed: {e}", self.name),
                is_error: true,
            }),
        }
    }
}

/// Replace `{{key}}` placeholders in `template` with the corresponding
/// jsonb values. Nested values are rendered as compact JSON strings.
fn interpolate(template: &str, args: &Value) -> std::result::Result<String, String> {
    let mut out = template.to_string();
    if let Value::Object(map) = args {
        for (key, val) in map {
            let placeholder = format!("{{{{{}}}}}" , key);
            let replacement = match val {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Bool(b) => b.to_string(),
                Value::Null => "NULL".to_string(),
                other => other.to_string(),
            };
            out = out.replace(&placeholder, &replacement);
        }
    }
    // Defensive: if any `{{...}}` remains, warn the model.
    if out.contains("{{") {
        return Err(format!(
            "interpolation incomplete: body still contains `{{...}}` placeholders. \
             Available arguments: {}",
            args.to_string()
        ));
    }
    Ok(out)
}

fn run_sql(query: &str) -> std::result::Result<String, String> {
    Spi::connect(|client| -> std::result::Result<String, String> {
        let tuptable = client.select(query, None, &[]).map_err(|e| e.to_string())?;
        let columns = tuptable.columns().map_err(|e| e.to_string())?;
        let col_names: Vec<String> = (1..=columns)
            .map(|i| {
                tuptable
                    .column_name(i)
                    .unwrap_or_else(|_| format!("col{i}"))
            })
            .collect();

        let mut rows: Vec<Vec<String>> = Vec::new();
        for row in tuptable.into_iter() {
            let mut cells: Vec<String> = Vec::with_capacity(col_names.len());
            for c in 1..=col_names.len() {
                let val: Option<String> = row
                    .get_datum_by_ordinal(c)
                    .ok()
                    .and_then(|d| d.value::<String>().ok().flatten());
                cells.push(truncate_cell(&val.unwrap_or_else(|| "NULL".to_string())));
            }
            rows.push(cells);
        }
        Ok(render_table(&col_names, &rows))
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
