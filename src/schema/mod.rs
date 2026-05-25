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
///
/// P2 (v0.5.2 review): the result is memoized per-backend with a TTL
/// of [`CACHE_TTL`]. Every `ask()` call previously re-scanned
/// pg_attribute/pg_class/pg_description across every user-visible
/// table; on a 500-table schema that's ~40ms on a warm cache and
/// hundreds of ms when pg_catalog isn't hot. The cache key includes
/// the char_budget so a runtime GUC change doesn't serve a stale
/// render.
///
/// Invalidation is purely time-based today. An event trigger on
/// `ddl_command_end` would let us bust on schema changes immediately,
/// but that requires the operator's role to own a trigger — not free.
/// The 60-second TTL is the same trade-off pg_stat_statements makes
/// for its query-text store.
pub fn summarize_within(char_budget: usize) -> Result<SchemaSummary> {
    use std::cell::RefCell;
    use std::time::Instant;

    // Entry: (when_inserted, budget_used, rendered_summary). Stored
    // per-backend in a thread-local because a Postgres backend is
    // single-threaded by design — no Mutex needed.
    thread_local! {
        static CACHE: RefCell<Option<(Instant, usize, std::sync::Arc<SchemaSummary>)>> =
            const { RefCell::new(None) };
    }

    if let Some(arc) = CACHE.with(|c| {
        let borrow = c.borrow();
        borrow.as_ref().and_then(|(ts, b, summary)| {
            if *b == char_budget && ts.elapsed() < CACHE_TTL {
                Some(summary.clone())
            } else {
                None
            }
        })
    }) {
        return Ok(SchemaSummary {
            text: arc.text.clone(),
            mode: arc.mode,
        });
    }

    let summary = compute_summary(char_budget)?;
    let arc = std::sync::Arc::new(SchemaSummary {
        text: summary.text.clone(),
        mode: summary.mode,
    });
    CACHE.with(|c| {
        *c.borrow_mut() = Some((Instant::now(), char_budget, arc));
    });
    Ok(summary)
}

/// TTL for the per-backend schema cache. See [`summarize_within`].
const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Uncached inner: does the actual pg_catalog scans + render. Split
/// out from [`summarize_within`] purely so the cache wrapper stays
/// easy to read.
fn compute_summary(char_budget: usize) -> Result<SchemaSummary> {
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
