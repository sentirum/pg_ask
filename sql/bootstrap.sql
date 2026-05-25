-- pg_ask bootstrap schema.
-- Loaded by the extension at CREATE EXTENSION time.

-- Create the install schema explicitly. With no `schema = '…'` in
-- pg_ask.control, Postgres treats the first object we create here as
-- the extension's owned schema and binds it accordingly. This must run
-- BEFORE any qualified `ask.<table>` reference below, otherwise the
-- catalog has nowhere to put the relation.
CREATE SCHEMA IF NOT EXISTS ask;

-- Configuration key/value store. API keys live here; revoke usage on the
-- schema for least-privileged roles in production.
CREATE TABLE IF NOT EXISTS ask._config (
    key        text PRIMARY KEY,
    value      text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- Session log. One row per multi-turn conversation. `owner` is captured
-- at create-time and every chat() / clear_session() call checks it against
-- current_user so sessions cannot leak across roles.
CREATE TABLE IF NOT EXISTS ask._sessions (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    owner      name        NOT NULL DEFAULT current_user,
    label      text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX IF NOT EXISTS _sessions_owner_idx
    ON ask._sessions (owner, updated_at DESC);

CREATE TABLE IF NOT EXISTS ask._messages (
    session_id uuid NOT NULL REFERENCES ask._sessions(id) ON DELETE CASCADE,
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
CREATE TABLE IF NOT EXISTS ask._traces (
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
CREATE INDEX IF NOT EXISTS _traces_ts_idx     ON ask._traces (ts DESC);
CREATE INDEX IF NOT EXISTS _traces_caller_idx ON ask._traces (caller, ts DESC);

-- The writer takes a single jsonb payload so the Rust side never has to
-- learn the column order. SECURITY DEFINER lets ordinary roles produce
-- trace rows without holding INSERT on _traces directly.
CREATE OR REPLACE FUNCTION ask._write_trace(payload jsonb)
RETURNS uuid
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    INSERT INTO ask._traces
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
-- in ask.control because most deployments don't need memory and we don't
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
        CREATE TABLE IF NOT EXISTS ask._memories (
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
            ON ask._memories (owner, namespace, created_at DESC);
        -- Full-text rank for the BM25-ish half of the hybrid score.
        CREATE INDEX IF NOT EXISTS _memories_tsv_idx
            ON ask._memories USING gin (tsv);
        -- ANN index. IVFFlat (cosine) keeps build time low and matches the
        -- single-tenant scale most pg_ask deployments will have; operators
        -- with millions of rows can DROP+REINDEX to HNSW after the fact.
        BEGIN
            CREATE INDEX IF NOT EXISTS _memories_embedding_idx
                ON ask._memories USING ivfflat (embedding vector_cosine_ops)
                WITH (lists = 100);
        EXCEPTION WHEN feature_not_supported THEN
            -- Older pgvector builds without ivfflat fall back to no ANN index;
            -- the cosine search still works, just sequentially.
            NULL;
        END;
        -- No direct DML grant: writes go through ask._memory_insert /
        -- ask._memory_delete_owned (SECURITY DEFINER), which enforce the
        -- owner predicate. SELECT remains GRANT-controlled by the
        -- blanket statement below + the optional RLS policy. See C3 in
        -- docs/SECURITY.md.
        GRANT SELECT ON ask._memories TO PUBLIC;
        -- Defense-in-depth: row-level policy so even a future grant
        -- typo can't expose another role's memories. We enable RLS
        -- *and* a permissive policy keyed on session_user; superusers
        -- bypass RLS by default (FORCE not used).
        ALTER TABLE ask._memories ENABLE ROW LEVEL SECURITY;
        DROP POLICY IF EXISTS _memories_owner_select ON ask._memories;
        CREATE POLICY _memories_owner_select ON ask._memories
            FOR SELECT USING (owner = session_user);
    END IF;
END
$bootstrap_memory$;

-- User-defined tools registry. Operators register SQL snippets that the
-- agent can invoke like built-in tools. Body is a SQL statement template
-- with `{{key}}` placeholders replaced from the tool's jsonb arguments at
-- invocation time. Only the owner (or a superuser) can delete a row.
CREATE TABLE IF NOT EXISTS ask._tools (
    name       text        PRIMARY KEY,
    owner      name        NOT NULL DEFAULT current_user,
    spec       jsonb       NOT NULL,
    body       text        NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);

-- SQL audit log. One row per sql_query / sample_table execution.
-- Written directly by the tool so operators can trace exactly what
-- the model asked the database to do, when, and with what result.
CREATE TABLE IF NOT EXISTS ask._sql_audit (
    id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    ts         timestamptz NOT NULL DEFAULT now(),
    caller     name        NOT NULL DEFAULT current_user,
    db         name        NOT NULL DEFAULT current_database(),
    query      text        NOT NULL,
    row_count  int,
    error      text,
    readonly   bool        NOT NULL,
    tool_name  text        NOT NULL
);
CREATE INDEX IF NOT EXISTS _sql_audit_ts_idx     ON ask._sql_audit (ts DESC);
CREATE INDEX IF NOT EXISTS _sql_audit_caller_idx ON ask._sql_audit (caller, ts DESC);

-- ---------------------------------------------------------------------------
-- SECURITY DEFINER helpers for internal-table writes.
--
-- Background (C3 in the v0.5.2 review): we previously granted
-- INSERT/UPDATE/DELETE on _memories / _tools / _sql_audit straight to
-- PUBLIC and relied on every code path to add a `WHERE owner =
-- current_user` filter. That meant a single missed predicate — or any
-- other role with USAGE on the `ask` schema and direct table access —
-- could read/write rows owned by anyone else. We now lock the tables
-- down to SELECT-only and funnel every write through one of these
-- helpers, each of which stamps `session_user` as the row's owner and
-- (where applicable) checks ownership on update/delete.
--
-- Why `session_user` and not `current_user`? Inside a SECURITY DEFINER
-- body Postgres sets current_user to the function *owner* (typically
-- the extension superuser); session_user always reflects the original
-- connecting role, which is what we want to bill the row to. The same
-- caveat applies to _write_trace above — see H4 in the review for the
-- planned fix there.
-- ---------------------------------------------------------------------------

-- Insert audit row for a tool execution. Always succeeds (audit must
-- not block the user's query). Returns the new id.
CREATE OR REPLACE FUNCTION ask._sql_audit_insert(
    query     text,
    row_count int,
    readonly  bool,
    tool_name text
) RETURNS uuid
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    INSERT INTO ask._sql_audit (caller, query, row_count, readonly, tool_name)
    VALUES (session_user, query, row_count, readonly, tool_name)
    RETURNING id;
$$;

-- Insert a memory row owned by the calling role. The Rust caller passes
-- the embedding as a text-encoded vector literal so we don't have to
-- mention the pgvector type in this function's signature — keeps the
-- helper installable when pgvector is missing (the function body simply
-- ERRORs at runtime, which is also what the previous code path did).
CREATE OR REPLACE FUNCTION ask._memory_insert(
    namespace text,
    content   text,
    metadata  jsonb,
    embedding text
) RETURNS uuid
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    new_id uuid;
BEGIN
    EXECUTE
        'INSERT INTO ask._memories (owner, namespace, content, metadata, embedding) '
     || 'VALUES (session_user, $1, $2, $3, $4::vector) RETURNING id'
    INTO new_id
    USING namespace, content, metadata, embedding;
    RETURN new_id;
END
$$;

-- Delete one memory row by id, only if it's owned by the calling role.
-- Returns true if a row was deleted, false otherwise. The Rust caller
-- can't distinguish "not found" from "not yours" (deliberate — same
-- NotFound==Unauthorized collapse the higher layer relies on).
CREATE OR REPLACE FUNCTION ask._memory_delete_owned(memory_id uuid)
RETURNS boolean
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n int;
BEGIN
    EXECUTE 'DELETE FROM ask._memories WHERE id = $1 AND owner = session_user'
        USING memory_id;
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n > 0;
END
$$;

-- Upsert a user-defined tool. Owner is stamped to session_user on
-- INSERT; on conflict we update spec/body only if the existing row is
-- owned by the caller, otherwise raise.
CREATE OR REPLACE FUNCTION ask._tool_register(
    tool_name text,
    spec      jsonb,
    body      text
) RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    existing_owner name;
BEGIN
    SELECT t.owner INTO existing_owner
        FROM ask._tools t WHERE t.name = tool_name;
    IF existing_owner IS NOT NULL AND existing_owner <> session_user THEN
        RAISE EXCEPTION 'tool % is owned by %, cannot overwrite',
            tool_name, existing_owner USING ERRCODE = 'insufficient_privilege';
    END IF;
    INSERT INTO ask._tools (name, owner, spec, body)
    VALUES (tool_name, session_user, spec, body)
    ON CONFLICT (name) DO UPDATE
        SET spec = EXCLUDED.spec,
            body = EXCLUDED.body,
            updated_at = now();
END
$$;

-- Delete a user-defined tool by name, only if owned by the caller.
CREATE OR REPLACE FUNCTION ask._tool_unregister(tool_name text)
RETURNS boolean
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n int;
BEGIN
    DELETE FROM ask._tools
        WHERE name = tool_name AND owner = session_user;
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n > 0;
END
$$;

-- ---------------------------------------------------------------------------
-- Grant policy.
--
-- Default: REVOKE everything from PUBLIC, then GRANT SELECT only on
-- the audit / registry views and EXECUTE on the writer helpers.
--
-- Operators who want a stricter posture (e.g. hide _sql_audit from
-- non-operators) should layer their own REVOKE after CREATE EXTENSION.
-- ---------------------------------------------------------------------------
REVOKE ALL ON ALL TABLES IN SCHEMA ask FROM PUBLIC;

-- Read-only public surface. _memories is granted conditionally because
-- the table only exists when pgvector is installed.
GRANT SELECT ON ask._traces    TO PUBLIC;
GRANT SELECT ON ask._tools     TO PUBLIC;
GRANT SELECT ON ask._sql_audit TO PUBLIC;
DO $grant_memory_select$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_class
                WHERE relnamespace = 'ask'::regnamespace
                  AND relname = '_memories') THEN
        EXECUTE 'GRANT SELECT ON ask._memories TO PUBLIC';
    END IF;
END
$grant_memory_select$;

-- Writer helpers — the only INSERT/UPDATE/DELETE path for internal tables.
-- Each enforces session_user ownership inside its body, so EXECUTE to
-- PUBLIC is safe.
REVOKE ALL ON FUNCTION ask._write_trace(jsonb)                            FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._sql_audit_insert(text, int, bool, text)       FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._memory_insert(text, text, jsonb, text)        FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._memory_delete_owned(uuid)                     FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._tool_register(text, jsonb, text)              FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._tool_unregister(text)                         FROM PUBLIC;
GRANT EXECUTE ON FUNCTION ask._write_trace(jsonb)                         TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._sql_audit_insert(text, int, bool, text)    TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._memory_insert(text, text, jsonb, text)     TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._memory_delete_owned(uuid)                  TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._tool_register(text, jsonb, text)           TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._tool_unregister(text)                      TO PUBLIC;

-- Config-surface lockdown (C6) lives in a finalize SQL block in
-- `sql/finalize.sql` because pgrx emits the #[pg_extern] config
-- functions *after* this bootstrap script runs.
