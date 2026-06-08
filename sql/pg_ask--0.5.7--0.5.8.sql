-- pg_ask 0.5.7 → 0.5.8 upgrade.
--
-- Production-hardens the event outbox (ADR-0017) without changing the
-- consumer-facing contract: the `pg_ask_events` channel, the `ask._outbox`
-- columns, the pending-row query (`WHERE processed_at IS NULL ORDER BY ts`),
-- and the `ask._outbox_emit(text,jsonb,text)` / `ask._outbox_mark_processed`
-- / `ask.emit` signatures are all unchanged, so a LISTEN pg_ask_events
-- consumer keeps working across the upgrade.
--
-- Changes:
--   1. New (emitter, event, ts) index so the flood-control checks in
--      ask._outbox_emit don't full-scan the outbox on every emit.
--   2. ask._outbox_emit becomes the single authority: it re-checks
--      events_enabled, validates input (event name regex/length, summary
--      length, payload bytes), enforces the rate-limit / dedup guards,
--      INSERTs, AND fires pg_notify — all atomically. This closes the bypass
--      where calling _outbox_emit directly skipped the Rust-side checks.
--      Suppressed emits return NULL (silent no-op) and never raise; caller
--      bugs (bad name / oversized payload) raise invalid_parameter_value.
--      Dedup compares md5(payload::text) instead of jsonb equality.
--   3. New ask._outbox_prune(interval, int) + ask.prune_events(text, int)
--      retention helper. Deletes ONLY already-delivered rows older than the
--      interval, in batches, so the first prune of a neglected outbox isn't
--      one giant transaction. Pending rows are never removed.
--   4. New GUCs (events_max_payload_bytes, events_max_per_minute,
--      events_dedup_window_ms) ship in the new .so.
--
-- Idempotent: CREATE OR REPLACE preserves grants/ownership; re-running is
-- safe.

-- ── Flood-control + retention indexes ───────────────────────────────────────
CREATE INDEX IF NOT EXISTS _outbox_rate_idx
    ON ask._outbox (emitter, event, ts);
CREATE INDEX IF NOT EXISTS _outbox_processed_idx
    ON ask._outbox (processed_at) WHERE processed_at IS NOT NULL;

-- ── Single-authority writer: validation + enabled + flood control + NOTIFY ──
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
    IF NOT enabled THEN
        RETURN NULL;
    END IF;

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

    IF max_per_min > 0 OR dedup_ms > 0 THEN
        PERFORM pg_advisory_xact_lock(
            (hashtextextended('pg_ask._outbox', 0) & 2147483647)::int,
            (hashtextextended(session_user::text || '|' || trimmed, 0) & 2147483647)::int
        );
    END IF;

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

    INSERT INTO ask._outbox (emitter, event, payload, summary)
    VALUES (session_user, trimmed, norm_payload, summary)
    RETURNING id INTO new_id;

    PERFORM pg_notify('pg_ask_events', new_id::text);

    RETURN new_id;
END
$$;

-- ── Batched retention helper ────────────────────────────────────────────────
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

-- ── Grants ──────────────────────────────────────────────────────────────────
-- _outbox_emit keeps its existing PUBLIC EXECUTE (CREATE OR REPLACE preserved
-- it). _outbox_prune is destructive → operator-only.
REVOKE ALL ON FUNCTION ask._outbox_prune(interval, int) FROM PUBLIC;

-- ask.prune_events(text, int) is a NEW #[pg_extern] in 0.5.8. Unlike the SQL
-- helpers above (which we CREATE OR REPLACE by hand), pgrx does NOT emit
-- CREATE FUNCTION DDL for #[pg_extern]s into an ALTER EXTENSION UPDATE script
-- — that DDL only lands in the base-install pg_ask--<version>.sql. So a new
-- C-language entry point must be created here explicitly, or it is simply
-- missing after an upgrade. This mirrors the pgrx-generated definition in
-- pg_ask--0.5.8.sql (LANGUAGE c, MODULE_PATHNAME, prune_events_wrapper).
CREATE OR REPLACE FUNCTION ask."prune_events"(
    "older_than" text,
    "batch_size" int DEFAULT 10000
) RETURNS bigint
STRICT VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'prune_events_wrapper';

-- Destructive (deletes outbox rows), so lock it to operators rather than
-- leaving pgrx's default EXECUTE TO PUBLIC. The finalize step that does this
-- on fresh installs does not run on ALTER EXTENSION UPDATE, so we repeat it.
REVOKE ALL ON FUNCTION ask.prune_events(text, int) FROM PUBLIC;
