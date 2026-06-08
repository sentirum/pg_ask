#!/usr/bin/env bash
# pg_ask initdb hook: optionally pre-configure the AI provider from
# environment variables so the container is ready without any manual
# ask.config() calls.
#
# Runs once on first-start (initdb phase), after 00-create-extension.sql.
#
# Recognised env vars:
#   PG_ASK_PROVIDER  — anthropic | openai | gemini | …
#   PG_ASK_BASE_URL  — override endpoint (e.g. https://api.z.ai/api/anthropic)
#   PG_ASK_MODEL     — model id (e.g. claude-sonnet-4-5, glm-5.1, gpt-4o)
#   PG_ASK_API_KEY   — provider API key (written as a secret; not echoed)
#
# All four are optional. Any that are empty are silently skipped.

set -euo pipefail

: "${POSTGRES_USER:=postgres}"
: "${POSTGRES_DB:=pg_ask_demo}"

run_sql() {
    psql --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" -c "$1"
}

configured=0

# Async job launcher support: the background-worker launcher runs in the
# 'postgres' maintenance DB and uses dblink to discover which databases have
# pg_ask installed (so it only spawns workers where there is work). Install
# dblink there if available; harmless when the async queue is unused. Without
# it the launcher falls back to probing every database via a short-lived
# worker (noisier, but still correct).
if psql --username "$POSTGRES_USER" --dbname postgres -tAc \
     "SELECT 1 FROM pg_available_extensions WHERE name='dblink'" | grep -q 1; then
    echo "[pg_ask] installing dblink in 'postgres' DB for the async job launcher"
    psql --username "$POSTGRES_USER" --dbname postgres \
         -c "CREATE EXTENSION IF NOT EXISTS dblink;" || true
fi

if [ -n "${PG_ASK_PROVIDER:-}" ]; then
    echo "[pg_ask] setting provider = $PG_ASK_PROVIDER"
    run_sql "SELECT ask.config('provider', '$PG_ASK_PROVIDER');"
    configured=1
fi

if [ -n "${PG_ASK_BASE_URL:-}" ]; then
    echo "[pg_ask] setting base_url = $PG_ASK_BASE_URL"
    run_sql "SELECT ask.config('base_url', '$PG_ASK_BASE_URL');"
    configured=1
fi

if [ -n "${PG_ASK_MODEL:-}" ]; then
    echo "[pg_ask] setting model = $PG_ASK_MODEL"
    run_sql "SELECT ask.config('model', '$PG_ASK_MODEL');"
    configured=1
fi

if [ -n "${PG_ASK_API_KEY:-}" ]; then
    echo "[pg_ask] setting api_key = <redacted>"
    # Use psql's \copy / extended protocol so the key value is never
    # visible in pg_stat_activity or shell history.
    psql --username "$POSTGRES_USER" --dbname "$POSTGRES_DB" \
         -c "SELECT ask.config('api_key', \$pgask\$$PG_ASK_API_KEY\$pgask\$);"
    configured=1
fi

if [ "$configured" -eq 1 ]; then
    echo "[pg_ask] provider configured. Test with:"
    echo "  psql -U $POSTGRES_USER -d $POSTGRES_DB -c \"SELECT ask.ask('hello!');\" "
else
    echo "[pg_ask] No PG_ASK_* env vars set — configure manually with ask.config()."
fi
