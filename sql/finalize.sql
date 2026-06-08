-- pg_ask finalize SQL.
--
-- This script runs AFTER pgrx has emitted the schema for all #[pg_extern]
-- functions, which means the user-facing `ask.*` entry points exist and
-- can be GRANTed / REVOKEd here. Anything that needs to reference them
-- by name belongs in this file rather than bootstrap.sql.
--
-- ---------------------------------------------------------------------------
-- C6 (v0.5.2 review): config-surface lockdown.
--
-- `ask.config(key, value)` writes into ask._config; `ask.get_config(key)`
-- reads from it. pgrx emits both with the default EXECUTE TO PUBLIC, so
-- a role with USAGE on the `ask` schema but no operator privileges
-- could otherwise:
--   * read api_key out of the table fallback, or
--   * write its own api_key under another role's identity (the function
--     is SECURITY DEFINER, which would have stamped owner == definer).
--
-- The Rust `get_config` already redacts SECRET_KEYS before returning,
-- so this REVOKE is a second line of defence: even if a future change
-- forgets to redact, PUBLIC can't reach the function.
--
-- Operators who want to expose these to a specific role should issue:
--   GRANT EXECUTE ON FUNCTION ask.config(text, text) TO operator_role;
--   GRANT EXECUTE ON FUNCTION ask.get_config(text)   TO operator_role;
-- after CREATE EXTENSION.
REVOKE ALL ON FUNCTION ask.config(text, text)     FROM PUBLIC;
REVOKE ALL ON FUNCTION ask.get_config(text)       FROM PUBLIC;

-- ---------------------------------------------------------------------------
-- ask.prune_events(interval-text) deletes delivered outbox rows. It is a
-- destructive maintenance operation, so — like the config surface — it is
-- locked to operators rather than left at pgrx's default EXECUTE TO PUBLIC.
-- Grant it explicitly to whatever role runs your retention job:
--   GRANT EXECUTE ON FUNCTION ask.prune_events(text, int) TO maintenance_role;
-- ---------------------------------------------------------------------------
REVOKE ALL ON FUNCTION ask.prune_events(text, int) FROM PUBLIC;

-- ---------------------------------------------------------------------------
-- ask.prune_jobs(interval-text, int) deletes terminal async jobs. Same
-- destructive-maintenance rationale as prune_events → operator-only.
--   GRANT EXECUTE ON FUNCTION ask.prune_jobs(text, int) TO maintenance_role;
-- ---------------------------------------------------------------------------
REVOKE ALL ON FUNCTION ask.prune_jobs(text, int) FROM PUBLIC;

-- ---------------------------------------------------------------------------
-- ask.run_pending_jobs() drains the async queue synchronously: it claims and
-- runs jobs regardless of who enqueued them (it calls the operator-only
-- worker-path helpers). That cross-owner reach makes it an operator action,
-- not something to expose to every role — so it is locked down alongside the
-- worker-path helpers it depends on. Grant it to your pg_cron / maintenance
-- role explicitly:
--   GRANT EXECUTE ON FUNCTION ask.run_pending_jobs() TO maintenance_role;
-- The background worker (connects as superuser) does not need this grant.
--
-- IMPORTANT (H2): run_pending_jobs() is SECURITY INVOKER and internally calls
-- the operator-only worker-path helpers (_job_claim / _job_complete /
-- _job_fail / _job_recover_orphans / _job_release). A NON-superuser
-- maintenance role therefore needs EXECUTE on those too, or it will get
-- "permission denied" mid-drain. Grant the full set:
--   GRANT EXECUTE ON FUNCTION ask._job_claim(),
--                             ask._job_complete(uuid, text, bigint, bigint),
--                             ask._job_fail(uuid, text, int),
--                             ask._job_recover_orphans(int),
--                             ask._job_release(uuid)
--     TO maintenance_role;
-- A superuser cron role needs none of these grants.
-- ---------------------------------------------------------------------------
REVOKE ALL ON FUNCTION ask.run_pending_jobs() FROM PUBLIC;
