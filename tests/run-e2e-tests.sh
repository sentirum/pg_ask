#!/usr/bin/env bash
# pg_ask live provider E2E tests.
#
# Works with any OpenAI-compatible provider: MiniMax, ZAI, Kimi, Mimo, etc.
#
# Usage (examples):
#   # MiniMax
#   PG_ASK_API_KEY=xxx PG_ASK_BASE_URL="https://api.minimax.chat/v1" PG_ASK_MODEL="MiniMax-Text-01" ./tests/run-e2e-tests.sh
#
#   # ZAI (z.ai)
#   PG_ASK_API_KEY=xxx PG_ASK_BASE_URL="https://api.z.ai/v1" ./tests/run-e2e-tests.sh
#
#   # Kimi (Moonshot)
#   PG_ASK_API_KEY=xxx PG_ASK_BASE_URL="https://api.moonshot.cn/v1" PG_ASK_MODEL="moonshot-v1-8k" ./tests/run-e2e-tests.sh
#
#   # Mimo
#   PG_ASK_API_KEY=xxx PG_ASK_BASE_URL="https://api.mimo.com/v1" ./tests/run-e2e-tests.sh
#
#   # Ollama (local, no key needed)
#   PG_ASK_BASE_URL="http://host.docker.internal:11434/v1" PG_ASK_MODEL="qwen3" ./tests/run-e2e-tests.sh
#
# Prerequisites:
#   docker compose -f docker-compose.test.yml up --build -d

set -euo pipefail

COMPOSE_FILE="docker-compose.test.yml"
PSQL_ARGS="-U postgres -d pg_ask_test"

PROVIDER="${PG_ASK_PROVIDER:-openai}"
MODEL="${PG_ASK_MODEL:-}"
BASE_URL="${PG_ASK_BASE_URL:-}"
API_KEY="${PG_ASK_API_KEY:-}"

echo "══════════════════════════════════════════════════════════════"
echo "  pg_ask live provider E2E tests"
echo "  Provider: $PROVIDER"
echo "  Model:    ${MODEL:-(default)}"
echo "  Base URL: ${BASE_URL:-(default)}"
echo "══════════════════════════════════════════════════════════════"

# Wait for PG
echo "Waiting for PostgreSQL..."
for i in $(seq 1 30); do
    if docker compose -f "$COMPOSE_FILE" exec -T pg pg_isready $PSQL_ARGS >/dev/null 2>&1; then
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: PostgreSQL did not become ready in 60s"
        exit 1
    fi
    sleep 2
done

run_sql() {
    docker compose -f "$COMPOSE_FILE" exec -T pg psql $PSQL_ARGS -c "$1" 2>&1
}

run_sql_file() {
    docker compose -f "$COMPOSE_FILE" exec -T pg psql $PSQL_ARGS -f "$1" 2>&1
}

# Configure provider
echo "Configuring provider..."
run_sql "SELECT ask.config('provider', '$PROVIDER');"

if [ -n "$MODEL" ]; then
    run_sql "SELECT ask.config('model', '$MODEL');"
fi

if [ -n "$BASE_URL" ]; then
    run_sql "SELECT ask.config('base_url', '$BASE_URL');"
fi

if [ -n "$API_KEY" ]; then
    # Dollar-quote to avoid key exposure in pg_stat_activity
    run_sql "SELECT ask.config('api_key', \$\$${API_KEY}\$\$);"
else
    echo "WARNING: No API key set. Provider calls may fail."
fi

# Seed demo data
echo "Seeding demo data..."
run_sql_file /tests/03-seed-demo.sql

# Run E2E tests
echo ""
echo "Running E2E tests..."

PASS=0
FAIL=0

run_test() {
    local name="$1"
    local file="$2"
    echo ""
    echo "── $name ──"
    if run_sql_file "$file" 2>&1; then
        PASS=$((PASS + 1))
        echo "✅ $name PASSED"
    else
        FAIL=$((FAIL + 1))
        echo "❌ $name FAILED"
    fi
}

run_test "Live E2E" /tests/04-e2e-live.sql

echo ""
echo "══════════════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════════"

[ "$FAIL" -eq 0 ] || exit 1
