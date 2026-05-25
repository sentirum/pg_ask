//! Schema introspection.
//!
//! Reads the current database's user-visible structure from `pg_catalog`
//! and renders a compact summary the LLM uses as system-prompt context.
//! Internal pg_ask tables and system schemas are excluded.
//!
//! v0.3 will add a token-budget mode that drops to a tables-only listing
//! when the full dump would exceed a configurable budget.

mod introspect;
mod render;

use crate::infra::errors::Result;

#[allow(unused_imports)]
pub use introspect::{ColumnRow, TableKey};

/// Summary the agent passes to the provider. Today this is just text; in
/// v0.3 we will add structured fields (table names, token estimate) so the
/// agent can decide between full-dump and compact modes.
#[derive(Debug)]
pub struct SchemaSummary {
    pub text: String,
}

/// Produce a textual schema summary suitable for inclusion in a system prompt.
pub fn summarize() -> Result<SchemaSummary> {
    let rows = introspect::fetch_columns()?;
    let text = render::render(&rows);
    Ok(SchemaSummary { text })
}
