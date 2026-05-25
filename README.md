# pg_ask

**Ask your PostgreSQL database in natural language.** An AI agent that runs
inside the database, reads your schema, plans SQL, executes it via SPI in the
current transaction, and synthesises an answer.

```sql
CREATE EXTENSION pg_ask;

SELECT pg_ask.config('provider', 'anthropic');
SELECT pg_ask.config('api_key',  'sk-ant-...');
SELECT pg_ask.config('model',    'claude-sonnet-4-5');

SELECT pg_ask.ask('How many orders shipped last week?');
-- "127 orders shipped between 2026-05-18 and 2026-05-24."

SELECT pg_ask.sql('top 5 customers by lifetime revenue');
-- SELECT customer_id, SUM(amount) AS revenue
-- FROM orders GROUP BY customer_id ORDER BY revenue DESC LIMIT 5;
```

> **Status:** v0.1 — early. Anthropic provider, single-shot agent loop,
> readonly SQL tool. OpenAI/Gemini, multi-turn sessions, and pgvector-backed
> long-term memory land next.

## Why

PostgreSQL has no native, agentic AI extension. The closest projects either
generate SQL without executing (`pg_ai_query`), focus on classical ML
(`postgresml`), or were archived in early 2026 (`timescaledb/pgai`).
`pg_ask` fills the gap: a pgrx-based extension that gives you the
`ask-your-database` experience as a single SQL function call.

## Architecture

```
SELECT pg_ask.ask('…')
        │
        ▼
  ┌─────────────────────────────────┐
  │ agent loop (src/agent.rs)       │
  │   ├─ schema::summarize()        │  ← pg_catalog
  │   ├─ provider.complete(…)       │  ← Anthropic / OpenAI / Gemini (HTTP)
  │   └─ tool dispatch              │
  │        └─ sql_query (SPI)       │  ← same backend, same transaction
  └─────────────────────────────────┘
```

- **Pure Rust + pgrx 0.18**, PostgreSQL 14–18.
- **SPI in caller's transaction** — tool reads are consistent with the
  surrounding query.
- **Readonly by default** — `sql_query` rejects anything that isn't
  `SELECT`/`WITH`/`EXPLAIN`/`TABLE`.
- **Cooperative cancellation** via `check_for_interrupts!()` between
  iterations, so `pg_cancel_backend` works.
- **No `unsafe` in our code**; all I/O is blocking `ureq` (PG backend is
  single-threaded, no async runtime).

## Install (development)

```bash
cargo install --locked cargo-pgrx --version ^0.18
cargo pgrx init                      # downloads + builds PG dev envs
cargo pgrx run pg18                  # spawns a psql shell against a temp PG18
```

Then in the psql shell:

```sql
CREATE EXTENSION pg_ask;
SELECT pg_ask.config('provider', 'anthropic');
SELECT pg_ask.config('api_key',  :'anthropic_key');
SELECT pg_ask.ask('list all tables and their row counts');
```

## Configuration

| Key              | Default                     | Notes                                          |
|------------------|-----------------------------|------------------------------------------------|
| `provider`       | *(required)*                | `anthropic` (v0.1). `openai`, `gemini` soon.   |
| `api_key`        | *(required)*                | Provider API key.                              |
| `model`          | `claude-sonnet-4-5`         | Model id, provider-specific.                   |
| `base_url`       | provider default            | For proxies / OpenAI-compatible endpoints.     |
| `max_tokens`     | `4096`                      | Per-completion cap.                            |
| `max_iterations` | `16`                        | Hard ceiling on the agent loop.                |
| `readonly`       | `true`                      | When `true`, `sql_query` refuses writes.       |

## Security

API keys are stored in `pg_ask._config`. Grant `USAGE` on the `pg_ask`
schema and `EXECUTE` on the public functions only to the roles that should
be able to ask. Internal tables `REVOKE ALL` from `PUBLIC` by default.

For multi-tenant or untrusted-caller scenarios, run with `readonly = true`
and gate `pg_ask.ask` behind a `SECURITY DEFINER` wrapper that pins the
search path and any RLS context you need.

## Roadmap

See [`docs/ROADMAP.md`](docs/ROADMAP.md).

## License

PostgreSQL License.
