-- pg_ask 0.5.1 → 0.5.2 upgrade.
--
-- Pure-hardening release. No new public surface; the script reapplies
-- the SECURITY-DEFINER writer helpers (bodies identical to v0.5.2
-- bootstrap.sql), the REVOKE/GRANT lockdown, and the
-- _sql_audit.latency_ms column. Everything is idempotent so a partial
-- run can be retried without ill effects.
--
-- Notes on what is intentionally NOT in this script:
--
-- * Rust-side fixes (subtxn isolation, SET LOCAL leak fix,
--   RuntimeConfig single-load, schema cache, HttpClient pool,
--   stream truncation, parser-authoritative sql_guard) ship in the
--   new .so. They take effect the moment ALTER EXTENSION UPDATE
--   swaps the library; no SQL migration is required.
-- * pgrx auto-regenerates the public ask.* function signatures from
--   the #[pg_extern] annotations. Whenever a signature changes pgrx
--   emits the DROP/CREATE pair as part of its generated bridge SQL,
--   not here. The 0.5.2 release does not change any public signature.
--
-- Parameter NAMES of the writer helpers are pinned to the v0.5.2
-- bootstrap shape; v0.5.0/v0.5.1 used the same names. If you forked
-- a helper locally and renamed a parameter, the CREATE OR REPLACE
-- will fail with ERRCODE 42P13 ("cannot change name of input
-- parameter"); resolve by hand-dropping your fork before running the
-- upgrade (you may need to ALTER EXTENSION pg_ask DROP FUNCTION
-- first because of the extension dependency).

-- ---------------------------------------------------------------------------
-- 1. _sql_audit.latency_ms (H3 review item: wall time from audit insert
--    to result render, in milliseconds). Existing rows stay NULL.

ALTER TABLE IF EXISTS ask._sql_audit
    ADD COLUMN IF NOT EXISTS latency_ms bigint;

-- ---------------------------------------------------------------------------
-- 2. SECURITY DEFINER writer helpers (C2). Bodies are byte-identical to
--    v0.5.2 sql/bootstrap.sql; if you change one, change both.
--
--    Each helper pins `search_path = pg_catalog, pg_temp` so a caller
--    who has SET search_path = malicious_schema cannot redirect table
--    writes elsewhere. Each enforces session_user ownership inside the
--    body, so EXECUTE-to-PUBLIC (re-granted in section 4) is safe.

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
        COALESCE(payload->>'caller',     session_user),
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

CREATE OR REPLACE FUNCTION ask._sql_audit_finish(
    audit_id    uuid,
    latency_ms  bigint,
    err_message text
) RETURNS boolean
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n int;
BEGIN
    -- Parameter / column name collisions resolved by passing every
    -- value through USING on an EXECUTE — simpler than fully
    -- qualifying every reference.
    EXECUTE
        'UPDATE ask._sql_audit '
     || 'SET row_count  = CASE WHEN $3 IS NULL THEN 0 ELSE row_count END, '
     || '    error      = $3, '
     || '    latency_ms = $2 '
     || 'WHERE id = $1 AND caller = session_user'
    USING audit_id, latency_ms, err_message;
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n > 0;
END
$$;

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
-- 3. Tighten table-level grants (C2). Internal tables get SELECT-to-PUBLIC
--    only where it's already the documented contract; writes go strictly
--    through the helpers above.

REVOKE ALL ON ask._config    FROM PUBLIC;
REVOKE ALL ON ask._sessions  FROM PUBLIC;
REVOKE ALL ON ask._messages  FROM PUBLIC;
REVOKE ALL ON ask._traces    FROM PUBLIC;
REVOKE ALL ON ask._tools     FROM PUBLIC;
REVOKE ALL ON ask._sql_audit FROM PUBLIC;

DO $revoke_memory$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_class
                WHERE relnamespace = 'ask'::regnamespace
                  AND relname = '_memories') THEN
        EXECUTE 'REVOKE ALL ON ask._memories FROM PUBLIC';
        EXECUTE 'GRANT SELECT ON ask._memories TO PUBLIC';
    END IF;
END
$revoke_memory$;

GRANT SELECT ON ask._traces    TO PUBLIC;
GRANT SELECT ON ask._tools     TO PUBLIC;
GRANT SELECT ON ask._sql_audit TO PUBLIC;

-- ---------------------------------------------------------------------------
-- 4. Re-grant EXECUTE on the writer helpers to PUBLIC. The bodies enforce
--    session_user ownership; this is the only DML path users have.

REVOKE ALL ON FUNCTION ask._write_trace(jsonb)                            FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._sql_audit_insert(text, int, bool, text)       FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._sql_audit_finish(uuid, bigint, text)          FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._memory_insert(text, text, jsonb, text)        FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._memory_delete_owned(uuid)                     FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._tool_register(text, jsonb, text)              FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._tool_unregister(text)                         FROM PUBLIC;

GRANT EXECUTE ON FUNCTION ask._write_trace(jsonb)                         TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._sql_audit_insert(text, int, bool, text)    TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._sql_audit_finish(uuid, bigint, text)       TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._memory_insert(text, text, jsonb, text)     TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._memory_delete_owned(uuid)                  TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._tool_register(text, jsonb, text)           TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._tool_unregister(text)                      TO PUBLIC;
