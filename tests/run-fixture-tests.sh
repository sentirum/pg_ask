#!/usr/bin/env bash
# pg_ask fixture-based integration tests.
#
# Tests the SQL surface, RLS, SECURITY DEFINER helpers, and metadata
# without requiring a live LLM provider.
#
# Usage:
#   ./tests/run-fixture-tests.sh
#
# Prerequisites:
#   Docker Compose environment running:
#   docker compose -f docker-compose.test.yml up --build -d

set -euo pipefail

COMPOSE_FILE="docker-compose.test.yml"
PSQL_ARGS="-h localhost -p 15432 -U postgres -d pg_ask_test"

echo "══════════════════════════════════════════════════════════════"
echo "  pg_ask fixture integration tests"
echo "══════════════════════════════════════════════════════════════"

# Wait for PG to be ready
echo "Waiting for PostgreSQL..."
for i in $(seq 1 30); do
    if docker compose -f "$COMPOSE_FILE" exec -T pg pg_isready -U postgres -d pg_ask_test >/dev/null 2>&1; then
        echo "PostgreSQL is ready."
        break
    fi
    if [ "$i" -eq 30 ]; then
        echo "ERROR: PostgreSQL did not become ready in 60s"
        exit 1
    fi
    sleep 2
done

PASS=0
FAIL=0

run_test() {
    local name="$1"
    local file="$2"
    echo ""
    echo "── $name ──"
    if docker compose -f "$COMPOSE_FILE" exec -T pg psql $PSQL_ARGS -f "$file" 2>&1; then
        PASS=$((PASS + 1))
        echo "✅ $name PASSED"
    else
        FAIL=$((FAIL + 1))
        echo "❌ $name FAILED"
    fi
}

run_test "Fixture baseline" /tests/01-fixture-baseline.sql
run_test "RLS isolation" /tests/02-rls-isolation.sql

echo ""
echo "══════════════════════════════════════════════════════════════"
echo "  Results: $PASS passed, $FAIL failed"
echo "══════════════════════════════════════════════════════════════"

[ "$FAIL" -eq 0 ] || exit 1
