-- pg_ask initdb hook: create the extension on first start.
--
-- This file runs automatically when the Docker container initialises
-- a fresh data directory. It runs in the database named by
-- POSTGRES_DB (default: pg_ask_demo).
--
-- After the container starts, connect and configure a provider:
--
--   psql -h localhost -U postgres -d pg_ask_demo
--   SELECT ask.config('provider', 'anthropic');
--   SELECT ask.config('api_key',  'sk-ant-...');
--   SELECT ask.config('model',    'claude-sonnet-4-5');
--   SELECT ask.ask('how many tables are in this database?');
--
-- For the ZAI Anthropic-compatible endpoint (GLM-5.1, end-to-end verified):
--   SELECT ask.config('provider', 'anthropic');
--   SELECT ask.config('base_url', 'https://api.z.ai/api/anthropic');
--   SELECT ask.config('model',    'glm-5.1');
--   SELECT ask.config('api_key',  '<your-zai-key>');
--
-- See README.md for the full quickstart.

CREATE EXTENSION IF NOT EXISTS pg_ask;

-- Print a helpful banner visible in the container startup logs.
DO $$
BEGIN
    RAISE NOTICE '';
    RAISE NOTICE '──────────────────────────────────────────────';
    RAISE NOTICE '  pg_ask % installed in schema "ask"', ask.version();
    RAISE NOTICE '';
    RAISE NOTICE '  Next step: configure a provider.';
    RAISE NOTICE '  Example:';
    RAISE NOTICE '    SELECT ask.config(''provider'', ''anthropic'');';
    RAISE NOTICE '    SELECT ask.config(''api_key'',  ''sk-ant-...'');';
    RAISE NOTICE '    SELECT ask.ask(''hello!'');';
    RAISE NOTICE '──────────────────────────────────────────────';
    RAISE NOTICE '';
END
$$;
