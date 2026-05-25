//! Long-term memory.
//!
//! Stores arbitrary text + structured metadata, indexed by an embedding
//! vector and a generated `tsvector`. Lookups use a hybrid score:
//!
//! ```text
//! score = alpha * cosine_similarity + (1 - alpha) * ts_rank
//! ```
//!
//! where `alpha` comes from `pg_ask.memory_search_alpha` (default 0.7).
//!
//! Every row is owned by the role that created it (`owner =
//! current_user`); the public API enforces `WHERE owner = current_user`
//! on every read, write, and delete. Same `NotFound == Unauthorized`
//! collapse we use in `session/` so id-space probing leaks nothing.
//!
//! Optional layer: if pgvector is not installed (and therefore
//! `ask._memories` was not created during bootstrap), every entry
//! point returns a clean error pointing the operator at
//! `CREATE EXTENSION vector;`.

pub(crate) mod store;

use crate::embeddings;
use crate::infra::config::RuntimeConfig;
use crate::infra::errors::{AskError, Result};
use crate::infra::http::HttpClient;
use pgrx::Uuid;
use serde_json::Value;

pub use store::{Hit, MemoryRow, NamespaceCount};

/// Insert a new memory row. Returns its id.
pub fn remember(
    content: &str,
    namespace: Option<&str>,
    metadata: Option<Value>,
) -> Result<Uuid> {
    let cfg = RuntimeConfig::load()?;
    remember_with_cfg(&cfg, content, namespace, metadata)
}

/// Variant that uses a pre-loaded snapshot (P1, v0.5.2 review).
pub fn remember_with_cfg(
    cfg: &RuntimeConfig,
    content: &str,
    namespace: Option<&str>,
    metadata: Option<Value>,
) -> Result<Uuid> {
    ensure_memory_available(cfg)?;

    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = embeddings::build(cfg, http)?;

    let embedding = provider
        .embed(&[content])?
        .into_iter()
        .next()
        .ok_or(AskError::EmptyResponse)?;

    store::insert(
        content,
        namespace.unwrap_or("default"),
        metadata.unwrap_or_else(|| serde_json::json!({})),
        &embedding,
    )
}

/// Look up the top-N hits for a free-text query. Combines vector and
/// full-text scores; the blend weight is configurable per session via
/// `SET LOCAL pg_ask.memory_search_alpha = ...`.
pub fn recall(
    query: &str,
    namespace: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>> {
    let cfg = RuntimeConfig::load()?;
    recall_with_cfg(&cfg, query, namespace, limit)
}

/// Variant that uses a pre-loaded snapshot (P1, v0.5.2 review).
pub fn recall_with_cfg(
    cfg: &RuntimeConfig,
    query: &str,
    namespace: Option<&str>,
    limit: usize,
) -> Result<Vec<Hit>> {
    ensure_memory_available(cfg)?;

    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = embeddings::build(cfg, http)?;

    let embedding = provider
        .embed(&[query])?
        .into_iter()
        .next()
        .ok_or(AskError::EmptyResponse)?;

    store::hybrid_search(
        &embedding,
        query,
        namespace.unwrap_or("default"),
        limit.max(1),
        cfg.memory_search_alpha,
    )
}

/// Delete a memory row by id. Ownership is checked inside the SQL; an
/// attempt to delete someone else's id returns `Ok(false)` (collapsed
/// with `not found`) so callers cannot probe id space.
pub fn forget(id: Uuid) -> Result<bool> {
    let cfg = RuntimeConfig::load()?;
    ensure_memory_available(&cfg)?;
    store::delete(id)
}

/// Browse stored memories — owner-scoped, newest-first. Unlike
/// [`recall`] this does not embed anything; it is a plain catalog read.
/// `limit` is clamped inside the store to `[1, 200]`.
pub fn list(
    namespace: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Vec<MemoryRow>> {
    let cfg = RuntimeConfig::load()?;
    ensure_memory_available(&cfg)?;
    store::list_memories(namespace, limit, offset)
}

/// Enumerate the caller's namespaces with row counts. Ordered by count
/// desc — the agent / operator can use this to discover what is in
/// memory before issuing a targeted `recall`.
pub fn namespaces() -> Result<Vec<NamespaceCount>> {
    let cfg = RuntimeConfig::load()?;
    ensure_memory_available(&cfg)?;
    store::list_namespaces()
}

// ---------- Helpers ----------

/// Reject the call early when the memory layer is disabled or pgvector is
/// absent. Cheap; runs once per public call.
///
/// H1 (v0.5.2 review): also runs `ask._memory_bootstrap()` so the
/// `_memories` table exists even when pgvector was installed AFTER
/// `CREATE EXTENSION pg_ask`. The bootstrap helper is idempotent and
/// fast-paths out as soon as the table is present; the typical hot
/// `recall()` only pays for a single pg_class lookup inside the helper.
fn ensure_memory_available(cfg: &RuntimeConfig) -> Result<()> {
    if !cfg.memory_enabled {
        return Err(AskError::Sql(
            "pg_ask.memory_enabled is off — SET LOCAL pg_ask.memory_enabled = on".into(),
        ));
    }
    // The bootstrap helper returns false if pgvector isn't installed,
    // so we can use a single SPI roundtrip to cover both the
    // "pgvector missing" and the "table missing" cases. We still
    // surface a distinct error message for missing pgvector because the
    // operator action is different (install the extension vs. nothing).
    let bootstrapped: Option<bool> =
        pgrx::Spi::get_one("SELECT ask._memory_bootstrap()")
            .map_err(|e| AskError::Sql(format!("_memory_bootstrap: {e}")))?;
    if !bootstrapped.unwrap_or(false) {
        return Err(AskError::Sql(
            "memory layer requires pgvector — run `CREATE EXTENSION vector;` first".into(),
        ));
    }
    Ok(())
}
