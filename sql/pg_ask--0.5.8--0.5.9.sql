-- pg_ask 0.5.8 → 0.5.9 upgrade.
--
-- Adds the async job queue (ADR-0018): ask.ask_async() enqueues a question
-- to ask._jobs and returns immediately; a background worker (or a manual
-- ask.run_pending_jobs() / pg_cron call) runs the agent loop in its own
-- backend and writes the answer back. This is the only correct shape for
-- async work in PostgreSQL — a backend is single-threaded and SPI is not
-- thread-safe, so "async" means handing the work to a separate process.
--
-- Everything here is additive (new table, indexes, helpers, #[pg_extern]s,
-- GUCs in the new .so) and idempotent. Existing surface is untouched, so
-- nothing breaks across the upgrade.
--
-- Function bodies below are character-identical to sql/bootstrap.sql (verified
-- by diff in CI); keep the two in sync.

-- ── Job queue table + indexes ───────────────────────────────────────────────
CREATE TABLE IF NOT EXISTS ask._jobs (
    id           uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    ts           timestamptz NOT NULL DEFAULT now(),
    owner        name        NOT NULL DEFAULT session_user,
    db           name        NOT NULL DEFAULT current_database(),
    kind         text        NOT NULL DEFAULT 'ask',
    question     text        NOT NULL,
    status       text        NOT NULL DEFAULT 'pending'
                 CHECK (status IN ('pending','running','done','failed','cancelled')),
    attempts     int         NOT NULL DEFAULT 0,
    started_at   timestamptz,
    finished_at  timestamptz,
    answer       text,
    error        text,
    prompt_tokens     bigint,
    completion_tokens bigint,
    worker_pid   int
);
-- Drop-and-recreate the hot-path indexes so any install that created the
-- earlier (ts)/(started_at) forms picks up the (db, ...) leading column
-- (CREATE INDEX IF NOT EXISTS alone won't alter an existing index).
DROP INDEX IF EXISTS ask._jobs_pending_idx;
DROP INDEX IF EXISTS ask._jobs_running_idx;
CREATE INDEX IF NOT EXISTS _jobs_pending_idx
    ON ask._jobs (db, ts) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS _jobs_running_idx
    ON ask._jobs (db, started_at) WHERE status = 'running';
CREATE INDEX IF NOT EXISTS _jobs_owner_idx
    ON ask._jobs (owner, ts);
CREATE INDEX IF NOT EXISTS _jobs_terminal_idx
    ON ask._jobs (finished_at) WHERE status IN ('done','failed','cancelled');

-- Row-level security (mirrors bootstrap.sql): a direct SELECT on ask._jobs is
-- scoped to the caller's own rows. Superuser (table owner, bgworker) bypasses.
ALTER TABLE ask._jobs ENABLE ROW LEVEL SECURITY;
DO $jobs_rls$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_policy WHERE polname = '_jobs_owner_select'
          AND polrelid = 'ask._jobs'::regclass
    ) THEN
        CREATE POLICY _jobs_owner_select ON ask._jobs
            FOR SELECT USING (owner = session_user);
    END IF;
END
$jobs_rls$;

-- ── SECURITY DEFINER state-machine helpers ──────────────────────────────────
-- (bodies mirror sql/bootstrap.sql)

CREATE OR REPLACE FUNCTION ask._job_submit(
    kind     text,
    question text
) RETURNS uuid
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    enabled bool := COALESCE(current_setting('pg_ask.jobs_enabled', true)::bool, false);
    norm    text := btrim(question);
    new_id  uuid;
BEGIN
    IF NOT enabled THEN
        RETURN NULL;
    END IF;
    IF kind IS NULL OR kind NOT IN ('ask','sql') THEN
        RAISE EXCEPTION 'job kind must be ''ask'' or ''sql'', got %', kind
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    IF norm IS NULL OR norm = '' THEN
        RAISE EXCEPTION 'job question must not be empty'
            USING ERRCODE = 'invalid_parameter_value';
    END IF;
    INSERT INTO ask._jobs (owner, kind, question)
    VALUES (session_user, kind, norm)
    RETURNING id INTO new_id;
    PERFORM pg_notify('pg_ask_jobs', new_id::text);
    RETURN new_id;
END
$$;

-- Claim the oldest pending job for THIS database, atomically. Uses
-- FOR UPDATE SKIP LOCKED so concurrent workers never claim the same row and
-- never block each other. Flips pending -> running, stamps started_at /
-- worker_pid, bumps attempts. Returns the full job row (or no row when the
-- queue is empty). Scoped to current_database() so a worker only ever runs
-- jobs from the DB it is connected to (a bgworker binds to one DB).
CREATE OR REPLACE FUNCTION ask._job_claim()
RETURNS TABLE (id uuid, kind text, question text, attempts int, owner name)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RETURN QUERY
    WITH next AS (
        SELECT j.id FROM ask._jobs j
         WHERE j.status = 'pending'
           AND j.db = current_database()
         ORDER BY j.ts, j.id
         FOR UPDATE SKIP LOCKED
         LIMIT 1
    )
    UPDATE ask._jobs j
       SET status     = 'running',
           started_at = now(),
           finished_at = NULL,
           worker_pid = pg_backend_pid(),
           attempts   = j.attempts + 1
      FROM next
     WHERE j.id = next.id
    RETURNING j.id, j.kind, j.question, j.attempts, j.owner;
END
$$;

-- Mark a claimed job done with its answer + token usage. Only transitions a
-- row that is still 'running' (a cancel mid-flight wins). Fires
-- pg_notify('pg_ask_jobs_done', id) so a waiting client wakes.
CREATE OR REPLACE FUNCTION ask._job_complete(
    job_id      uuid,
    p_answer    text,
    p_prompt    bigint,
    p_completion bigint
) RETURNS boolean
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n int;
BEGIN
    -- The worker_pid guard (B1) makes completion claim-scoped: only the
    -- backend that currently owns the running row can finish it. Without it,
    -- a slow-but-alive worker A whose job was orphan-recovered and re-claimed
    -- by worker B could complete B's fresh attempt with A's stale answer
    -- (double-execution / wrong result).
    UPDATE ask._jobs
       SET status = 'done',
           answer = p_answer,
           error = NULL,
           prompt_tokens = p_prompt,
           completion_tokens = p_completion,
           finished_at = now()
     WHERE id = job_id AND status = 'running' AND worker_pid = pg_backend_pid();
    GET DIAGNOSTICS n = ROW_COUNT;
    IF n > 0 THEN
        PERFORM pg_notify('pg_ask_jobs_done', job_id::text);
    END IF;
    RETURN n > 0;
END
$$;

-- Mark a claimed job failed. If attempts remain (< max_attempts) the job is
-- returned to 'pending' for retry; otherwise it is terminal 'failed'. Only
-- acts on a 'running' row. `max_attempts` is passed by the caller (read from
-- the GUC in Rust) so the policy lives in one place. Notifies done-channel
-- only on terminal failure so a client awaiting a result isn't woken for a
-- transient retry.
CREATE OR REPLACE FUNCTION ask._job_fail(
    job_id       uuid,
    p_error      text,
    max_attempts int
) RETURNS text
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    cur_attempts int;
    new_status   text;
BEGIN
    -- worker_pid guard (B1): only the backend that owns the running claim may
    -- fail it, so a re-claimed job isn't failed/retried by a ghost worker.
    SELECT attempts INTO cur_attempts
      FROM ask._jobs
     WHERE id = job_id AND status = 'running' AND worker_pid = pg_backend_pid()
      FOR UPDATE;
    IF cur_attempts IS NULL THEN
        RETURN NULL;  -- not ours / not running (done/cancelled/re-claimed); no-op
    END IF;
    IF cur_attempts >= max_attempts THEN
        new_status := 'failed';
        UPDATE ask._jobs
           SET status = 'failed', error = p_error, finished_at = now()
         WHERE id = job_id;
        PERFORM pg_notify('pg_ask_jobs_done', job_id::text);
    ELSE
        new_status := 'pending';
        UPDATE ask._jobs
           SET status = 'pending', error = p_error,
               started_at = NULL, worker_pid = NULL
         WHERE id = job_id;
        PERFORM pg_notify('pg_ask_jobs', job_id::text);  -- re-wake a worker
    END IF;
    RETURN new_status;
END
$$;

-- Crash recovery: return 'running' jobs whose started_at is older than
-- `timeout_ms` back to 'pending' (their worker presumably died). Scoped to
-- current_database(). Returns the number of jobs recovered. Idempotent.
CREATE OR REPLACE FUNCTION ask._job_recover_orphans(
    timeout_ms int
) RETURNS bigint
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n bigint;
BEGIN
    WITH revived AS (
        UPDATE ask._jobs
           SET status = 'pending', started_at = NULL, worker_pid = NULL
         WHERE status = 'running'
           AND db = current_database()
           AND started_at < now() - make_interval(secs => timeout_ms / 1000.0)
        RETURNING id
    )
    SELECT count(*) INTO n FROM revived;
    RETURN n;
END
$$;

-- Owner-scoped cancel. A job that is pending or running flips to
-- 'cancelled'; the completion helpers refuse to transition a non-running
-- row, so an in-flight worker's result is discarded. Returns true if it
-- changed anything. Filtered by owner = session_user so a role can only
-- cancel its own jobs.
CREATE OR REPLACE FUNCTION ask._job_cancel(
    job_id uuid
) RETURNS boolean
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n int;
BEGIN
    UPDATE ask._jobs
       SET status = 'cancelled', finished_at = now()
     WHERE id = job_id
       AND owner = session_user
       AND status IN ('pending','running');
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n > 0;
END
$$;

CREATE OR REPLACE FUNCTION ask._job_release(
    job_id uuid
) RETURNS boolean
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp
AS $$
DECLARE
    n int;
BEGIN
    UPDATE ask._jobs
       SET status = 'pending', started_at = NULL, worker_pid = NULL
     WHERE id = job_id AND status = 'running' AND worker_pid = pg_backend_pid();
    GET DIAGNOSTICS n = ROW_COUNT;
    RETURN n > 0;
END
$$;

-- Delete terminal (done/failed/cancelled) jobs older than `older_than`,
-- in batches — same retention pattern as ask._outbox_prune. Pending/running
-- jobs are never touched. Operator-only.
CREATE OR REPLACE FUNCTION ask._jobs_prune(
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
            DELETE FROM ask._jobs
             WHERE status IN ('done','failed','cancelled')
               AND finished_at < now() - older_than
            RETURNING 1
        )
        SELECT count(*) INTO total FROM del;
        RETURN total;
    END IF;
    LOOP
        WITH cand AS (
            SELECT id FROM ask._jobs
             WHERE status IN ('done','failed','cancelled')
               AND finished_at < now() - older_than
             LIMIT batch_size
        ), del AS (
            DELETE FROM ask._jobs j USING cand
             WHERE j.id = cand.id
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
-- Two tiers (mirrors bootstrap.sql):
--   * user-facing owner-scoped → PUBLIC: _job_submit, _job_cancel.
--   * worker-path, NOT owner-filtered → operator-only (NOT granted to
--     PUBLIC): _job_claim, _job_complete, _job_fail, _job_recover_orphans.
--     A malicious role could otherwise claim/complete/fail another tenant's
--     job. The worker connects as superuser; grant these to a dedicated
--     drain role if you use one.
--   * destructive prune → operator-only.
GRANT SELECT ON ask._jobs TO PUBLIC;
REVOKE ALL ON FUNCTION ask._job_submit(text, text)                   FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._job_claim()                              FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._job_complete(uuid, text, bigint, bigint) FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._job_fail(uuid, text, int)                FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._job_recover_orphans(int)                 FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._job_release(uuid)                        FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._job_cancel(uuid)                         FROM PUBLIC;
REVOKE ALL ON FUNCTION ask._jobs_prune(interval, int)                FROM PUBLIC;
GRANT EXECUTE ON FUNCTION ask._job_submit(text, text)                   TO PUBLIC;
GRANT EXECUTE ON FUNCTION ask._job_cancel(uuid)                         TO PUBLIC;

-- ── Public #[pg_extern] entry points ────────────────────────────────────────
-- pgrx does NOT emit CREATE FUNCTION DDL for #[pg_extern]s into an
-- ALTER EXTENSION UPDATE script (only into the base-install file), so each
-- new C-language entry point must be created explicitly here, mirroring the
-- pgrx-generated definitions in pg_ask--0.5.9.sql. (Lesson from the 0.5.8
-- prune_events fix.) MODULE_PATHNAME expands to $libdir/pg_ask in the
-- extension-script context.
CREATE OR REPLACE FUNCTION ask."ask_async"(
    "question" text,
    "kind" text DEFAULT 'ask'
) RETURNS uuid
STRICT VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'ask_async_wrapper';

CREATE OR REPLACE FUNCTION ask."job_status"(
    "job_id" uuid
) RETURNS text
STRICT STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'job_status_wrapper';

CREATE OR REPLACE FUNCTION ask."job_result"(
    "job_id" uuid
) RETURNS text
STRICT STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'job_result_wrapper';

CREATE OR REPLACE FUNCTION ask."job_error"(
    "job_id" uuid
) RETURNS text
STRICT STABLE PARALLEL SAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'job_error_wrapper';

CREATE OR REPLACE FUNCTION ask."cancel_job"(
    "job_id" uuid
) RETURNS bool
STRICT VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'cancel_job_wrapper';

CREATE OR REPLACE FUNCTION ask."run_pending_jobs"() RETURNS bigint
VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'run_pending_jobs_wrapper';

CREATE OR REPLACE FUNCTION ask."prune_jobs"(
    "older_than" text,
    "batch_size" int DEFAULT 10000
) RETURNS bigint
STRICT VOLATILE PARALLEL UNSAFE
LANGUAGE c
AS 'MODULE_PATHNAME', 'prune_jobs_wrapper';

-- prune_jobs AND run_pending_jobs are operator-only (match finalize.sql on
-- fresh installs). run_pending_jobs claims/runs jobs regardless of owner via
-- the operator-only worker-path helpers, so it must not be PUBLIC.
REVOKE ALL ON FUNCTION ask.prune_jobs(text, int) FROM PUBLIC;
REVOKE ALL ON FUNCTION ask.run_pending_jobs() FROM PUBLIC;
