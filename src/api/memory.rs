//! SQL surface for the long-term memory layer.
//!
//! ```sql
//! SELECT pg_ask.remember('User prefers concise SQL answers.');
//! SELECT pg_ask.remember(
//!   'Q4 revenue numbers',
//!   namespace := 'analytics',
//!   metadata  := '{"source":"slack"}'::jsonb
//! );
//!
//! SELECT * FROM pg_ask.recall('what does the user prefer?');
//! SELECT * FROM pg_ask.recall('revenue', namespace := 'analytics', limit_n := 10);
//!
//! SELECT pg_ask.forget('00000000-…'::uuid);
//! ```
//!
//! All three functions are owner-scoped — a row inserted by role A is
//! invisible to role B. The `recall` SRF is a table-valued function so
//! the result is JOIN-able against your own tables.

use crate::memory;
use pgrx::prelude::*;
use pgrx::Uuid;

/// Persist a piece of text in the caller's long-term memory. Returns the
/// new row's id so it can be passed to `pg_ask.forget` later.
#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn remember(
    content: &str,
    namespace: default!(&str, "'default'"),
    metadata: default!(Option<pgrx::Json>, "NULL"),
) -> Uuid {
    let md = metadata.map(|j| j.0);
    match memory::remember(content, Some(namespace), md) {
        Ok(id) => id,
        Err(e) => error!("pg_ask.remember: {e}"),
    }
}

/// Top-N hybrid search over the caller's memory.
#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn recall(
    query: &str,
    namespace: default!(&str, "'default'"),
    limit_n: default!(i32, "5"),
) -> TableIterator<
    'static,
    (
        name!(id, Uuid),
        name!(content, String),
        name!(metadata, pgrx::Json),
        name!(similarity, f64),
    ),
> {
    let hits = match memory::recall(query, Some(namespace), limit_n.max(1) as usize) {
        Ok(h) => h,
        Err(e) => error!("pg_ask.recall: {e}"),
    };

    // Materialise once: TableIterator wants 'static, and the hit set is
    // already bounded by `limit_n.max(1).min(100)` inside the search SQL.
    let rows: Vec<_> = hits
        .into_iter()
        .map(|h| (h.id, h.content, pgrx::Json(h.metadata), h.similarity))
        .collect();
    TableIterator::new(rows.into_iter())
}

/// Delete a memory row. Returns `false` for unknown / not-owned ids — same
/// answer in both cases so id existence does not leak.
#[pg_extern(schema = "pg_ask", volatile, parallel_unsafe)]
fn forget(id: Uuid) -> bool {
    match memory::forget(id) {
        Ok(b) => b,
        Err(e) => error!("pg_ask.forget: {e}"),
    }
}

/// Browse memories the caller owns. Optional `namespace` filter; defaults
/// to NULL (all namespaces). Newest-first, `limit_n` capped at 200.
#[pg_extern(schema = "pg_ask", stable, parallel_safe)]
fn list_memories(
    namespace: default!(Option<&str>, "NULL"),
    limit_n: default!(i32, "50"),
    offset_n: default!(i32, "0"),
) -> TableIterator<
    'static,
    (
        name!(id, Uuid),
        name!(namespace, String),
        name!(content, String),
        name!(metadata, pgrx::Json),
        name!(created_at_iso, String),
    ),
> {
    let rows = match memory::list(
        namespace,
        limit_n.max(1) as usize,
        offset_n.max(0) as usize,
    ) {
        Ok(r) => r,
        Err(e) => error!("pg_ask.list_memories: {e}"),
    };
    let materialised: Vec<_> = rows
        .into_iter()
        .map(|r| (r.id, r.namespace, r.content, pgrx::Json(r.metadata), r.created_at_iso))
        .collect();
    TableIterator::new(materialised.into_iter())
}

/// Enumerate namespaces the caller has populated, with row counts.
/// Ordered by row count desc — a good "what is in here?" probe.
#[pg_extern(schema = "pg_ask", stable, parallel_safe)]
fn list_namespaces() -> TableIterator<
    'static,
    (name!(namespace, String), name!(n, i64)),
> {
    let rows = match memory::namespaces() {
        Ok(r) => r,
        Err(e) => error!("pg_ask.list_namespaces: {e}"),
    };
    let materialised: Vec<_> = rows.into_iter().map(|r| (r.namespace, r.n)).collect();
    TableIterator::new(materialised.into_iter())
}
