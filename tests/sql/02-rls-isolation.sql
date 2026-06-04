-- pg_ask RLS / security integration tests.
--
-- Verifies multi-tenant isolation: different roles should only see
-- their own traces.
--
-- Run: psql -h localhost -p 15432 -U postgres -d pg_ask_test -f /tests/02-rls-isolation.sql

\set ON_ERROR_STOP off
\pset pager off

\echo '═══════════════════════════════════════════════════════════════'
\echo '  pg_ask RLS / security isolation tests'
\echo '═══════════════════════════════════════════════════════════════'

-- Create test roles
DO $$
BEGIN
    CREATE ROLE tenant_a LOGIN PASSWORD 'test';
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

DO $$
BEGIN
    CREATE ROLE tenant_b LOGIN PASSWORD 'test';
EXCEPTION WHEN duplicate_object THEN NULL;
END $$;

GRANT USAGE ON SCHEMA ask TO tenant_a, tenant_b;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA ask TO tenant_a, tenant_b;

-- ── 1. _traces RLS: tenant isolation ────────────────────────────
\echo '── 1. _traces RLS: tenant isolation ──────────────────────────'

-- As superuser, insert trace rows for different callers
INSERT INTO ask._traces (caller, kind, question, iterations, tool_calls, provider, latency_ms)
VALUES ('tenant_a', 'ask', 'tenant a question', 1, '[]'::jsonb, 'test', 100);

INSERT INTO ask._traces (caller, kind, question, iterations, tool_calls, provider, latency_ms)
VALUES ('tenant_b', 'ask', 'tenant b question', 1, '[]'::jsonb, 'test', 100);

-- Superuser sees everything (bypasses RLS)
SELECT count(*) AS superuser_sees_all FROM ask._traces WHERE caller IN ('tenant_a', 'tenant_b');

-- tenant_a sees only their own
\connect pg_ask_test tenant_a
SELECT count(*) AS tenant_a_sees_own FROM ask._traces WHERE caller = 'tenant_a';
SELECT count(*) AS tenant_a_cannot_see_b FROM ask._traces WHERE caller = 'tenant_b';

-- tenant_b sees only their own
\connect pg_ask_test tenant_b
SELECT count(*) AS tenant_b_sees_own FROM ask._traces WHERE caller = 'tenant_b';
SELECT count(*) AS tenant_b_cannot_see_a FROM ask._traces WHERE caller = 'tenant_a';

-- ── 2. Session ownership (through SECURITY DEFINER helper) ─────
\connect pg_ask_test tenant_a
\echo '── 2. Session ownership ──────────────────────────────────────'

-- Create session through the public API (SECURITY DEFINER helper)
SELECT ask.create_session('tenant a session') AS tenant_a_sid \gset
\echo 'Session created:' :tenant_a_sid

-- _session_is_owned should return true for tenant_a
SELECT ask._session_is_owned(:'tenant_a_sid') AS owns_own_session;

-- ── 3. Cleanup ──────────────────────────────────────────────────
\connect pg_ask_test postgres
\echo '── 3. Cleanup ──────────────────────────────────────────────────'

-- Clean up test data before dropping roles
DELETE FROM ask._traces WHERE caller IN ('tenant_a', 'tenant_b');
DELETE FROM ask._sessions WHERE owner IN ('tenant_a', 'tenant_b');
REVOKE USAGE ON SCHEMA ask FROM tenant_a, tenant_b;
REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA ask FROM tenant_a, tenant_b;
DROP ROLE IF EXISTS tenant_a;
DROP ROLE IF EXISTS tenant_b;

\echo ''
\echo '═══════════════════════════════════════════════════════════════'
\echo '  RLS / security isolation tests complete'
\echo '═══════════════════════════════════════════════════════════════'
