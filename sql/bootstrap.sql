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

-- Session log (reserved for v0.2 multi-turn).
CREATE TABLE IF NOT EXISTS pg_ask._sessions (
    id         uuid PRIMARY KEY,
    created_at timestamptz NOT NULL DEFAULT now(),
    label      text
);

CREATE TABLE IF NOT EXISTS pg_ask._messages (
    session_id uuid NOT NULL REFERENCES pg_ask._sessions(id) ON DELETE CASCADE,
    idx        int  NOT NULL,
    role       text NOT NULL CHECK (role IN ('system','user','assistant','tool')),
    content    text NOT NULL,
    tool_calls jsonb,
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

-- Lock down internals by default. Users get the public-facing functions via
-- explicit GRANT in their setup script. _traces stays readable so operators
-- can audit without extra grants; the writer above is the only INSERT path.
REVOKE ALL ON ALL TABLES IN SCHEMA pg_ask FROM PUBLIC;
GRANT  SELECT  ON pg_ask._traces TO PUBLIC;
REVOKE ALL ON FUNCTION pg_ask._write_trace(jsonb) FROM PUBLIC;
GRANT  EXECUTE ON FUNCTION pg_ask._write_trace(jsonb) TO PUBLIC;
