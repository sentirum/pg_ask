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
//! `pg_ask._memories` was not created during bootstrap), every entry
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
    ensure_memory_available(&cfg)?;

    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = embeddings::build(&cfg, http)?;

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
    ensure_memory_available(&cfg)?;

    let http = HttpClient::new(cfg.http_connect_timeout_ms, cfg.http_total_timeout_ms);
    let provider = embeddings::build(&cfg, http)?;

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
fn ensure_memory_available(cfg: &RuntimeConfig) -> Result<()> {
    if !cfg.memory_enabled {
        return Err(AskError::Sql(
            "pg_ask.memory_enabled is off — SET LOCAL pg_ask.memory_enabled = on".into(),
        ));
    }
    if !store::pgvector_installed()? {
        return Err(AskError::Sql(
            "memory layer requires pgvector — run `CREATE EXTENSION vector;` and \
             reload pg_ask"
                .into(),
        ));
    }
    Ok(())
}
