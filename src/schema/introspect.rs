//! `pg_catalog` queries for schema introspection.
//!
//! Returning structured rows (rather than rendering text inside the SPI
//! callback) keeps render-policy decisions — truncation, token budget,
//! markdown vs plain text — out of the SQL layer.

use crate::infra::errors::Result;
use pgrx::prelude::*;

/// Reserved for the schema-summary cache in v0.3.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TableKey {
    pub schema: String,
    pub table: String,
}

#[derive(Debug, Clone)]
pub struct ColumnRow {
    pub schema: String,
    pub table: String,
    pub column: String,
    pub data_type: String,
    pub not_null: bool,
    pub comment: String,
}

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

pub fn fetch_columns() -> Result<Vec<ColumnRow>> {
    let mut out: Vec<ColumnRow> = Vec::new();

    Spi::connect(|client| -> Result<()> {
        let rows = client.select(SCHEMA_QUERY, None, &[])?;

        for row in rows {
            out.push(ColumnRow {
                schema: text_at(&row, 1).unwrap_or_default(),
                table: text_at(&row, 2).unwrap_or_default(),
                column: text_at(&row, 3).unwrap_or_default(),
                data_type: text_at(&row, 4).unwrap_or_default(),
                not_null: bool_at(&row, 5).unwrap_or(false),
                comment: text_at(&row, 6).unwrap_or_default(),
            });
        }
        Ok(())
    })?;

    Ok(out)
}

fn text_at(row: &pgrx::spi::SpiHeapTupleData<'_>, ord: usize) -> Option<String> {
    row.get_datum_by_ordinal(ord)
        .ok()
        .and_then(|d| d.value::<String>().ok().flatten())
}

fn bool_at(row: &pgrx::spi::SpiHeapTupleData<'_>, ord: usize) -> Option<bool> {
    row.get_datum_by_ordinal(ord)
        .ok()
        .and_then(|d| d.value::<bool>().ok().flatten())
}
