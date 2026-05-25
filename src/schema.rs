//! Schema introspection — reads the current database's user-visible structure
//! and renders a compact summary the LLM can use as system-prompt context.
//!
//! Skips system schemas (pg_catalog, information_schema) and pg_ask's own
//! internal tables (those starting with `_` in the pg_ask schema).

use crate::error::{AskError, Result};
use pgrx::prelude::*;

const SCHEMA_QUERY: &str = r#"
SELECT
    n.nspname              AS schema_name,
    c.relname              AS table_name,
    a.attname              AS column_name,
    pg_catalog.format_type(a.atttypid, a.atttypmod) AS data_type,
    a.attnotnull           AS not_null,
    COALESCE(d.description, '') AS comment
FROM pg_catalog.pg_attribute a
JOIN pg_catalog.pg_class     c ON c.oid = a.attrelid
JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN pg_catalog.pg_description d
       ON d.objoid = c.oid AND d.objsubid = a.attnum
WHERE a.attnum > 0
  AND NOT a.attisdropped
  AND c.relkind IN ('r','v','m','p','f')           -- tables/views/matviews/partitions/foreign
  AND n.nspname NOT IN ('pg_catalog','information_schema','pg_toast')
  AND n.nspname NOT LIKE 'pg\_temp\_%'
  AND n.nspname NOT LIKE 'pg\_toast\_temp\_%'
  AND NOT (n.nspname = 'pg_ask')                   -- hide our own internals
ORDER BY schema_name, table_name, a.attnum
"#;

/// Produce a textual schema summary suitable for inclusion in a system prompt.
pub fn summarize() -> Result<String> {
    let mut out = String::new();
    let mut current_table: Option<(String, String)> = None;

    Spi::connect(|client| -> std::result::Result<(), String> {
        let rows = client
            .select(SCHEMA_QUERY, None, None)
            .map_err(|e| e.to_string())?;

        for row in rows {
            let schema: String = row
                .get_datum_by_ordinal(1)
                .ok()
                .and_then(|d| d.value().ok().flatten())
                .unwrap_or_default();
            let table: String = row
                .get_datum_by_ordinal(2)
                .ok()
                .and_then(|d| d.value().ok().flatten())
                .unwrap_or_default();
            let column: String = row
                .get_datum_by_ordinal(3)
                .ok()
                .and_then(|d| d.value().ok().flatten())
                .unwrap_or_default();
            let dtype: String = row
                .get_datum_by_ordinal(4)
                .ok()
                .and_then(|d| d.value().ok().flatten())
                .unwrap_or_default();
            let not_null: bool = row
                .get_datum_by_ordinal(5)
                .ok()
                .and_then(|d| d.value().ok().flatten())
                .unwrap_or(false);
            let comment: String = row
                .get_datum_by_ordinal(6)
                .ok()
                .and_then(|d| d.value().ok().flatten())
                .unwrap_or_default();

            if current_table.as_ref().map(|(s, t)| (s.as_str(), t.as_str()))
                != Some((schema.as_str(), table.as_str()))
            {
                if current_table.is_some() {
                    out.push('\n');
                }
                out.push_str(&format!("TABLE {schema}.{table}\n"));
                current_table = Some((schema.clone(), table.clone()));
            }

            let nn = if not_null { " NOT NULL" } else { "" };
            let cm = if comment.is_empty() {
                String::new()
            } else {
                format!("  -- {comment}")
            };
            out.push_str(&format!("  {column} {dtype}{nn}{cm}\n"));
        }
        Ok(())
    })
    .map_err(|e| AskError::Sql(e.to_string()))?;

    if out.is_empty() {
        out.push_str("(no user-visible tables found)");
    }
    Ok(out)
}
