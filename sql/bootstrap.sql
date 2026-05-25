-- pg_ask bootstrap schema.
-- Loaded by the extension at CREATE EXTENSION time.

CREATE SCHEMA IF NOT EXISTS pg_ask;

-- Configuration key/value store. API keys live here; revoke usage on the
-- schema for least-privileged roles in production.
CREATE TABLE IF NOT EXISTS pg_ask._config (
    key        text PRIMARY KEY,
    value      text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Session log. One row per multi-turn conversation. `owner` is captured
-- at create-time and every chat() / clear_session() call checks it against
-- current_user so sessions cannot leak across roles.
CREATE TABLE IF NOT EXISTS pg_ask._sessions (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    owner      name        NOT NULL DEFAULT current_user,
    label      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS _sessions_owner_idx
    ON pg_ask._sessions (owner, updated_at DESC);

CREATE TABLE IF NOT EXISTS pg_ask._messages (
    session_id uuid NOT NULL REFERENCES pg_ask._sessions(id) ON DELETE CASCADE,
    idx        int  NOT NULL,
    role       text NOT NULL CHECK (role IN ('system','user','assistant','tool')),
    content    text NOT NULL,
    tool_calls jsonb,
    tool_call_id text,
    is_error   bool,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (session_id, idx)
);

-- Audit / trace log. One row per ask() / sql() / preview() / chat() call.
-- Read by operators, written only via the SECURITY DEFINER helper below so
-- non-owner callers (who lack INSERT) can still produce trace rows.
CREATE TABLE IF NOT EXISTS pg_ask._traces (
    id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    ts           timestamptz NOT NULL DEFAULT now(),
    caller       name        NOT NULL DEFAULT current_user,
    db           name        NOT NULL DEFAULT current_database(),
    kind         text        NOT NULL CHECK (kind IN ('ask','sql','preview','chat')),
    question     text        NOT NULL,
    iterations   int         NOT NULL DEFAULT 0,
    tool_calls   jsonb       NOT NULL DEFAULT '[]'::jsonb,
    final_text   text,
    provider     text,
    model        text,
    latency_ms   int,
    error        text
);
CREATE INDEX IF NOT EXISTS _traces_ts_idx     ON pg_ask._traces (ts DESC);
CREATE INDEX IF NOT EXISTS _traces_caller_idx ON pg_ask._traces (caller, ts DESC);

-- The writer takes a single jsonb payload so the Rust side never has to
-- learn the column order. SECURITY DEFINER lets ordinary roles produce
-- trace rows without holding INSERT on _traces directly.
CREATE OR REPLACE FUNCTION pg_ask._write_trace(payload jsonb)
RETURNS uuid
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    INSERT INTO pg_ask._traces
        (caller, kind, question, iterations, tool_calls,
         final_text, provider, model, latency_ms, error)
    VALUES (
        COALESCE(payload->>'caller',     current_user),
        payload->>'kind',
        COALESCE(payload->>'question',   ''),
        COALESCE((payload->>'iterations')::int, 0),
        COALESCE(payload->'tool_calls',  '[]'::jsonb),
        payload->>'final_text',
        payload->>'provider',
        payload->>'model',
        NULLIF(payload->>'latency_ms', '')::int,
        payload->>'error'
    )
    RETURNING id;
$$;

-- ---------------------------------------------------------------------------
-- Long-term memory (requires the `vector` extension).
--
-- We detect pgvector at install time rather than declaring a hard dependency
-- in pg_ask.control because most deployments don't need memory and we don't
-- want to force the operator to install pgvector for the chat / preview
-- surface. If pgvector is missing, `_memories` is simply not created and the
-- memory.* SQL surface returns a clean error pointing the operator at
-- `CREATE EXTENSION vector`.
--
-- Embedding dimension is fixed at 1536 (OpenAI text-embedding-3-small,
-- Gemini text-embedding-004). Operators using larger models must ALTER
-- the column after install — see docs/SECURITY.md.
-- ---------------------------------------------------------------------------
DO $bootstrap_memory$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector') THEN
        CREATE TABLE IF NOT EXISTS pg_ask._memories (
            id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
            owner      name        NOT NULL DEFAULT current_user,
            namespace  text        NOT NULL DEFAULT 'default',
            content    text        NOT NULL,
            metadata   jsonb       NOT NULL DEFAULT '{}'::jsonb,
            embedding  vector(1536) NOT NULL,
            tsv        tsvector    GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED,
            created_at timestamptz NOT NULL DEFAULT now()
        );
        -- Ownership / namespace listing index.
        CREATE INDEX IF NOT EXISTS _memories_owner_ns_idx
            ON pg_ask._memories (owner, namespace, created_at DESC);
        -- Full-text rank for the BM25-ish half of the hybrid score.
        CREATE INDEX IF NOT EXISTS _memories_tsv_idx
            ON pg_ask._memories USING gin (tsv);
        -- ANN index. IVFFlat (cosine) keeps build time low and matches the
        -- single-tenant scale most pg_ask deployments will have; operators
        -- with millions of rows can DROP+REINDEX to HNSW after the fact.
        BEGIN
            CREATE INDEX IF NOT EXISTS _memories_embedding_idx
                ON pg_ask._memories USING ivfflat (embedding vector_cosine_ops)
                WITH (lists = 100);
        EXCEPTION WHEN feature_not_supported THEN
            -- Older pgvector builds without ivfflat fall back to no ANN index;
            -- the cosine search still works, just sequentially.
            NULL;
        END;
        GRANT SELECT, INSERT, UPDATE, DELETE ON pg_ask._memories TO PUBLIC;
    END IF;
END
$bootstrap_memory$;

-- Lock down internals by default. Users get the public-facing functions via
-- explicit GRANT in their setup script. _traces stays readable so operators
-- can audit without extra grants; the writer above is the only INSERT path.
REVOKE ALL ON ALL TABLES IN SCHEMA pg_ask FROM PUBLIC;
GRANT  SELECT  ON pg_ask._traces TO PUBLIC;
REVOKE ALL ON FUNCTION pg_ask._write_trace(jsonb) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION pg_ask._write_trace(jsonb) TO PUBLIC;

-- Memory-table grants must be re-applied AFTER the blanket REVOKE above so
-- ownership-checked memory.* functions can read/write on behalf of the caller.
-- (The `WHERE owner = current_user` clause in every query is what actually
-- prevents cross-tenant access; the GRANT is just "PostgreSQL, please let
-- the row-level predicate decide".)
DO $memory_grants$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_class
                WHERE relnamespace = 'pg_ask'::regnamespace
                  AND relname = '_memories') THEN
        EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON pg_ask._memories TO PUBLIC';
    END IF;
END
$memory_grants$;
