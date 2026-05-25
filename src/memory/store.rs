//! SPI primitives backing `memory/mod.rs`.
//!
//! Every read or write is parameterised. `embedding` vectors are passed
//! to pgvector as a text literal (`'[0.1,0.2,...]'`) cast with `::vector`
//! inside the SQL — that avoids needing pgvector's binary protocol
//! definition on the Rust side and works against every pgvector version.
//!
//! The `WHERE owner = current_user` predicate is repeated on **every**
//! row-touching query; we never trust the caller to supply an owner.

use crate::infra::errors::{AskError, Result};
use pgrx::prelude::*;
use pgrx::Uuid;
use serde_json::Value;

/// One row from `ask.list_memories()` — caller-visible admin view.
#[derive(Debug, Clone)]
pub struct MemoryRow {
    pub id: Uuid,
    pub namespace: String,
    pub content: String,
    pub metadata: Value,
    pub created_at_iso: String,
}

/// One row from `ask.list_namespaces()` — namespace + row count.
#[derive(Debug, Clone)]
pub struct NamespaceCount {
    pub namespace: String,
    pub n: i64,
}

/// A single recall result. `similarity` is the hybrid score (higher is
/// better, scaled into roughly `[0, 1]`).
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: Uuid,
    pub content: String,
    pub metadata: Value,
    pub similarity: f64,
}

/// Detect whether pgvector is installed in the current database. Used by
/// the memory entry points so we can fail fast with an operator-friendly
/// message instead of bouncing off a missing type / table.
///
/// P3 (v0.5.2 review): the agent loop runs this on every iteration to
/// decide whether to include the recall tool. Each call is a fresh SPI
/// round-trip into pg_extension; in a tight agent loop that's pure
/// waste because pgvector can't be uninstalled inside a transaction
/// (DROP EXTENSION takes AccessExclusiveLock and the agent is itself
/// holding the connection open). We memoize a `true` answer for the
/// lifetime of the backend.
///
/// We deliberately do NOT memoize a `false` answer: the operator may
/// `CREATE EXTENSION vector;` mid-session and then call recall, and
/// caching `false` would force them to reconnect for the change to
/// take effect. The probe is cheap (single pg_catalog hit), and the
/// expected steady state once memory is in use is `true` — which IS
/// cached.
pub fn pgvector_installed() -> Result<bool> {
    use std::sync::atomic::{AtomicBool, Ordering};
    static INSTALLED: AtomicBool = AtomicBool::new(false);
    if INSTALLED.load(Ordering::Relaxed) {
        return Ok(true);
    }
    let found: Option<bool> = Spi::get_one(
        "SELECT TRUE FROM pg_extension WHERE extname = 'vector'",
    )?;
    let v = found.unwrap_or(false);
    if v {
        INSTALLED.store(true, Ordering::Relaxed);
    }
    Ok(v)
}

pub fn insert(
    content: &str,
    namespace: &str,
    metadata: Value,
    embedding: &[f32],
) -> Result<Uuid> {
    let embedding_lit = encode_vector(embedding);
    let metadata_text = metadata.to_string();

    // Route through the SECURITY DEFINER helper rather than direct
    // INSERT: PUBLIC no longer holds INSERT on ask._memories (see C3 in
    // the v0.5.2 review and the corresponding bootstrap.sql comment).
    // The helper stamps owner = session_user and is the only INSERT
    // path into the table.
    let id: Option<Uuid> = Spi::get_one_with_args(
        "SELECT ask._memory_insert($1, $2, $3::jsonb, $4)",
        &[
            namespace.into(),
            content.into(),
            metadata_text.as_str().into(),
            embedding_lit.as_str().into(),
        ],
    )?;
    id.ok_or_else(|| AskError::Sql("_memory_insert returned no id".into()))
}

/// List every namespace the caller has ever stored something under,
/// plus a row count per namespace. Ordered by count desc, then name.
pub fn list_namespaces() -> Result<Vec<NamespaceCount>> {
    let mut out: Vec<NamespaceCount> = Vec::new();
    Spi::connect(|client| -> Result<()> {
        let rows = client.select(
            "SELECT namespace, COUNT(*)::bigint AS n
               FROM ask._memories
              WHERE owner = current_user
              GROUP BY namespace
              ORDER BY n DESC, namespace ASC",
            None,
            &[],
        )?;
        for row in rows {
            let ns: String = row
                .get_datum_by_ordinal(1)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let n: i64 = row
                .get_datum_by_ordinal(2)
                .ok()
                .and_then(|d| d.value::<i64>().ok().flatten())
                .unwrap_or(0);
            out.push(NamespaceCount { namespace: ns, n });
        }
        Ok(())
    })?;
    Ok(out)
}

/// Browse the caller's memories. Owner-scoped; ordered newest-first.
/// `limit` is clamped to `[1, 200]`; `offset` to `>= 0`.
pub fn list_memories(
    namespace: Option<&str>,
    limit: usize,
    offset: usize,
) -> Result<Vec<MemoryRow>> {
    let mut out: Vec<MemoryRow> = Vec::new();
    let limit_i32 = i32::try_from(limit.clamp(1, 200)).unwrap_or(50);
    let offset_i32 = i32::try_from(offset).unwrap_or(0).max(0);

    // The optional namespace filter is folded into SQL via a NULL trick
    // so we keep one prepared shape regardless of caller intent.
    //
    // P6 (v0.5.2 review): `id` is selected as its native `uuid` type,
    // not `::text`. pgrx decodes it directly into [`pgrx::Uuid`] via
    // the postgres binary representation — strictly fewer allocations
    // than the previous round-trip through a 36-char hyphenated
    // string + manual hex parser. Same applies to `hybrid_search`
    // below.
    let query = "
        SELECT id,
               namespace,
               content,
               metadata::text,
               to_char(created_at AT TIME ZONE 'UTC',
                       'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_iso
          FROM ask._memories
         WHERE owner = current_user
           AND ($1::text IS NULL OR namespace = $1::text)
         ORDER BY created_at DESC
         LIMIT $2::int OFFSET $3::int
    ";

    Spi::connect(|client| -> Result<()> {
        let rows = client.select(
            query,
            None,
            &[
                namespace.into(),
                limit_i32.into(),
                offset_i32.into(),
            ],
        )?;
        for row in rows {
            let id: Uuid = match row
                .get_datum_by_ordinal(1)
                .ok()
                .and_then(|d| d.value::<Uuid>().ok().flatten())
            {
                Some(u) => u,
                None => continue, // null id shouldn't happen (PK), but stay defensive
            };
            let ns: String = row
                .get_datum_by_ordinal(2)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let content: String = row
                .get_datum_by_ordinal(3)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let metadata_text: String = row
                .get_datum_by_ordinal(4)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_else(|| "{}".into());
            let created_iso: String = row
                .get_datum_by_ordinal(5)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();

            let metadata: Value =
                serde_json::from_str(&metadata_text).unwrap_or(Value::Null);
            out.push(MemoryRow {
                id,
                namespace: ns,
                content,
                metadata,
                created_at_iso: created_iso,
            });
        }
        Ok(())
    })?;
    Ok(out)
}

pub fn delete(id: Uuid) -> Result<bool> {
    // Route through the SECURITY DEFINER helper. It runs the
    // owner-checked DELETE itself and returns whether a row was
    // removed; we can't distinguish "wrong owner" from "doesn't exist",
    // which is the NotFound-collapse the higher layer relies on.
    let deleted: Option<bool> =
        Spi::get_one_with_args("SELECT ask._memory_delete_owned($1)", &[id.into()])?;
    Ok(deleted.unwrap_or(false))
}

/// Hybrid search: vector cosine + full-text rank blended by `alpha`.
///
/// We compute both signals in SQL — cheaper than streaming a candidate
/// set through Rust and re-ranking. `ts_rank_cd` is normalised by
/// `1 / (1 + rank)` so it sits in `[0, 1)` alongside cosine similarity.
pub fn hybrid_search(
    embedding: &[f32],
    query_text: &str,
    namespace: &str,
    limit: usize,
    alpha: f64,
) -> Result<Vec<Hit>> {
    let embedding_lit = encode_vector(embedding);
    let mut out: Vec<Hit> = Vec::new();

    // SQL notes:
    //   - `1 - (embedding <=> q)` turns cosine *distance* into a similarity
    //      in [0,1] (assuming non-zero vectors, which OpenAI/Voyage/Gemini
    //      always return).
    //   - `plainto_tsquery('simple', ...)` mirrors the generated `tsv`
    //      column's analyser so token boundaries align.
    //   - Limit is clamped via min(limit, 100) — protects against the
    //      agent asking for thousands of rows.
    //
    // H9 (v0.5.2 review): the previous single-stage query did
    //     ORDER BY (alpha * cosine + (1-alpha) * ts_rank_cd) DESC
    // which the planner cannot satisfy with the ivfflat ANN index —
    // ivfflat only accelerates `ORDER BY embedding <=> q LIMIT N`, not
    // an arbitrary expression. The result was a full sequential scan +
    // sort over `_memories`, which becomes painful past ~10k rows.
    //
    // Fix: two-stage. Stage 1 uses the ANN index to pull an over-fetch
    // candidate set (limit * 5, with a floor of 50 so a tiny limit
    // still benefits from FTS). Stage 2 reranks those candidates by
    // the blended score. The candidate set is still owner/namespace
    // filtered so we don't waste ANN slots on rows we can't return.
    //
    // The over-fetch multiplier is a heuristic: too low and FTS-only
    // matches at high alpha (low FTS weight) drop out; too high and
    // stage 1 stops being meaningfully cheaper than a full scan. 5x
    // is the same ratio Weaviate/Qdrant use by default for hybrid;
    // operators with adversarial recall workloads can ALTER the
    // multiplier later via a GUC (planned but not in this fix).
    let query = "
        WITH q AS (
            SELECT $1::vector AS vec,
                   plainto_tsquery('simple', $2::text) AS tsq
        ),
        cand AS (
            -- Stage 1: ANN over-fetch. Order-by uses the cosine
            -- distance operator alone so the ivfflat index can serve
            -- it; we pull GREATEST(limit*5, 50) rows so the rerank in
            -- stage 2 has enough signal even at small limits.
            SELECT m.id, m.content, m.metadata, m.embedding, m.tsv
              FROM ask._memories m
             WHERE m.owner = current_user
               AND m.namespace = $4::text
             ORDER BY m.embedding <=> (SELECT vec FROM q)
             LIMIT GREATEST($5::int * 5, 50)
        )
        SELECT c.id,
               c.content,
               c.metadata::text,
               (
                   ($3::float8) * (1 - (c.embedding <=> (SELECT vec FROM q)))
                 + (1 - $3::float8) * (1.0 / (1.0 + COALESCE(ts_rank_cd(c.tsv, (SELECT tsq FROM q)), 0)))
               ) AS score
          FROM cand c
         ORDER BY score DESC
         LIMIT LEAST($5::int, 100)
    ";

    Spi::connect(|client| -> Result<()> {
        let limit_i32 = i32::try_from(limit).unwrap_or(5);
        let rows = client.select(
            query,
            None,
            &[
                embedding_lit.as_str().into(),
                query_text.into(),
                alpha.into(),
                namespace.into(),
                limit_i32.into(),
            ],
        )?;
        for row in rows {
            // P6 (v0.5.2 review): native uuid decode, no ::text + parse.
            let id: Uuid = match row
                .get_datum_by_ordinal(1)
                .ok()
                .and_then(|d| d.value::<Uuid>().ok().flatten())
            {
                Some(u) => u,
                None => continue,
            };
            let content: String = row
                .get_datum_by_ordinal(2)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_default();
            let metadata_text: String = row
                .get_datum_by_ordinal(3)
                .ok()
                .and_then(|d| d.value::<String>().ok().flatten())
                .unwrap_or_else(|| "{}".into());
            let score: f64 = row
                .get_datum_by_ordinal(4)
                .ok()
                .and_then(|d| d.value::<f64>().ok().flatten())
                .unwrap_or(0.0);
            let metadata: Value =
                serde_json::from_str(&metadata_text).unwrap_or_else(|_| Value::Null);
            out.push(Hit {
                id,
                content,
                metadata,
                similarity: score,
            });
        }
        Ok(())
    })?;

    Ok(out)
}

// ---------- Encoding helpers ----------

/// Render `[1,2,3,...]` in pgvector's text format. We use the minimal
/// shape with comma separators and no spaces — saves bytes on the wire
/// when a 1536-D vector is ~25 KB of text.
fn encode_vector(values: &[f32]) -> String {
    let mut out = String::with_capacity(values.len() * 8 + 2);
    out.push('[');
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        // `{:?}` on f32 round-trips; `to_string()` drops the trailing zero
        // on values like `0.1` which is fine for our purposes.
        out.push_str(&v.to_string());
    }
    out.push(']');
    out
}

// P6 (v0.5.2 review): the previous `parse_uuid` helper plus a manual
// hex parser is gone — every callsite now `SELECT id` (native uuid)
// and pulls the column with `.value::<Uuid>()`. The hex parser was
// only ever exercised against well-formed Postgres uuids and added
// allocation + cleanup that pgrx already does natively.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_vector_empty_is_brackets() {
        assert_eq!(encode_vector(&[]), "[]");
    }

    #[test]
    fn encode_vector_no_spaces_comma_separated() {
        let v = encode_vector(&[0.5_f32, 1.0, -2.25]);
        assert!(v.starts_with('['));
        assert!(v.ends_with(']'));
        assert!(!v.contains(' '));
        // pgvector accepts this exact shape: `[0.5,1,-2.25]`.
        // We don't pin the float formatting (Rust's `to_string`
        // round-trips f32 and we don't care about trailing zeros),
        // but we DO want the structural guarantees.
        let stripped = &v[1..v.len() - 1];
        assert_eq!(stripped.split(',').count(), 3);
    }

}
