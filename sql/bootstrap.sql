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

-- Lock down internals by default. Users get the public-facing functions via
-- explicit GRANT in their setup script.
REVOKE ALL ON ALL TABLES IN SCHEMA pg_ask FROM PUBLIC;
