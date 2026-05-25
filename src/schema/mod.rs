//! Schema introspection.
//!
//! Reads the current database's user-visible structure from `pg_catalog`
//! and renders a compact summary the LLM uses as system-prompt context.
//! Internal pg_ask tables and system schemas are excluded.
//!
//! Two render modes:
//!
//! * [`SchemaMode::Full`] — every column of every table. Used when the
//!   render fits in the operator's `pg_ask.schema_char_budget`.
//! * [`SchemaMode::Compact`] — tables-only listing plus a note that
//!   `describe_table` is available for column detail. Kicks in
//!   automatically when the full render would blow the budget.
//!
//! Choosing the mode is the caller's job — `summarize_within(budget)`
//! does it for you.

mod introspect;
mod render;

use crate::infra::errors::Result;

pub use introspect::{fetch_columns, fetch_columns_for, fetch_table_comments, ColumnRow};

/// Output of [`summarize_within`]. Today the mode is informational only,
/// surfaced in trace rows once we wire it through.
#[derive(Debug)]
pub struct SchemaSummary {
    pub text: String,
    #[allow(dead_code)]
    pub mode: SchemaMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaMode {
    Full,
    Compact,
}

/// Build a schema summary that fits within `char_budget`. We first try the
/// full render; if it overflows, we fall back to the compact listing.
///
/// `char_budget` is a *soft* cap on the chosen render itself — the compact
/// mode is always allowed even when it too exceeds the budget (a DB with
/// thousands of tables still benefits from knowing they exist).
pub fn summarize_within(char_budget: usize) -> Result<SchemaSummary> {
    let columns = fetch_columns()?;
    let table_comments = fetch_table_comments()?;

    let full = render::render_full(&columns, &table_comments);
    if full.chars().count() <= char_budget {
        return Ok(SchemaSummary {
            text: full,
            mode: SchemaMode::Full,
        });
    }

    let compact = render::render_compact(&columns, &table_comments);
    Ok(SchemaSummary {
        text: compact,
        mode: SchemaMode::Compact,
    })
}
