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

/// Extract the distinct schema names from a rendered schema dump, in
/// first-seen order. Both render modes prefix every table with
/// `schema.table` (full mode as `TABLE schema.table`, compact mode as a
/// leading `schema.table` token), so the same scan works for both.
///
/// Used in two places:
///  - the system prompt, to tell the model which schemas are in play, and
///  - the agent loop, to pin `search_path` so the model's queries resolve
///    even if it forgets to qualify (or mis-qualifies as `public`).
pub fn distinct_schemas(schema_text: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for raw in schema_text.lines() {
        let line = raw.trim_start();
        let candidate = line.strip_prefix("TABLE ").unwrap_or(line);
        let token = candidate.split_whitespace().next().unwrap_or("");
        if let Some((schema, rest)) = token.split_once('.') {
            if !schema.is_empty() && !rest.is_empty() && !schema.contains(' ') {
                let owned = schema.to_string();
                if !seen.contains(&owned) {
                    seen.push(owned);
                }
            }
        }
    }
    seen
}

/// Build a safe `search_path` clause value from a rendered schema dump:
/// every introspected schema (double-quoted to survive odd identifiers),
/// with `public` appended as a fallback. Returns an empty string when the
/// dump exposes no schemas, in which case callers leave search_path alone.
///
/// Identifiers are quoted with the standard `"x"` form and any embedded
/// double-quote is doubled (`"` -> `""`), so the result is safe to splice
/// into `SET LOCAL search_path = ...`. The schema names themselves come
/// from `pg_namespace` via introspection, not from user input.
pub fn search_path_clause(schema_text: &str) -> String {
    let mut parts: Vec<String> = distinct_schemas(schema_text)
        .into_iter()
        .map(|s| format!("\"{}\"", s.replace('"', "\"\"")))
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    if !parts.iter().any(|p| p == "\"public\"") {
        parts.push("\"public\"".to_string());
    }
    parts.join(", ")
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

#[cfg(test)]
mod tests {
    use super::distinct_schemas;

    #[test]
    fn distinct_schemas_full_mode() {
        let dump = "TABLE shop.orders -- customer orders\n  \
                    order_id int NOT NULL\n  customer_id int\n\
                    TABLE shop.customers\n  customer_id int\n\
                    TABLE analytics.events\n  event_id int\n";
        assert_eq!(distinct_schemas(dump), vec!["shop", "analytics"]);
    }

    #[test]
    fn distinct_schemas_compact_mode() {
        let dump = "TABLES (use describe_table for columns):\n  \
                    shop.orders   (7 columns)\n  shop.customers  (6 columns)\n";
        assert_eq!(distinct_schemas(dump), vec!["shop"]);
    }

    #[test]
    fn distinct_schemas_ignores_column_lines() {
        let dump = "TABLE public.users\n  id uuid NOT NULL\n  \
                    email text\n  created_at timestamptz\n";
        assert_eq!(distinct_schemas(dump), vec!["public"]);
    }

    #[test]
    fn search_path_clause_appends_public_fallback() {
        let dump = "TABLE shop.orders\n  id int\n";
        assert_eq!(super::search_path_clause(dump), "\"shop\", \"public\"");
    }

    #[test]
    fn search_path_clause_no_double_public() {
        let dump = "TABLE public.users\n  id int\n";
        assert_eq!(super::search_path_clause(dump), "\"public\"");
    }

    #[test]
    fn search_path_clause_empty_when_no_schema() {
        assert_eq!(
            super::search_path_clause("(no user-visible tables found)"),
            ""
        );
    }

    #[test]
    fn search_path_clause_quotes_embedded_quote() {
        let dump = "TABLE we\"ird.t\n  id int\n";
        // embedded quote doubled, public appended
        assert_eq!(super::search_path_clause(dump), "\"we\"\"ird\", \"public\"");
    }
}
