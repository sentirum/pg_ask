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
--
-- H4 note: the table-level DEFAULTs intentionally stay on `current_user`,
-- not `session_user`. Rationale:
--   * SECURITY DEFINER helpers (ask._memory_insert, ask._tool_register, …)
--     always pass `session_user` explicitly, so the default is never
--     observed under a definer transition.
--   * Direct INSERTs from user sessions have current_user == session_user,
--     so the default value is the same either way.
--   * Operators who deliberately `SET ROLE` to act as another role expect
--     subsequent INSERTs to be attributed to the assumed role; only
--     `session_user` would override that, which would surprise them.
-- `_write_trace` is the one place where the choice matters at runtime
-- (payload-driven INSERT inside a SECURITY DEFINER body); that helper
-- uses session_user explicitly.
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
    error        text,
    -- P4 fix: token usage from provider response.
    prompt_tokens     int,
    completion_tokens int
);
CREATE INDEX IF NOT EXISTS _traces_ts_idx     ON ask._traces (ts DESC);
CREATE INDEX IF NOT EXISTS _traces_caller_idx ON ask._traces (caller, ts DESC);

-- S6 fix: _traces visibility is owner-scoped via RLS. Only the calling
-- role (recorded in `caller`) can see their own traces. Superusers bypass
-- RLS and see everything (standard PG behaviour).
ALTER TABLE ask._traces ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS _traces_owner_select ON ask._traces;
CREATE POLICY _traces_owner_select ON ask._traces
    FOR SELECT USING (caller = session_user);

-- The writer takes a single jsonb payload so the Rust side never has to
-- learn the column order. SECURITY DEFINER lets ordinary roles produce
-- trace rows without holding INSERT on _traces directly.
--
-- H4 (v0.5.2 review): the default caller is `session_user` rather than
-- `current_user`. Inside a SECURITY DEFINER body Postgres switches
-- current_user to the function owner (the extension superuser), so
-- using current_user here meant every trace row was attributed to the
-- definer regardless of who actually called ask.ask(). session_user
-- preserves the original connecting role even through SECURITY DEFINER
-- transitions. The Rust callers continue to override via
-- payload->>'caller' when they need to record a specific identity.
CREATE OR REPLACE FUNCTION ask._write_trace(payload jsonb)
RETURNS uuid
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    INSERT INTO ask._traces
        (caller, kind, question, iterations, tool_calls,
         final_text, provider, model, latency_ms, error,
         prompt_tokens, completion_tokens)
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
        payload->>'error',
        NULLIF(payload->>'prompt_tokens', '')::int,
        NULLIF(payload->>'completion_tokens', '')::int
    )
    RETURNING id;
$$;

-- ---------------------------------------------------------------------------
-- Long-term memory (requires the `vector` extension).
--
-- H1 (v0.5.2 review): the table create lives in a SECURITY DEFINER
-- helper that the Rust layer can also call lazily on first use. Before
-- this fix, `CREATE EXTENSION pg_ask` installed without pgvector
-- skipped the table create entirely, and a later `CREATE EXTENSION
-- vector` left the memory layer in a broken state: pgvector_installed()
-- returned true so ensure_memory_available() passed, but the first
-- INSERT then ERRORed with `relation ask._memories does not exist`.
--
-- Now the helper is idempotent (CREATE IF NOT EXISTS everywhere) and
-- gated on pgvector being present; bootstrap calls it once, and so
-- does every public memory entry point through `ensure_memory_table`.
--
-- S3 fix: the helper now accepts an explicit `dimensions` parameter
-- (defaults to 1536 for backward compatibility). This lets operators
-- use any embedding model without hand-editing the column type.
-- ---------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION ask._memory_bootstrap(dims int DEFAULT 1536)
RETURNS boolean
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    existing_dims int;
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector') THEN
        RETURN false;
    END IF;
    -- Already installed? Check dimension compatibility.
    IF EXISTS (SELECT 1 FROM pg_class
                WHERE relnamespace = 'ask'::regnamespace
                  AND relname = '_memories') THEN
        -- Verify existing column width matches the requested dimensions.
        -- A mismatch means the operator changed embedding models without
        -- ALTERing the column — surface a clear error with remediation.
        EXECUTE format('
            SELECT a.atttypmod
              FROM pg_attribute a
              JOIN pg_class c ON c.oid = a.attrelid
              JOIN pg_namespace n ON n.oid = c.relnamespace
             WHERE n.nspname = ''ask''
               AND c.relname = ''_memories''
               AND a.attname = ''embedding''
        ') INTO existing_dims;
        -- pgvector stores dimensions as (typmod & 0xFFFF) for vector type.
        -- If typmod is -1 the column exists but type info is opaque;
        -- skip the check in that edge case.
        IF existing_dims IS NOT NULL AND existing_dims != -1 THEN
            existing_dims := existing_dims & 65535;
            IF existing_dims != dims THEN
                RAISE EXCEPTION
                    'ask._memories.embedding is vector(%), but pg_ask.embedding_dimensions = %. '
                    'Run: ALTER TABLE ask._memories ALTER COLUMN embedding TYPE vector(%);',
                    existing_dims, dims, dims
                    USING ERRCODE = 'invalid_parameter_value';
            END IF;
        END IF;
        RETURN true;
    END IF;

    -- DDL goes through EXECUTE because the vector type may not have been
    -- known to the parser when this function was compiled (it isn't
    -- referenced at parse time, only at execute time).
    EXECUTE format($ddl$
        CREATE TABLE IF NOT EXISTS ask._memories (
            id         uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
            owner      name        NOT NULL DEFAULT current_user,
            namespace  text        NOT NULL DEFAULT 'default',
            content    text        NOT NULL,
            metadata   jsonb       NOT NULL DEFAULT '{}'::jsonb,
            embedding  vector(%s)  NOT NULL,
            tsv        tsvector    GENERATED ALWAYS AS (to_tsvector('simple', content)) STORED,
            created_at timestamptz NOT NULL DEFAULT now()
        )
    $ddl$, dims);
    CREATE INDEX IF NOT EXISTS _memories_owner_ns_idx
        ON ask._memories (owner, namespace, created_at DESC);
    CREATE INDEX IF NOT EXISTS _memories_tsv_idx
        ON ask._memories USING gin (tsv);
    BEGIN
        -- S4 fix: compute lists from expected row count.
        -- sqrt(n) is the standard pgvector recommendation for ivfflat.
        -- With no data yet, use a conservative default of 100.
        CREATE INDEX IF NOT EXISTS _memories_embedding_idx
            ON ask._memories USING ivfflat (embedding vector_cosine_ops)
            WITH (lists = greatest(10, least(4000, 100)));
    EXCEPTION WHEN feature_not_supported THEN
        -- Older pgvector builds without ivfflat fall back to no ANN index.
        NULL;
    END;

    -- No direct DML grant: writes go through ask._memory_insert /
    -- ask._memory_delete_owned (SECURITY DEFINER), which enforce the
    -- owner predicate. SELECT stays public + RLS-policed below.
    EXECUTE 'GRANT SELECT ON ask._memories TO PUBLIC';
    EXECUTE 'ALTER TABLE ask._memories ENABLE ROW LEVEL SECURITY';
    EXECUTE 'DROP POLICY IF EXISTS _memories_owner_select ON ask._memories';
    EXECUTE 'CREATE POLICY _memories_owner_select ON ask._memories '
         || 'FOR SELECT USING (owner = session_user)';
    RETURN true;
END
$$;

-- Run it once now so the install-time table create still happens when
-- pgvector is already present at CREATE EXTENSION time. Uses the default
-- 1536 dimensions; the Rust caller passes the GUC value on subsequent
-- calls so operators can change models without hand-editing.
DO $bootstrap_memory$ BEGIN PERFORM ask._memory_bootstrap(1536); END $bootstrap_memory$;

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
    -- row_count starts at -1 ("in flight") and is updated by
    -- ask._sql_audit_finish after the query runs. A row still showing
    -- -1 long after its `ts` means the connection died mid-query.
    row_count  int,
    error      text,
    readonly   bool        NOT NULL,
    tool_name  text        NOT NULL,
    -- H3 (v0.5.2 review): wall time from audit insert to result render,
    -- in milliseconds. NULL while the row is still in flight; populated
    -- by ask._sql_audit_finish.
    latency_ms bigint
);
-- Older installs may have the table without latency_ms; add it idempotently.
ALTER TABLE ask._sql_audit ADD COLUMN IF NOT EXISTS latency_ms bigint;
CREATE INDEX IF NOT EXISTS _sql_audit_ts_idx     ON ask._sql_audit (ts DESC);
CREATE INDEX IF NOT EXISTS _sql_audit_caller_idx ON ask._sql_audit (caller, ts DESC);

-- ---------------------------------------------------------------------------
-- Event outbox (ADR-0017: in-database reverse notifications).
--
-- `ask.emit(event, payload)` appends a durable row here and fires
-- `pg_notify('pg_ask_events', <id>)`. An external orchestrator (any
-- process holding a LISTEN pg_ask_events connection)
-- LISTENs on that channel and drains unprocessed rows, marking
-- `processed_at`. The durable table is the source of truth; NOTIFY is
-- only a low-latency wake-up, so no event is lost if the listener is
-- offline (it drains the backlog on reconnect).
--
-- Append-only from the caller's side: writes go through the
-- SECURITY DEFINER helper `ask._outbox_emit` (like _sql_audit), and the
-- only mutation a consumer performs is stamping `processed_at` via
-- `ask._outbox_mark_processed`. Readable by PUBLIC (no secrets here —
-- the payload is operator-authored), writable only through the helpers.
CREATE TABLE IF NOT EXISTS ask._outbox (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    ts           timestamptz NOT NULL DEFAULT now(),
    emitter      name        NOT NULL DEFAULT session_user,
    db           name        NOT NULL DEFAULT current_database(),
    event        text        NOT NULL,
    payload      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    -- Optional human-readable summary (e.g. an ask.ask() result). Kept
    -- separate from payload so a listener can show it without parsing JSON.
    summary      text,
    -- NULL while pending; stamped by the consumer once delivered.
    processed_at timestamptz
);
-- The hot path is "give me unprocessed rows, oldest first". A partial
-- index keeps it tiny even when the table accumulates processed history.
CREATE INDEX IF NOT EXISTS _outbox_pending_idx
    ON ask._outbox (ts) WHERE processed_at IS NULL;
-- Flood-control hot path (v0.5.8): the rate-limit and dedup checks in
-- ask._outbox_emit filter by (emitter, event, ts). Without this index those
-- guards degrade to a full scan of the whole outbox (including delivered
-- history) on every emit — turning the DoS protection into a DoS amplifier
-- on a large table. Cheap to maintain; only matters when a guard is on.
-- Not partial: the dedup/rate checks must see recent rows regardless of
-- processed status, so the index necessarily covers history too and grows
-- with the unpruned outbox — another reason to run ask.prune_events().
CREATE INDEX IF NOT EXISTS _outbox_rate_idx
    ON ask._outbox (emitter, event, ts);
-- Retention hot path (v0.5.8): ask._outbox_prune deletes delivered rows by
-- `processed_at < cutoff`. A partial index on the delivered rows serves the
-- prune scan directly (the pending partial index above has the opposite
-- predicate and can't help) and stays small — it only indexes rows that are
-- candidates for deletion.
CREATE INDEX IF NOT EXISTS _outbox_processed_idx
    ON ask._outbox (processed_at) WHERE processed_at IS NOT NULL;

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

-- H3 companion: update an in-flight audit row with the post-query
-- outcome. Caller is the tool wrapper (not the model), running under
-- session_user; the WHERE filter is on `caller` so a malicious extension
-- can't update someone else's audit row by guessing its uuid.
-- H3 caveat: this helper can only update the audit row in writable
-- transactions. When the surrounding ask.ask() runs in readonly mode
-- (the default), Postgres refuses to set `transaction_read_only = off`
-- mid-transaction ("must be set before any query"), and per-function
-- SET clauses are subject to the same restriction. The Rust caller
-- (tools::sql_query::audit_finish) detects readonly mode and skips the
-- helper entirely — the audit row stays at row_count = -1 ("in flight"),
-- which we document in the table comment as the readonly-mode tombstone.
-- Real H3 (post-query stats in readonly mode) needs a proper
-- subtransaction via pgrx FFI; tracked separately.
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
    -- Note: parameters with the same names as columns (latency_ms,
    -- error) need to be referenced via the function name as a
    -- qualifier; otherwise plpgsql resolves them to the column. We
    -- avoid the ambiguity by passing all three as explicit USING
    -- variables on an EXECUTE — simpler and easier to follow.
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

-- Event outbox writer (ADR-0017). The SINGLE authority for appending an
-- event: it validates input, honours the on/off switch, enforces flood
-- control, writes the durable row, AND fires the NOTIFY — all atomically in
-- one SECURITY DEFINER call. Returns the new id, or NULL when the emit was a
-- no-op (events disabled, or suppressed by a guard).
--
-- Why everything lives here, not in the Rust caller (v0.5.8 B2 fix): this
-- function is GRANTed to PUBLIC so ordinary roles can emit without direct
-- INSERT on ask._outbox. If validation / the enabled-check / NOTIFY lived
-- only in ask.emit (the #[pg_extern]), a caller could bypass all of it by
-- invoking ask._outbox_emit() directly — writing a newline-laced event name,
-- a multi-megabyte payload, or a row while events are globally disabled. By
-- making the helper self-contained, BOTH entry points are protected and the
-- Rust layer is pure defense-in-depth (nicer error messages, early exit).
-- session_user survives SECURITY DEFINER, so the row is still billed to the
-- original connecting role (see _sql_audit_insert).
--
-- Flood control. Two optional, GUC-driven guards run BEFORE the INSERT:
--   * pg_ask.events_max_per_minute  — per-(emitter,event) rate cap.
--   * pg_ask.events_dedup_window_ms — collapse identical
--                                     (emitter,event,payload) repeats.
-- Both default to 0 (off). A suppressed emit returns NULL WITHOUT writing a
-- row and does NOT raise: emit runs inside the caller's (often a trigger's)
-- transaction, and an ERROR here would roll back the very INSERT/UPDATE that
-- fired it. Suppressions are surfaced via RAISE DEBUG so an operator can see
-- them with log_min_messages=debug1 without paying anything in production.
--
-- Atomicity: a plain count-then-insert is racy under concurrency (two
-- backends both pass the cap before either commits, since uncommitted rows
-- aren't mutually visible). We serialize same-key emitters with a
-- transaction-scoped advisory lock keyed on (emitter,event); it is released
-- automatically at commit/rollback and is only taken when a guard is active,
-- so the unguarded fast path pays nothing.
--
-- Validation is owned entirely here (the single authority); the Rust caller
-- does no size/charset checks, so there is nothing to drift out of sync.
-- event: non-empty, <= 127 chars, ^[A-Za-z0-9][A-Za-z0-9._:-]*$ (ASCII, so
-- char count == byte count). summary: <= 8192 BYTES (octet_length, so the
-- ceiling is exact for multi-byte text). payload: <= the
-- pg_ask.events_max_payload_bytes serialized-JSON bytes (0 disables).
CREATE OR REPLACE FUNCTION ask._outbox_emit(
    event   text,
    payload jsonb,
    summary text
) RETURNS uuid
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    enabled      bool := COALESCE(current_setting('pg_ask.events_enabled', true)::bool, false);
    max_per_min  int  := COALESCE(NULLIF(current_setting('pg_ask.events_max_per_minute', true), '')::int, 0);
    dedup_ms     int  := COALESCE(NULLIF(current_setting('pg_ask.events_dedup_window_ms', true), '')::int, 0);
    max_payload  int  := COALESCE(NULLIF(current_setting('pg_ask.events_max_payload_bytes', true), '')::int, 65536);
    trimmed      text := btrim(event);
    norm_payload jsonb := COALESCE(payload, '{}'::jsonb);
    payload_len  int;
    new_id       uuid;
BEGIN
    -- Global off switch (also enforced in Rust; duplicated so a direct
    -- _outbox_emit call can't write while events are disabled).
    IF NOT enabled THEN
        RETURN NULL;
    END IF;

    -- ---- Validation (caller bugs → hard error, before any write) --------
    IF trimmed IS NULL OR trimmed = '' THEN
        RAISE EXCEPTION 'event name must not be empty'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF length(trimmed) > 127 THEN
        RAISE EXCEPTION 'event name must be <= 127 chars, got %', length(trimmed)
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF trimmed !~ '^[A-Za-z0-9][A-Za-z0-9._:-]*$' THEN
        RAISE EXCEPTION 'event name must start alphanumeric and contain only [A-Za-z0-9._:-]'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF summary IS NOT NULL AND octet_length(summary) > 8192 THEN
        RAISE EXCEPTION 'summary must be <= 8192 bytes, got %', octet_length(summary)
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF max_payload > 0 THEN
        payload_len := octet_length(norm_payload::text);
        IF payload_len > max_payload THEN
            RAISE EXCEPTION 'payload must be <= % bytes, got %', max_payload, payload_len
                USING ERRCODE = 'invalid_parameter_value';
        END IF;
    END IF;

    -- ---- Flood control --------------------------------------------------
    IF max_per_min > 0 OR dedup_ms > 0 THEN
        -- Serialize same-key emitters so the checks below are not subject to
        -- a check-then-insert race. Transaction-scoped advisory lock, freed
        -- automatically at commit/rollback.
        --
        -- Two-argument (int4, int4) form: the FIRST arg is a fixed domain
        -- ('pg_ask._outbox') and the SECOND is the (emitter, event) key.
        -- Postgres keeps the two-arg lock space wholly separate from the
        -- one-arg int8 space, so this can never collide with the int8
        -- session lock in ask._session_lock_for_append. Within this domain a
        -- collision only happens for the same (emitter, event) — exactly the
        -- pairs we mean to serialize.
        --
        -- hashtextextended returns bigint; the two-arg lock takes int4, so we
        -- mask to the low 31 bits (& 0x7fffffff) to land in a non-negative
        -- int4 instead of casting (which raises "integer out of range" for
        -- hashes outside int4's range).
        PERFORM pg_advisory_xact_lock(
            (hashtextextended('pg_ask._outbox', 0) & 2147483647)::int,
            (hashtextextended(session_user::text || '|' || trimmed, 0) & 2147483647)::int
        );
    END IF;

    -- Dedup: identical (emitter,event,payload) within the window → no-op.
    -- Compared via md5(payload::text) rather than the jsonb `=` operator:
    -- jsonb normalizes (key order / whitespace) before text-casting, so the
    -- hash is stable for logically-equal payloads, and hashing a short text
    -- digest is far cheaper than an equality scan over large jsonb values.
    IF dedup_ms > 0 THEN
        IF EXISTS (
            SELECT 1 FROM ask._outbox o
             WHERE o.emitter = session_user
               AND o.event   = trimmed
               AND o.ts > now() - make_interval(secs => dedup_ms / 1000.0)
               AND md5(o.payload::text) = md5(norm_payload::text)
        ) THEN
            RAISE DEBUG 'pg_ask: emit deduped (emitter=%, event=%)', session_user, trimmed;
            RETURN NULL;
        END IF;
    END IF;

    -- Rate limit: per (emitter,event) over the last rolling minute.
    IF max_per_min > 0 THEN
        IF (SELECT count(*) FROM ask._outbox o
             WHERE o.emitter = session_user
               AND o.event   = trimmed
               AND o.ts > now() - interval '1 minute') >= max_per_min THEN
            RAISE DEBUG 'pg_ask: emit rate-limited (emitter=%, event=%, cap=%/min)',
                session_user, trimmed, max_per_min;
            RETURN NULL;
        END IF;
    END IF;

    -- ---- Durable append + low-latency wake-up (atomic) ------------------
    INSERT INTO ask._outbox (emitter, event, payload, summary)
    VALUES (session_user, trimmed, norm_payload, summary)
    RETURNING id INTO new_id;

    -- NOTIFY carries only the id (pg_notify's payload is 8 KB capped); the
    -- listener reads the full row from the outbox. Fired here so a direct
    -- _outbox_emit caller can't write a row without waking listeners.
    PERFORM pg_notify('pg_ask_events', new_id::text);

    RETURN new_id;
END
$$;

-- Retention helper (v0.5.8). Deletes outbox rows that have ALREADY been
-- delivered (processed_at IS NOT NULL) and are older than `older_than`.
-- Pending rows are never touched, so a slow/offline consumer can't lose
-- undelivered events to a prune. Returns the number of rows removed.
--
-- Batched (H4): the first prune on a long-neglected outbox could otherwise
-- delete millions of rows in ONE statement. We instead delete in chunks of
-- `batch_size`, which bounds the size of each individual DELETE — capping
-- per-statement memory, lock acquisition, and dead-tuple churn, and letting
-- autovacuum interleave more easily.
--
-- HONEST LIMITATION: this is a plpgsql function, whose whole body runs in
-- the caller's single transaction. The loop therefore does NOT commit
-- between batches — all chunks become durable together at the outer COMMIT,
-- so total WAL volume and the lifetime of the accumulated row locks are the
-- same as one big DELETE. To get per-batch commits (genuinely shorter locks
-- and incremental WAL flush) the caller must invoke this repeatedly from
-- separate transactions, or a future version must expose a PROCEDURE that
-- can COMMIT mid-loop. The batching here is still worthwhile for the
-- per-statement bounds above. `batch_size <= 0` falls back to a single
-- unbounded DELETE.
-- SECURITY DEFINER so an operator/maintenance role can prune without owning
-- the table; exposed to callers via ask.prune_events(interval, int).
CREATE OR REPLACE FUNCTION ask._outbox_prune(
    older_than interval,
    batch_size int DEFAULT 10000
) RETURNS bigint
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    total   bigint := 0;
    removed bigint;
BEGIN
    IF batch_size IS NULL OR batch_size <= 0 THEN
        WITH del AS (
            DELETE FROM ask._outbox
             WHERE processed_at IS NOT NULL
               AND processed_at < now() - older_than
            RETURNING 1
        )
        SELECT count(*) INTO total FROM del;
        RETURN total;
    END IF;

    LOOP
        WITH cand AS (
            SELECT id FROM ask._outbox
             WHERE processed_at IS NOT NULL
               AND processed_at < now() - older_than
             LIMIT batch_size
        ), del AS (
            DELETE FROM ask._outbox o
             USING cand
             WHERE o.id = cand.id
            RETURNING 1
        )
        SELECT count(*) INTO removed FROM del;
        total := total + removed;
        EXIT WHEN removed = 0;
    END LOOP;
    RETURN total;
END
$$;

-- Consumer-side: stamp a delivered row as processed. Idempotent (only
-- stamps rows still pending) and returns whether it changed anything, so a
-- listener can tell a fresh delivery from a duplicate wake-up. SECURITY
-- DEFINER because the designated consumer role need not own the table.
--
-- Deliberately NOT filtered by emitter: the consumer (a LISTEN
-- pg_ask_events drainer) almost always connects as a DIFFERENT role than the one that
-- emitted the event (a trigger fires as the app role; the listener uses a
-- dedicated reader DSN). An `emitter = session_user` filter would make the
-- consumer unable to stamp those rows, causing infinite re-delivery. The
-- consumer is by design the single trusted drain for the whole outbox.
--
-- Suppression protection (a rogue role pre-marking events to hide alerts)
-- is an operator concern, handled the same way as the rest of pg_ask's
-- write surface: in a multi-tenant / untrusted-caller database, REVOKE
-- EXECUTE on this helper from PUBLIC after CREATE EXTENSION and GRANT it
-- only to the consumer role. See docs/SECURITY.md.
CREATE OR REPLACE FUNCTION ask._outbox_mark_processed(
    outbox_id uuid
) RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    WITH upd AS (
        UPDATE ask._outbox
           SET processed_at = now()
         WHERE id = outbox_id AND processed_at IS NULL
        RETURNING id
    )
    SELECT EXISTS (SELECT 1 FROM upd);
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
-- Session writer helpers (C2-bis from the Gemini v0.5.2 review).
--
-- ask.create_session / chat / clear_session are SECURITY INVOKER
-- pg_extern functions, but `REVOKE ALL ON ask._sessions / _messages
-- FROM PUBLIC` (see grant policy below) makes direct INSERT/UPDATE/
-- DELETE through SPI fail for non-superusers. These helpers are the
-- one funnel the Rust caller goes through; each enforces session_user
-- ownership inside its body so EXECUTE to PUBLIC is safe.
-- ---------------------------------------------------------------------------

CREATE OR REPLACE FUNCTION ask._session_create(label text)
RETURNS uuid
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    INSERT INTO ask._sessions (label, owner)
    VALUES (label, session_user)
    RETURNING id;
$$;

CREATE OR REPLACE FUNCTION ask._session_is_owned(p_session_id uuid)
RETURNS boolean
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
STABLE
AS $$
    SELECT EXISTS (
        SELECT 1 FROM ask._sessions
         WHERE id = p_session_id AND owner = session_user
    );
$$;

CREATE OR REPLACE FUNCTION ask._session_fetch_messages(p_session_id uuid)
RETURNS TABLE (
    role          text,
    content       text,
    tool_calls    text,
    tool_call_id  text,
    is_error      boolean
)
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
STABLE
AS $$
    SELECT m.role,
           m.content,
           m.tool_calls::text,
           m.tool_call_id,
           m.is_error
      FROM ask._messages m
      JOIN ask._sessions s ON s.id = m.session_id
     WHERE m.session_id = p_session_id
       AND s.owner = session_user
     ORDER BY m.idx;
$$;

CREATE OR REPLACE FUNCTION ask._session_lock_for_append(p_session_id uuid)
RETURNS void
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    SELECT pg_advisory_xact_lock(
        hashtextextended('ask._messages:' || p_session_id::text, 0)
    );
$$;

CREATE OR REPLACE FUNCTION ask._session_append_message(
    p_session_id      uuid,
    msg_role          text,
    msg_content       text,
    tool_calls_text   text,
    msg_tool_call_id  text,
    msg_is_error      boolean
)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    -- Parameter names are prefixed with `p_` (session_id) / `msg_`
    -- because plpgsql's name resolution would otherwise treat
    -- `WHERE session_id = session_id` as `WHERE col = col` (always
    -- true) and let any role read every other role's history. The
    -- prefix kills the ambiguity at the cost of a slightly noisier
    -- helper signature.
    IF NOT EXISTS (
        SELECT 1 FROM ask._sessions
         WHERE id = p_session_id AND owner = session_user
    ) THEN
        RAISE EXCEPTION 'no such session for current_user'
            USING ERRCODE = 'insufficient_privilege';
    END IF;

    INSERT INTO ask._messages
        (session_id, idx, role, content, tool_calls, tool_call_id, is_error)
    SELECT p_session_id,
           COALESCE(MAX(m.idx), -1) + 1,
           msg_role,
           msg_content,
           NULLIF(tool_calls_text, '')::jsonb,
           msg_tool_call_id,
           msg_is_error
      FROM ask._messages m
     WHERE m.session_id = p_session_id;
END
$$;

CREATE OR REPLACE FUNCTION ask._session_touch(p_session_id uuid)
RETURNS void
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    UPDATE ask._sessions
       SET updated_at = now()
     WHERE id = p_session_id AND owner = session_user;
$$;

CREATE OR REPLACE FUNCTION ask._session_clear_messages(p_session_id uuid)
RETURNS void
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    DELETE FROM ask._messages m
     USING ask._sessions s
     WHERE m.session_id = p_session_id
       AND s.id = m.session_id
       AND s.owner = session_user;
$$;

-- Internal _config reader (C3-bis follow-up). RuntimeConfig::load on
-- the Rust side resolves provider / model / api_key by calling back
-- into `read_table`; under v0.5.2's `REVOKE ALL ON ask._config FROM
-- PUBLIC` policy that SELECT trips `permission denied` for every
-- non-superuser caller. The helper here is SECURITY DEFINER so the
-- non-superuser path can still read its own config; the public
-- `ask.get_config()` continues to redact secret keys on the Rust
-- side before returning.
CREATE OR REPLACE FUNCTION ask._config_get(lookup_key text)
RETURNS text
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
STABLE
AS $$
BEGIN
    IF lookup_key IN ('api_key', 'embedding_api_key') AND NOT current_setting('is_superuser')::boolean THEN
        RAISE EXCEPTION 'permission denied to read secret config key';
    END IF;
    RETURN (SELECT value FROM ask._config WHERE key = lookup_key);
END;
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
-- the table only exists when pgvector is installed. _traces uses RLS
-- (S6 fix) so SELECT is granted to PUBLIC but rows are filtered by caller.
GRANT SELECT ON ask._traces    TO PUBLIC;
GRANT SELECT ON ask._tools     TO PUBLIC;
GRANT SELECT ON ask._sql_audit TO PUBLIC;
GRANT SELECT ON ask._outbox    TO PUBLIC;
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
REVOKE ALL ON FUNCTION ask._write_trace(jsonb)                                            FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._sql_audit_insert(text, int, bool, text)                       FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._sql_audit_finish(uuid, bigint, text)                          FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._outbox_emit(text, jsonb, text)                                FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._outbox_mark_processed(uuid)                                   FROM PUBLIC;
-- _outbox_prune deletes delivered rows; kept operator-only (like the config
-- surface). NOT granted to PUBLIC — operators grant it to a maintenance role
-- after CREATE EXTENSION. See finalize.sql for the matching ask.prune_events.
REVOKE ALL ON FUNCTION ask._outbox_prune(interval, int)                                   FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._memory_insert(text, text, jsonb, text)                        FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._memory_delete_owned(uuid)                                     FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._tool_register(text, jsonb, text)                              FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._tool_unregister(text)                                         FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._memory_bootstrap(int)                                            FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._session_create(text)                                          FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._session_is_owned(uuid)                                        FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._session_fetch_messages(uuid)                                  FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._session_lock_for_append(uuid)                                 FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._session_append_message(uuid, text, text, text, text, bool)    FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._session_touch(uuid)                                           FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._session_clear_messages(uuid)                                  FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._config_get(text)                                              FROM PUBLIC;
GRANT EXECUTE ON FUNCTION ask._write_trace(jsonb)                                         TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._memory_bootstrap(int)                                         TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._sql_audit_insert(text, int, bool, text)                    TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._sql_audit_finish(uuid, bigint, text)                       TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._outbox_emit(text, jsonb, text)                             TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._outbox_mark_processed(uuid)                                TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._memory_insert(text, text, jsonb, text)                     TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._memory_delete_owned(uuid)                                  TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._tool_register(text, jsonb, text)                           TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._tool_unregister(text)                                      TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._session_create(text)                                       TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._session_is_owned(uuid)                                     TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._session_fetch_messages(uuid)                               TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._session_lock_for_append(uuid)                              TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._session_append_message(uuid, text, text, text, text, bool) TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._session_touch(uuid)                                        TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._session_clear_messages(uuid)                               TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._config_get(text)                                           TO PUBLIC;

-- Config-surface lockdown (C6) lives in a finalize SQL block in
-- `sql/finalize.sql` because pgrx emits the #[pg_extern] config
-- functions *after* this bootstrap script runs.
