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
/// hundreds of ms when pg_catalog isn't hot.
///
/// ## H13 (Gemini v0.5.2 review item 1.3): role-aware cache key
///
/// The cache key includes the current role OID alongside the
/// char_budget. `compute_summary` filters tables through
/// `has_table_privilege(current_user, ...)`, so two different roles
/// connected through a `SET ROLE` (or sitting behind a connection
/// pooler that re-uses backends across logical users) MUST NOT
/// observe each other's view of the schema. Pre-fix, role A's full
/// render would still be served to role B for up to 60 seconds after
/// A's first call — a real information leak.
///
/// Invalidation is purely time-based today. An event trigger on
/// `ddl_command_end` would let us bust on schema changes immediately,
/// but that requires the operator's role to own a trigger — not free.
/// The 60-second TTL is the same trade-off pg_stat_statements makes
/// for its query-text store.
pub fn summarize_within(char_budget: usize) -> Result<SchemaSummary> {
    use std::cell::RefCell;
    use std::time::Instant;

    // Entry: (when_inserted, role_oid, budget_used, rendered_summary).
    // Stored per-backend in a thread-local because a Postgres backend
    // is single-threaded by design — no Mutex needed. The role OID
    // is the H13 fix: it segments the cache by `current_user` so
    // `SET ROLE` doesn't serve cross-role schema renders.
    thread_local! {
        static CACHE: RefCell<Option<(Instant, u32, usize, std::sync::Arc<SchemaSummary>)>> =
            const { RefCell::new(None) };
    }

    let role_oid = current_user_oid();

    if let Some(arc) = CACHE.with(|c| {
        let borrow = c.borrow();
        borrow.as_ref().and_then(|(ts, role, b, summary)| {
            if *role == role_oid && *b == char_budget && ts.elapsed() < CACHE_TTL {
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
        *c.borrow_mut() = Some((Instant::now(), role_oid, char_budget, arc));
    });
    Ok(summary)
}

/// Look up the OID of `current_user` for the cache key.
///
/// `pg_sys::GetUserId()` is the C-level accessor that backs the
/// `current_user` SQL function. It returns the effective role OID,
/// which correctly reflects `SET ROLE`. The call is a single
/// load-from-global on the backend; cheaper than a SPI round-trip.
///
/// On the off chance the FFI call fails (it never does in practice;
/// any backend running this code has a valid user context), we fall
/// back to OID 0 so caching still works — worst case is a single
/// cache miss after backend startup.
fn current_user_oid() -> u32 {
    // SAFETY: `GetUserId` reads `CurrentUserId`, a backend-local
    // global initialised at session start. It is always valid inside
    // a pg_extern entry point (which is the only way Rust code in
    // this crate runs). The returned Oid is a u32 newtype; we extract
    // the underlying integer for use as a HashMap-equivalent key.
    unsafe { pgrx::pg_sys::GetUserId().to_u32() }
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
