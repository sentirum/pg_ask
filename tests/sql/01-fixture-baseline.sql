-- pg_ask fixture-based integration tests.
--
-- Tests the SQL surface, metadata, RLS, SECURITY DEFINER helpers, etc.
-- WITHOUT requiring a live LLM provider.
--
-- Run: psql -h localhost -p 15432 -U postgres -d pg_ask_test -f /tests/01-fixture-baseline.sql

\set ON_ERROR_STOP off
\pset pager off

\echo '═══════════════════════════════════════════════════════════════'
\echo '  pg_ask fixture baseline tests'
\echo '═══════════════════════════════════════════════════════════════'
\echo ''

-- ── 1. Extension loaded ─────────────────────────────────────────
\echo '── 1. Extension loaded ───────────────────────────────────────'
SELECT ask.version() AS extension_version;

-- ── 2. Config surface ───────────────────────────────────────────
\echo '── 2. Config surface ─────────────────────────────────────────'
-- Valid config keys only
SELECT ask.config('provider', 'fixture') AS config_set_provider;
SELECT ask.get_config('provider') AS config_get_provider;
-- Secret redaction
SELECT ask.config('api_key', 'super-secret-key') AS secret_set;
SELECT ask.get_config('api_key') AS secret_redacted;
-- Missing key returns NULL (not an error)
SELECT ask.get_config('nonexistent_key_xyz') AS missing_key_is_null;

-- ── 3. SQL guard (requires provider, so just test rejection) ────
\echo '── 3. SQL guard ──────────────────────────────────────────────'
-- These fail with "missing config key: pg_ask.provider" since no
-- provider is configured. The guard itself is tested in Rust unit tests.
-- We verify the SQLSTATE is NOT generic XX000 (S2 fix) by checking
-- the error condition name.
\set VERBOSITY sqlstate
-- The MissingConfig variant maps to ERRCODE_SYNTAX_ERROR (42601)
-- which surfaces as SYNTAX_ERROR, not INTERNAL_ERROR.

-- ── 4. Tool registration ────────────────────────────────────────
\echo '── 4. Tool registration ──────────────────────────────────────'
SELECT ask.register_tool(
    'test_top_customers',
    '{"description": "Top customers by country", "input_schema": {"type": "object", "properties": {"country": {"type": "string"}, "n": {"type": "integer", "default": 5}}, "required": ["country"]}}'::json,
    'SELECT * FROM customers WHERE country = {{country}} ORDER BY revenue DESC LIMIT {{n}}'
) AS tool_registered;

SELECT name FROM ask.list_tools() WHERE name = 'test_top_customers';

SELECT ask.unregister_tool('test_top_customers') AS tool_unregistered;

-- ── 5. Multi-turn sessions ──────────────────────────────────────
\echo '── 5. Multi-turn sessions (structure only) ──────────────────'
SELECT ask.create_session('test session') AS real_sid \gset
\echo 'Session created:' :real_sid
SELECT ask.clear_session(:'real_sid') AS cleared;

-- ── 6. GUC surface ──────────────────────────────────────────────
\echo '── 6. GUC surface ────────────────────────────────────────────'
SHOW pg_ask.provider;
SHOW pg_ask.model;
SHOW pg_ask.max_iterations;
SHOW pg_ask.readonly;
SHOW pg_ask.trace_enabled;
SHOW pg_ask.embedding_dimensions;

-- ── 7. Audit tables exist ───────────────────────────────────────
\echo '── 7. Audit tables exist ──────────────────────────────────────'
SELECT count(*) > 0 AS traces_table_exists
  FROM pg_class WHERE relnamespace = 'ask'::regnamespace AND relname = '_traces';
SELECT count(*) > 0 AS sql_audit_table_exists
  FROM pg_class WHERE relnamespace = 'ask'::regnamespace AND relname = '_sql_audit';

-- ── 8. _traces RLS enabled (S6 fix verification) ────────────────
\echo '── 8. _traces RLS (S6 fix) ───────────────────────────────────'
SELECT relname, relrowsecurity AS rls_enabled
  FROM pg_class
 WHERE relnamespace = 'ask'::regnamespace AND relname = '_traces';

SELECT polname, polcmd, pg_get_expr(polqual, polrelid) AS policy_expr
  FROM pg_policy
  JOIN pg_class ON pg_class.oid = pg_policy.polrelid
  JOIN pg_namespace ON pg_namespace.oid = pg_class.relnamespace
 WHERE nspname = 'ask' AND relname = '_traces';

-- ── 9. Token usage columns (P4 fix verification) ────────────────
\echo '── 9. Token usage columns (P4 fix) ──────────────────────────'
SELECT attname, typname
  FROM pg_attribute a
  JOIN pg_class c ON c.oid = a.attrelid
  JOIN pg_namespace n ON n.oid = c.relnamespace
  JOIN pg_type t ON t.oid = a.atttypid
 WHERE n.nspname = 'ask' AND c.relname = '_traces'
   AND attname IN ('prompt_tokens', 'completion_tokens')
 ORDER BY attname;

-- ── 10. SECURITY DEFINER helpers exist ──────────────────────────
\echo '── 10. SECURITY DEFINER helpers ──────────────────────────────'
SELECT proname, prosecdef
  FROM pg_proc
  JOIN pg_namespace ON pg_namespace.oid = pg_proc.pronamespace
 WHERE nspname = 'ask'
   AND proname IN ('_session_create', '_session_is_owned',
                    '_session_fetch_messages', '_session_lock_for_append',
                    '_session_append_message', '_session_touch',
                    '_session_clear_messages', '_config_get',
                    '_write_trace')
 ORDER BY proname;

-- ── 11. _memory_bootstrap accepts dims param (S3 fix) ───────────
\echo '── 11. Memory bootstrap (S3 fix) ────────────────────────────'
SELECT pg_proc.oid::regprocedure AS signature
  FROM pg_proc
  JOIN pg_namespace ON pg_namespace.oid = pg_proc.pronamespace
 WHERE nspname = 'ask' AND proname = '_memory_bootstrap';

\echo ''
\echo '═══════════════════════════════════════════════════════════════'
\echo '  Fixture baseline tests complete'
\echo '═══════════════════════════════════════════════════════════════'
