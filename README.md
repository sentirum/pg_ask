# pg_ask

[![CI](https://github.com/sentirum/pg_ask/actions/workflows/ci.yml/badge.svg)](https://github.com/sentirum/pg_ask/actions/workflows/ci.yml)
[![Docker](https://ghcr.io/sentirum/pg_ask)](https://github.com/sentirum/pg_ask/pkgs/container/pg_ask)
[![License: PostgreSQL](https://img.shields.io/badge/License-PostgreSQL-blue.svg)](LICENSE)

**Ask your PostgreSQL database in natural language.** An AI agent that runs
inside the database, reads your schema, plans SQL, executes it via SPI in the
current transaction, and synthesises an answer.

```sql
CREATE EXTENSION pg_ask;  -- installs into schema "ask"

SELECT ask.config('provider', 'anthropic');
SELECT ask.config('api_key',  'sk-ant-...');
SELECT ask.config('model',    'claude-sonnet-4-5');

SELECT ask.ask('How many orders shipped last week?');
-- "127 orders shipped between 2026-05-18 and 2026-05-24."

SELECT ask.sql('top 5 customers by lifetime revenue');
-- SELECT customer_id, SUM(amount) AS revenue
-- FROM orders GROUP BY customer_id ORDER BY revenue DESC LIMIT 5;
```

> Note: SQL identifiers live in the `ask` schema (since v0.5.1). GUC keys
> still live under `pg_ask.*` (e.g. `SET pg_ask.provider = '…'`) because
> Postgres ties a GUC namespace to the extension name, not its install
> schema.

> **Status:** v0.5.6 — adds the `ask.emit()` event outbox for reverse
> notifications (ADR-0017), on top of the v0.5.5 agent-loop efficiency
> release: pinned `search_path`,
> bare-name prompting, higher iteration ceiling, graceful budget
> finalisation. Builds on the v0.5.4 `ask.status()` capability
> handshake (`api_level = 1`) for external orchestrators, on top of the v0.5.3
> hardening release. v0.5 feature set
> (Anthropic + OpenAI + Gemini chat, OpenAI / Voyage / Gemini embeddings,
> pgvector-backed long-term memory, multi-turn sessions, audit log,
> agent loop, readonly SQL tool, HTTP fetch with SSRF defence,
> sample-table + user-defined tools, RLS-aware schema dump, column
> redaction, streaming SRF, real SQL parser) **plus** 25 security /
> correctness / performance fixes across three review waves, **plus** 10 additional
> hardening fixes in v0.5.3 (SQLSTATE-aware errors, token usage tracking,
> embedding retry/backoff, _traces RLS, dynamic embedding dimensions,
> user-tool caching, soft empty-response recovery). End-to-end
> verified against a live PG18 backend with ZAI GLM-5.1 over the
> Anthropic-compatible endpoint, including a four-turn `ask.chat`
> anaphora canary. 90/90 tests green. See [`CHANGELOG.md`](CHANGELOG.md).

## Why

PostgreSQL has no native, agentic AI extension. The closest projects either
generate SQL without executing (`pg_ai_query`), focus on classical ML
(`postgresml`), or were archived in early 2026 (`timescaledb/pgai`).
`pg_ask` fills the gap: a pgrx-based extension that gives you the
`ask-your-database` experience as a single SQL function call.

## Architecture

```
SELECT ask.ask('…')
        │
        ▼
  ┌─────────────────────────────────┐
  │ agent loop (src/agent/)         │
  │   ├─ schema::summarize()        │  ← pg_catalog (cached, 60s TTL)
  │   ├─ provider.complete(…)       │  ← Anthropic / OpenAI / Gemini (HTTP)
  │   └─ tool dispatch              │
  │        ├─ sql_query  (SPI)      │  ← runs inside a Postgres subtxn,
  │        ├─ sample_table          │     statement_timeout + readonly
  │        ├─ describe_table        │     scoped per call
  │        ├─ http_fetch            │  ← SSRF: URL parser + IP CIDR check
  │        ├─ recall (pgvector)     │
  │        └─ user-defined tool     │  ← {{key}} → $N parameterised
  └─────────────────────────────────┘
```

- **Pure Rust + pgrx 0.18**, PostgreSQL 14–18 (production tested on PG18).
- **SPI in caller's transaction** — tool reads are consistent with the
  surrounding query.
- **Readonly by default** — `sql_query` rejects anything that isn't
  `SELECT`/`WITH`/`EXPLAIN`/`TABLE`.
- **Each tool call wrapped in a Postgres subtransaction** (`src/infra/subtxn.rs`)
  — a failed model-emitted query no longer poisons the agent loop.
- **Cooperative cancellation** via `check_for_interrupts!()` between
  iterations, so `pg_cancel_backend` works.
- **No `unsafe` outside `src/infra/subtxn.rs`**; all I/O is blocking `ureq`
  (PG backend is single-threaded, no async runtime).

## Install from packages

All packages are published to [Cloudsmith](https://cloudsmith.io). Browse them
on the public page:
[cloudsmith.io/~sentirum/repos/pg_ask](https://cloudsmith.io/~sentirum/repos/pg_ask/packages/).

Packages are built for **PostgreSQL 16, 17, and 18** — the package name
encodes the major (e.g. `postgresql-17-pg-ask`, `pg_ask_16`), so just swap
the number to match your server. (Exception: the Alpine/APK channel is
PG 18 only — see that section below.)

### APT (Debian / Ubuntu)

```bash
# Sets up the keyring + source list for your distro automatically
curl -sLf 'https://dl.cloudsmith.io/public/sentirum/pg_ask/cfg/setup/bash.deb.sh' \
  | sudo bash

sudo apt install postgresql-18-pg-ask   # or -17- / -16- for your PG major
```

Supported: **Debian 12 (bookworm)**, **Debian 13 (trixie)**,
**Ubuntu 22.04 (jammy)**, **Ubuntu 24.04 (noble)** — `amd64` + `arm64`,
for PG 16 / 17 / 18.

### RPM (RHEL / Rocky / Alma / Fedora)

```bash
# Sets up the yum/dnf repo for your distro automatically
curl -sLf 'https://dl.cloudsmith.io/public/sentirum/pg_ask/cfg/setup/bash.rpm.sh' \
  | sudo bash

sudo dnf install pg_ask_18      # or pg_ask_17 / pg_ask_16; yum also works
```

Supported: **EL 8 / EL 9** (RHEL, Rocky, Alma) `x86_64` + `aarch64`,
**Fedora 42** `x86_64` — for PG 16 / 17 / 18.

### APK (Alpine)

```bash
# Sets up the /etc/apk/repositories entry + signing key automatically
curl -sLf 'https://dl.cloudsmith.io/public/sentirum/pg_ask/cfg/setup/bash.apk.sh' \
  | sudo bash

sudo apk add pg_ask18
```

Supported: **Alpine edge** `x86_64`, **PG 18 only**. (Alpine has a single
un-versioned `pg_config` that always points at the newest installed
major, so per-major builds aren't possible the way they are on
Debian/RHEL — PG 16/17 users should use the APT, RPM, or Docker channels.
The setup script configures the `edge` repo for you.)

Then, in psql:

```sql
CREATE EXTENSION pg_ask;
SELECT ask.config('provider', 'anthropic');
SELECT ask.config('api_key',  'sk-ant-...');
SELECT ask.ask('how many tables are in this database?');
```

## Docker (quickest start)

```bash
# Pull and run (no Rust toolchain needed)
docker run -d \
  --name pg_ask \
  -e POSTGRES_PASSWORD=secret \
  -e PG_ASK_PROVIDER=anthropic \
  -e PG_ASK_API_KEY=sk-ant-... \
  -p 5432:5432 \
  ghcr.io/sentirum/pg_ask:latest-pg18

# Ask a question
psql -h localhost -U postgres -d pg_ask_demo \
  -c "SELECT ask.ask('how many tables are in this database?');"
```

Or with **docker compose** (env file supported):

```bash
PG_ASK_PROVIDER=anthropic PG_ASK_API_KEY=sk-ant-... \
  docker compose up -d
```

See [`docker-compose.yml`](docker-compose.yml) for all options.

The image is published to two registries (multi-arch, `amd64` + `arm64`)
with a tag per PostgreSQL major — use whichever you prefer:

```bash
# GitHub Container Registry
ghcr.io/sentirum/pg_ask:latest-pg18          # or latest-pg17 / latest-pg16

# Cloudsmith
docker.cloudsmith.io/sentirum/pg_ask/pg_ask:latest-pg18
```

## Install (development)

```bash
cargo install --locked cargo-pgrx --version ^0.18
cargo pgrx init                      # downloads + builds PG dev envs
cargo pgrx run pg18                  # spawns a psql shell against a temp PG18
```

> **macOS PG18 note:** `cargo pgrx init --pg18 download` fails on Homebrew
> systems unless ICU is on `PKG_CONFIG_PATH`. Run once:
>
> ```bash
> brew install icu4c
> export PKG_CONFIG_PATH="$(brew --prefix icu4c)/lib/pkgconfig"
> cargo pgrx init --pg18 download
> ```
>
> On Linux, `apt install libicu-dev` (or distro equivalent) is enough.

Then in the psql shell:

```sql
CREATE EXTENSION pg_ask;
SELECT ask.config('provider', 'anthropic');
SELECT ask.config('api_key',  :'anthropic_key');
SELECT ask.ask('list all tables and their row counts');
```

### Install against an existing PostgreSQL

```bash
git clone https://github.com/sentirum/pg_ask
cd pg_ask
cargo pgrx install --release --features pg18    # writes into $PGHOME/lib
psql -c 'CREATE EXTENSION pg_ask;'
```

### Upgrade to 0.5.9

```sql
ALTER EXTENSION pg_ask UPDATE TO '0.5.9';
```

Adds the async job queue (`ask._jobs` + `ask.ask_async()`, ADR-0018): a
background worker drains pending jobs with `FOR UPDATE SKIP LOCKED`,
retries transient failures up to `pg_ask.jobs_max_attempts`, and reclaims
orphaned jobs whose worker died. Opt-in (see
[Async jobs](#async-jobs-v059)). The upgrade chain `0.5.7 → 0.5.8 → 0.5.9`
is applied automatically; `ALTER EXTENSION … UPDATE` without a target
goes straight to the control file's `default_version`.

#### Earlier: 0.5.8

```sql
ALTER EXTENSION pg_ask UPDATE TO '0.5.8';
```

Production-hardens the event outbox (`ask._outbox` + `ask.emit()`) for
reverse notifications (ADR-0017): `ask._outbox_emit` becomes the single
SECURITY DEFINER authority, flood control runs under a transaction-scoped
advisory lock, and validation moves into SQL so the fresh-install and
upgrade paths can't drift. Opt-in via `pg_ask.events_enabled`.

#### Earlier: 0.5.7

```sql
ALTER EXTENSION pg_ask UPDATE TO '0.5.7';
```

**Security release.** Closes several ways a low-privilege role could read
secrets or reach blocked resources. Most importantly, `ask._config_get()`
no longer leaks `api_key` / `embedding_api_key` to non-superusers — the
upgrade script redefines it so the fix reaches **existing** databases, not
just fresh installs. Also hardens `sql_guard` against quoted-identifier
bypass and the SSRF guard against IPv4-compatible IPv6. Upgrading is
strongly recommended. See [`CHANGELOG.md`](CHANGELOG.md).

#### Earlier: 0.5.6

```sql
ALTER EXTENSION pg_ask UPDATE TO '0.5.6';
```

Adds the first cut of the event outbox (`ask._outbox` + `ask.emit()`) for
reverse notifications (ADR-0017). Opt-in via `pg_ask.events_enabled`. See
[`CHANGELOG.md`](CHANGELOG.md).

#### Earlier: 0.5.5

```sql
ALTER EXTENSION pg_ask UPDATE TO '0.5.5';
```

Performance release: pins `search_path` to the introspected schemas and
teaches the model to use bare table names, cutting wasted schema-discovery
iterations; raises `max_iterations` 16→ 24; finalises gracefully at the
limit. All changes live in the library; the SQL upgrade script is a no-op.
See [`CHANGELOG.md`](CHANGELOG.md).

#### Earlier: 0.5.4

```sql
ALTER EXTENSION pg_ask UPDATE TO '0.5.4';
```

Additive release: adds the `ask.status()` capability handshake
(`api_level = 1`) and `ask.status_api_level()`. No table, GUC, or
existing-signature changes; the SQL upgrade script is a documented no-op
(the new functions ship in the library and are granted to PUBLIC by the
generated schema). See [`CHANGELOG.md`](CHANGELOG.md).

## Quickstart — choose a provider

Any OpenAI-compatible, Anthropic-compatible, or Gemini endpoint works. The
extension recognises the following provider aliases out of the box;
anything else can be reached by setting `provider` to the closest
protocol family and overriding `base_url`.

```sql
-- Anthropic
SELECT ask.config('provider', 'anthropic');
SELECT ask.config('model',    'claude-sonnet-4-5');
SELECT ask.config('api_key',  :'anthropic_key');

-- ZAI (Anthropic-compatible) with GLM-5.1 — end-to-end verified
SELECT ask.config('provider', 'anthropic');
SELECT ask.config('base_url', 'https://api.z.ai/api/anthropic');
SELECT ask.config('model',    'glm-5.1');
SELECT ask.config('api_key',  :'zai_key');

-- OpenAI (also Groq, Together, Mistral, Ollama, vLLM, LM Studio)
SELECT ask.config('provider', 'openai');           -- or 'groq', 'ollama', …
SELECT ask.config('model',    'gpt-4o-mini');
SELECT ask.config('api_key',  :'openai_key');
-- For self-hosted / proxy endpoints, additionally set
SELECT ask.config('base_url', 'http://localhost:11434/v1');

-- Gemini
SELECT ask.config('provider', 'gemini');           -- or 'google'
SELECT ask.config('model',    'gemini-1.5-pro');
SELECT ask.config('api_key',  :'gemini_key');
```

Then ask:

```sql
SELECT ask.ask('what is the total revenue per category last month?');
```

## Configuration

| Key              | Default                     | Notes                                          |
|------------------|-----------------------------|------------------------------------------------|
| `provider`       | *(required)*                | `anthropic` · `openai` (also `groq`/`together`/`mistral`/`ollama`/`vllm`/`lmstudio` via `base_url`) · `gemini` (also `google`, `google-genai`). |
| `api_key`        | *(required)*                | Provider API key. Redacted from `get_config` and `pg_settings`. |
| `model`          | `claude-sonnet-4-5`         | Model id, provider-specific.                   |
| `base_url`       | provider default            | For proxies / OpenAI- or Anthropic-compatible endpoints (e.g. `https://api.z.ai/api/anthropic`). |
| `max_tokens`     | `4096`                      | Per-completion cap.                            |
| `max_iterations` | `24`                        | Hard ceiling on the agent loop.                |
| `readonly`       | `true`                      | When `true`, `sql_query` refuses writes.       |

For the full list (timeouts, allow-lists, embedding config, schema
budget, sensitive-column redaction, …) see
[`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) § Configuration model.

### Function reference

The full public API surface, grouped by area. All functions live in the
`ask` schema.

**Ask**

| Function | Returns | Notes |
|----------|---------|-------|
| `ask.ask(question text)` | `text` | Run the agent loop inline and return the answer. |
| `ask.sql(question text)` | `text` | Like `ask`, but returns just the generated SQL. |
| `ask.preview(question text, …)` | `text` | Dry-run: show the plan/SQL without executing writes. |
| `ask.ask_stream(question text)` | `setof text` | Stream the answer row-by-row as it is produced. |

**Async jobs** (v0.5.9, opt-in — see [Async jobs](#async-jobs-v059))

| Function | Returns | Notes |
|----------|---------|-------|
| `ask.ask_async(question text, kind text default 'ask')` | `uuid` | Enqueue a job; returns its id (NULL if the queue is disabled). |
| `ask.job_status(job_id uuid)` | `text` | `pending` · `running` · `done` · `failed`. |
| `ask.job_result(job_id uuid)` | `text` | The answer once `done`. |
| `ask.job_error(job_id uuid)` | `text` | The error once `failed`. |
| `ask.cancel_job(job_id uuid)` | `bool` | Cancel a pending job. |
| `ask.prune_jobs(older_than text, batch_size int default 10000)` | `bigint` | Delete terminal jobs older than an interval. |

**Sessions**

| Function | Returns | Notes |
|----------|---------|-------|
| `ask.create_session(label text default null)` | `uuid` | Start a multi-turn session. |
| `ask.chat(session_id uuid, message text)` | `text` | Send a turn; context carries across turns. |
| `ask.clear_session(session_id uuid)` | `bool` | Wipe a session's history. |

**Config**

| Function | Returns | Notes |
|----------|---------|-------|
| `ask.config(key text, value text)` | `bool` | Set a config value (stored in `ask._config`). |
| `ask.get_config(key text)` | `text` | Read a value; secrets are redacted. |

**Memory** (v0.3, opt-in)

| Function | Returns | Notes |
|----------|---------|-------|
| `ask.remember(content text, namespace text default null, metadata jsonb default null)` | `uuid` | Store a memory. |
| `ask.recall(query text, namespace text default null, limit int default 5)` | `setof` | Hybrid-search recall. |
| `ask.forget(id uuid)` | `bool` | Delete one memory. |
| `ask.list_memories(…)` / `ask.list_namespaces()` | `setof` | Browse stored memories. |

**Events** (v0.5.8 outbox, opt-in — see [Reverse notifications](#reverse-notifications-event-outbox))

| Function | Returns | Notes |
|----------|---------|-------|
| `ask.emit(event text, payload jsonb default null, summary text default null)` | `uuid` | Publish an event to `ask._outbox` + `NOTIFY`. |
| `ask.prune_events(older_than text, batch_size int default 10000)` | `bigint` | Delete processed events older than an interval. |

**Tools & introspection**

| Function | Returns | Notes |
|----------|---------|-------|
| `ask.register_tool(name text, spec jsonb, body text)` | `bool` | Register a custom SQL-backed tool. |
| `ask.unregister_tool(name text)` | `bool` | Remove a custom tool. |
| `ask.list_tools()` | `setof (name, spec)` | List registered tools. |
| `ask.status()` | `jsonb` | Self-describing capability handshake (secret-free). |
| `ask.status_api_level()` | `int` | Numeric API level for orchestrators. |
| `ask.version()` | `text` | Extension version string. |

### Multi-turn sessions

```sql
SELECT ask.create_session('product analytics') AS sid \gset

SELECT ask.chat(:'sid', 'list the distinct product categories');
SELECT ask.chat(:'sid', 'of those, which has the highest total revenue?');
SELECT ask.chat(:'sid', 'who bought it? give me each customer name and units');
SELECT ask.chat(:'sid', 'average revenue per buyer for that category in dollars');
```

The session tracks every assistant turn and tool result so cross-turn
anaphora ("of those", "it", "that category") resolves naturally.
`ask._sessions.owner` defaults to `current_user`; existence and
unauthorised access collapse to the same error, so id-space probing
leaks no information.

### Async jobs (v0.5.9)

`ask.ask()` runs the agent loop inline, so the calling session blocks
until the answer is ready. For long-running questions — or to fire a
question and collect the answer later — `ask.ask_async()` enqueues the
work onto `ask._jobs` and a background worker runs it out of band
(ADR-0018).

It is **opt-in** and needs two things: the background worker must be
loaded via `shared_preload_libraries` (a one-time restart), and the queue
must be enabled. An install that doesn't use async pays nothing.

```bash
# 1. Load the worker (preserve any existing preload list) and restart once.
psql -c "ALTER SYSTEM SET shared_preload_libraries = 'pg_ask';"
psql -c "ALTER SYSTEM SET pg_ask.jobs_enabled = on;"
sudo systemctl restart postgresql   # or: pg_ctlcluster <ver> main restart
```

On startup you should see `pg_ask launcher started` in the server log.
Then enqueue and poll:

```sql
-- Returns a job UUID immediately (NULL if the queue is disabled).
SELECT ask.ask_async('weekly revenue per category, as a short report') AS job \gset

-- Poll until the worker finishes (status: pending → running → done | failed).
SELECT status, attempts, answer, error
FROM   ask._jobs
WHERE  id = :'job';
```

The worker claims jobs atomically with `FOR UPDATE SKIP LOCKED`, retries
transient failures up to `pg_ask.jobs_max_attempts` (default 3), and a
job whose worker dies mid-run is reclaimed from `running` back to
`pending` once `started_at` exceeds the orphan timeout. `ask._jobs.owner`
defaults to `current_user` and the table is row-level-security scoped, so
callers see only their own jobs.

### Reverse notifications (event outbox)

Where `ask.ask()` is request/response, the **event outbox** is the
reverse channel: it lets the database (or the agent, mid-run) push a
durable, ordered event out to an external listener (ADR-0017, hardened in
0.5.8). `ask.emit()` writes a row to `ask._outbox` **and** fires
`NOTIFY pg_ask_events` in the same transaction — the row is the source of
truth, the NOTIFY is just a low-latency wake-up, so nothing is lost if no
listener is connected.

It is **opt-in** via `pg_ask.events_enabled` (no worker or restart
needed — unlike async jobs, this is a plain in-transaction write):

```sql
ALTER SYSTEM SET pg_ask.events_enabled = on;
SELECT pg_reload_conf();

-- Emit an event (e.g. from a trigger, a job, or the agent).
SELECT ask.emit(
  'order.flagged',
  '{"order_id": 4711, "reason": "velocity"}'::jsonb,
  'Order 4711 flagged for manual review'   -- optional human summary
);
```

A consumer either `LISTEN pg_ask_events` for push delivery, or polls the
outbox directly and stamps `processed_at` as it drains:

```sql
-- Drain pending events, oldest first (a partial index keeps this cheap).
SELECT id, ts, event, payload, summary
FROM   ask._outbox
WHERE  processed_at IS NULL
ORDER  BY ts
LIMIT  100;
```

`ask._outbox_emit` is the single SECURITY DEFINER writer; it enforces
event-name validation, payload-size limits, dedup, and rate limiting
under a transaction-scoped advisory lock, so a misbehaving caller can't
flood the channel. Use `ask.prune_events('30 days')` to trim delivered
history.

### Long-term memory (v0.3, optional)

`pg_ask` ships an opt-in memory layer backed by
[pgvector](https://github.com/pgvector/pgvector). Install the extension
first (`CREATE EXTENSION vector;`) and pick an embedding provider:

```sql
SET pg_ask.embedding_provider  = 'openai';      -- or voyage / gemini /
                                                 -- together / vllm / ollama / ...
SET pg_ask.embedding_api_key   = '...';          -- separate from chat key
SET pg_ask.embedding_model     = 'text-embedding-3-small';
SET pg_ask.embedding_dimensions = 1536;          -- must match _memories col width

SELECT ask.remember('User prefers concise SQL answers.');
SELECT * FROM ask.recall('what does the user prefer?');
SELECT * FROM ask.list_namespaces();
SELECT * FROM ask.list_memories(namespace := 'analytics', limit_n := 20);
SELECT ask.forget('uuid-here'::uuid);
```

The agent itself is wired up: when memory is configured and pgvector is
installed, `ask.ask(...)` exposes a `recall` tool to the model so it
can pull relevant past context into the conversation on its own.

## Security

API keys are stored in `ask._config` and are returned redacted via
`ask.get_config('api_key')`. Internal tables (`_config`, `_sessions`,
`_messages`, `_memories`, `_tools`, `_sql_audit`, `_traces`)
`REVOKE ALL FROM PUBLIC` by default and all writes go through
`SECURITY DEFINER` helpers that enforce `session_user` ownership.

For multi-tenant or untrusted-caller scenarios, run with `readonly = true`
and gate `ask.ask` behind a `SECURITY DEFINER` wrapper that pins the
search path and any RLS context you need. See
[`docs/SECURITY.md`](docs/SECURITY.md) for the full threat model and
the production hardening checklist.

## Observability

```sql
-- Every ask.ask / sql / preview / chat call lands a row
SELECT ts, kind, provider, model, iterations, latency_ms,
       prompt_tokens, completion_tokens,
       substring(question for 60) AS q
FROM ask._traces
ORDER BY ts DESC LIMIT 20;

-- Every SQL the model executed, with timing and row count
SELECT ts, tool_name, readonly, row_count, latency_ms,
       substring(query for 80) AS query
FROM ask._sql_audit
WHERE caller = current_user
ORDER BY ts DESC LIMIT 20;
```

## Capability handshake (for external orchestrators)

Since v0.5.4, `pg_ask` exposes a single self-describing entry point so an
external agent platform can discover, in one secret-free round-trip,
whether a database is ready and how it is configured — without probing
pg_ask internals or risking exposure of the API key.

```sql
SELECT ask.status();
-- {
--   "extension": "pg_ask",
--   "version": "0.5.6",
--   "api_level": 1,            -- contract version for shape-gating
--   "ready": true,             -- ask.ask() callable right now?
--   "can_use": true,           -- caller has USAGE on schema ask
--   "provider_configured": true,
--   "provider": "anthropic",   -- name only, NEVER the api_key
--   "model": "claude-sonnet-4-5",
--   "readonly": true,
--   "memory_available": false,
--   "capabilities": ["ask", "sql", "chat", "preview", "register_tool"],
--   "limits": { "max_iterations": 24, "tool_max_rows": 200 },
--   "health": "ok"            -- ok | needs_config
-- }

SELECT ask.status_api_level();  -- 1  (cheap integer probe)
```

`ask.status()` is `STABLE`, granted to `PUBLIC`, and never raises on a
half-configured install — a freshly created extension with no provider
set returns `ready: false`, `health: "needs_config"`, so a caller can
guide the operator through setup instead of hitting an error. The
response contains `provider_configured` as a boolean and the provider
*name*; the API key is never returned.

This is the contract the
[senti-ai-agent](https://github.com/sentirum/senti-ai-agent) platform
uses to treat any pg_ask-enabled database as an `ask_database` tool
(natural-language / SQL-only / multi-turn chat), probe its readiness,
and offer guided installation — across a fleet of databases, each
carrying its own provider config inside the database.

## Docs

- [`CHANGELOG.md`](CHANGELOG.md) — release-by-release diff, including the
  v0.5.4 capability handshake and the v0.5.2 / v0.5.3 hardening lists.
- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — module layout, layering
  rules, request lifecycle, trait contracts, configuration table.
- [`docs/SECURITY.md`](docs/SECURITY.md) — threat model, defence layers,
  production hardening checklist.
- [`docs/ROADMAP.md`](docs/ROADMAP.md) — milestone plan, v0.1 → v0.5.6,
  what's next.

## License

PostgreSQL License — see [`LICENSE`](LICENSE).
