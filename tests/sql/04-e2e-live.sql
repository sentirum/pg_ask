-- pg_ask live provider E2E tests.
--
-- Full agent loop with a real LLM provider.
--
-- Run: psql -h localhost -p 15432 -U postgres -d pg_ask_test -f /tests/04-e2e-live.sql

\set ON_ERROR_STOP off
\pset pager off

\echo '═══════════════════════════════════════════════════════════════'
\echo '  pg_ask live provider E2E tests'
\echo '═══════════════════════════════════════════════════════════════'

-- ── 0. Verify provider is configured ────────────────────────────
\echo '── 0. Verify provider ────────────────────────────────────────'
SELECT COALESCE(ask.get_config('provider'), 'NOT SET') AS provider;
SELECT COALESCE(ask.get_config('model'), 'NOT SET') AS model;

-- ── 1. ask.ask() — full agent loop ──────────────────────────────
\echo '── 1. ask.ask() — full agent loop ────────────────────────────'
\timing on
SELECT ask.ask('How many customers are in the database?') AS ask_result;
\timing off

-- ── 2. ask.sql() — SQL generation only ──────────────────────────
\echo '── 2. ask.sql() — SQL generation only ────────────────────────'
\timing on
SELECT ask.sql('What are the top 5 customers by total order amount?') AS generated_sql;
\timing off

-- ── 3. ask.ask() — aggregation query ────────────────────────────
\echo '── 3. ask.ask() — aggregation query ──────────────────────────'
\timing on
SELECT ask.ask('How many orders are in each status? List them all.') AS agg_result;
\timing off

-- ── 4. ask.ask_stream() — streaming output ──────────────────────
\echo '── 4. ask.ask_stream() — streaming ───────────────────────────'
\timing on
SELECT * FROM ask.ask_stream('List all product categories with their average price') AS stream_event;
\timing off

-- ── 5. Multi-turn session ───────────────────────────────────────
\echo '── 5. Multi-turn session ──────────────────────────────────────'
SELECT ask.create_session('e2e analytics session') AS e2e_sid \gset
\echo 'Session:' :e2e_sid

\timing on
SELECT ask.chat(:'e2e_sid', 'How many products are in each category?') AS turn_1;
SELECT ask.chat(:'e2e_sid', 'Which category has the highest average price?') AS turn_2;
\timing off

SELECT ask.clear_session(:'e2e_sid') AS session_cleared;

-- ── 6. Token usage tracking (P4 fix verification) ───────────────
\echo '── 6. Token usage (P4 fix) ───────────────────────────────────'
SELECT
    count(*) FILTER (WHERE prompt_tokens IS NOT NULL) AS traces_with_tokens,
    sum(prompt_tokens) AS total_prompt_tokens,
    sum(completion_tokens) AS total_completion_tokens
  FROM ask._traces
 WHERE caller = session_user;

-- ── 7. Readonly mode ────────────────────────────────────────────
\echo '── 7. Readonly mode ───────────────────────────────────────────'
SET pg_ask.readonly = on;
SELECT ask.sql('Delete all customers') AS readonly_sql;
RESET pg_ask.readonly;

-- ── 8. Audit trail ──────────────────────────────────────────────
\echo '── 8. Audit trail ──────────────────────────────────────────────'
SELECT
    kind,
    count(*) AS call_count,
    avg(latency_ms)::int AS avg_latency_ms,
    sum(prompt_tokens) AS total_prompt_tokens
  FROM ask._traces
 WHERE caller = session_user
 GROUP BY kind
 ORDER BY kind;

SELECT
    tool_name,
    count(*) AS executions
  FROM ask._sql_audit
 WHERE caller = session_user
 GROUP BY tool_name
 ORDER BY tool_name;

-- ── 9. RLS verification (S6 fix) ────────────────────────────────
\echo '── 9. RLS — caller isolation ──────────────────────────────────'
SELECT count(*) AS all_traces_mine FROM ask._traces WHERE caller = session_user;
SELECT count(*) AS other_traces FROM ask._traces WHERE caller != session_user;

\echo ''
\echo '═══════════════════════════════════════════════════════════════'
\echo '  Live provider E2E tests complete'
\echo '═══════════════════════════════════════════════════════════════'
