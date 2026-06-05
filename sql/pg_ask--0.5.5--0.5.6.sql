-- pg_ask 0.5.5 → 0.5.6 upgrade.
--
-- Adds the event outbox (ADR-0017: pg_ask -> senti reverse notifications).
-- Unlike the 0.5.4 / 0.5.5 no-op scripts, this one DOES create SQL objects,
-- because the new table + writer helpers must exist for installs that are
-- upgrading from 0.5.5 (where they don't yet). The matching Rust surface
-- (ask.emit, the events_enabled GUC) ships in the new .so.
--
-- Everything below is idempotent so a partial run can be retried.

-- ── Event outbox table ──────────────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS ask._outbox (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    ts           timestamptz NOT NULL DEFAULT now(),
    emitter      name        NOT NULL DEFAULT session_user,
    db           name        NOT NULL DEFAULT current_database(),
    event        text        NOT NULL,
    payload      jsonb       NOT NULL DEFAULT '{}'::jsonb,
    summary      text,
    processed_at timestamptz
);
CREATE INDEX IF NOT EXISTS _outbox_pending_idx
    ON ask._outbox (ts) WHERE processed_at IS NULL;

-- ── Writer / consumer helpers (SECURITY DEFINER) ────────────────────────────
CREATE OR REPLACE FUNCTION ask._outbox_emit(
    event   text,
    payload jsonb,
    summary text
) RETURNS uuid
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
    INSERT INTO ask._outbox (emitter, event, payload, summary)
    VALUES (session_user, event, COALESCE(payload, '{}'::jsonb), summary)
    RETURNING id;
$$;

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

-- ── Grants: table readable by PUBLIC, writes only via the helpers ───────────
GRANT SELECT ON ask._outbox TO PUBLIC;
REVOKE ALL ON FUNCTION ask._outbox_emit(text, jsonb, text)      FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._outbox_mark_processed(uuid)         FROM PUBLIC;
GRANT EXECUTE ON FUNCTION ask._outbox_emit(text, jsonb, text)   TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._outbox_mark_processed(uuid)      TO PUBLIC;

-- Note: ask.emit(text, jsonb, text) itself is created by the pgrx-generated
-- portion of this upgrade (it is a #[pg_extern]); no hand-written DDL needed.
