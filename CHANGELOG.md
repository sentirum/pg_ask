# Changelog

All notable changes to `pg_ask` are recorded here. Format loosely follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); the project
itself follows [Semantic Versioning](https://semver.org/) for the public
SQL surface (`ask.*` functions, `pg_ask.*` GUCs, `ask._*` tables) and
treats internal Rust modules as private regardless of `pub` visibility.

Upgrade scripts ship as `sql/pg_ask--<from>--<to>.sql` and run
automatically under `ALTER EXTENSION pg_ask UPDATE`.

## [0.5.8] — 2026-06-08 — Event outbox production hardening (`ask.emit`)

Production-hardens the ADR-0017 event outbox without touching its
consumer-facing contract: the `pg_ask_events` channel, the `ask._outbox`
columns, the pending-row query (`WHERE processed_at IS NULL ORDER BY ts`),
and the `ask._outbox_emit(text,jsonb,text)` / `ask._outbox_mark_processed`
/ `ask.emit` signatures are all unchanged, so an existing
`LISTEN pg_ask_events` consumer keeps working across the upgrade. Upgrade
with `ALTER EXTENSION pg_ask UPDATE TO '0.5.8';`.

### Security

- **Bypass closed (`ask._outbox_emit`)**: validation, the `events_enabled`
  switch, and the `pg_notify` wake-up now ALL live inside the SECURITY
  DEFINER `ask._outbox_emit`, which is the single authority for writing an
  event. Previously these lived only in the Rust `ask.emit` wrapper, so a
  role with `EXECUTE` on the (PUBLIC-granted) helper could call it directly
  to write a newline-laced event name, a multi-megabyte payload, or a row
  while events were globally disabled — and without firing a NOTIFY. The
  Rust layer is now pure defense-in-depth.

### Added

- **Input validation on `ask.emit`**, owned solely by `ask._outbox_emit`
  (the single authority — the Rust layer does no size/charset checks, so
  there is nothing to drift): event names must be non-empty, `<= 127` chars,
  and match `[A-Za-z0-9][A-Za-z0-9._:-]*` (rejects whitespace / control
  chars that could corrupt the durable log or a listener's routing).
  `summary` is capped at 8192 **bytes** (`octet_length`, exact for
  multi-byte text). Payload size is capped by the new
  `pg_ask.events_max_payload_bytes` GUC (default 64 KiB; `0` disables),
  measured as serialized-JSON bytes. Violations raise
  `invalid_parameter_value` *before* any row is written.
- **Flood control** via two opt-in GUCs, both default `0` (off):
  `pg_ask.events_max_per_minute` (per-`(emitter, event)` rate cap) and
  `pg_ask.events_dedup_window_ms` (collapse identical
  `(emitter, event, payload)` repeats, compared via `md5(payload::text)`).
  Both are plain integers (e.g. `60000`), not unit-suffixed — the SQL writer
  reads them via `current_setting()::int`.
  A suppressed emit is a silent no-op returning `NULL` — it never raises,
  so a trigger's transaction is never rolled back, and emits a
  `RAISE DEBUG` line so operators can observe suppression under
  `log_min_messages = debug1`. Checks run atomically inside
  `ask._outbox_emit` under a transaction-scoped advisory lock to avoid a
  check-then-insert race. The lock uses the two-argument
  `pg_advisory_xact_lock(domain, key)` form (domain = `'pg_ask._outbox'`),
  so it shares no key space with the int8 session lock and can never
  collide across lock domains.
- **Indexes** for the new hot paths: `_outbox_rate_idx (emitter, event, ts)`
  so the rate-limit / dedup checks never full-scan the outbox on emit, and
  the partial `_outbox_processed_idx (processed_at) WHERE processed_at IS
  NOT NULL` so retention prunes hit an index instead of a seq scan.
- **Retention**: `ask.prune_events('<interval>'[, batch_size])` (e.g.
  `'30 days'`) plus the SECURITY DEFINER `ask._outbox_prune(interval, int)`
  helper. Deletes only already-delivered rows (`processed_at IS NOT NULL`)
  older than the interval, **in batches** (default 10000; `0` = single
  DELETE) to bound each DELETE's per-statement memory, lock acquisition,
  and dead-tuple churn. Note: as a plpgsql function the whole loop runs in
  the caller's single transaction, so batching does NOT shorten lock
  lifetime or split WAL across commits — for that, call it repeatedly from
  separate transactions. Pending rows are never removed. Locked to
  operators (not granted to PUBLIC).

### Changed

- `pg_notify` is fired inside `ask._outbox_emit` (was a separate Rust SPI
  call) and is `pg_catalog`-qualified, so a hostile `search_path` cannot
  shadow it and a direct helper call can't write a row without waking
  listeners.
- Dedup now compares `md5(payload::text)` instead of the `jsonb =` operator
  — stable for logically-equal payloads (jsonb normalizes key order /
  whitespace before the text cast) and far cheaper than an equality scan
  over large jsonb values.
- `ask.emit` doc-comments and SQL comments are now consumer-agnostic
  ("an external `LISTEN pg_ask_events` consumer") rather than naming a
  specific downstream; the orchestrator coupling lives only in ADR-0017.

## [0.5.7] — 2026-06-06 — Security hardening pass

A security / code-review pass closing several ways a low-privilege role
could read secrets or reach blocked resources. Upgrade with
`ALTER EXTENSION pg_ask UPDATE TO '0.5.7';` — the SQL upgrade script
redefines `ask._config_get()` so the fix reaches existing databases (not
just fresh installs).

### Security

- **Secret leakage via `ask._config_get()`**: the SECURITY DEFINER helper
  was granted to PUBLIC with no key filtering, so any role could read
  `api_key` / `embedding_api_key` in plaintext from `ask._config`.
  Reimplemented in plpgsql to reject reads of secret keys by
  non-superusers. (Carried to existing installs by
  `sql/pg_ask--0.5.6--0.5.7.sql`.)
- **`sql_guard` banned-function bypass via quoted identifiers**: the
  lexer-fallback `no_banned_functions` only matched bare word tokens, so a
  banned call written as `"pg_sleep"(10)` slipped through when the AST
  parser bailed on the surrounding syntax. Now matches quoted identifiers
  too.
- **SSRF via IPv4-compatible IPv6**: the private-network guard used
  `to_ipv4_mapped`, missing IPv4-compatible addresses like `::127.0.0.1`.
  Switched to `to_ipv4`, covering both mapped and compatible forms.

### Fixed

- **List config settings ignored the table fallback**: `http_allow_list`
  and `sensitive_columns` only read their GUC, so values set via
  `ask.config(...)` (stored in `ask._config`) were silently ignored. They
  now use the same GUC-then-table fallback as the scalar settings.
- **`embedding_api_key` fallback**: defaults to the main `api_key` when the
  embedding provider matches the main provider.
- **`describe_table` ignored `search_path`**: an unqualified table name was
  forced to the `public` schema instead of being resolved against
  `search_path`. Now resolved via `pg_catalog.pg_to_regclass`, so
  unqualified names follow `search_path` the way SQL does.

## [0.5.6] — 2026-06-06 — Event outbox for reverse notifications

Adds the pg_ask side of pg_ask → senti reverse alerting (ADR-0017): a
durable event outbox plus `ask.emit()`, so triggers / scheduled jobs can
report in-database conditions to an external orchestrator. pg_ask stays
ignorant of the consumer — it only deposits events in its own database;
who listens decides ownership, which keeps multi-tenant isolation
automatic. Opt-in via `pg_ask.events_enabled` (default off). Upgrade with
`ALTER EXTENSION pg_ask UPDATE TO '0.5.6';` — unlike 0.5.4/0.5.5 the SQL
upgrade script creates real objects (the `ask._outbox` table + writer
helpers). Verified live end-to-end against the demo DB, including a DB
trigger emitting a low-stock alert that a senti listener drained, and the
cross-role consumer path (emitter ≠ consumer). 90 pg_tests green.

### CI / tooling

- Restore CI to green: `cargo fmt` across 12 files (the rustfmt check had
  failed since v0.5.3, blocking the pipeline before clippy / pgrx test).
- Fixture tests fixed: ambiguous `oid` reference in the memory-bootstrap
  step (`pg_proc.oid`), and `run-fixture-tests.sh` now connects via the
  in-container socket with `ON_ERROR_STOP=1` so failing statements fail
  the run instead of silently passing.
- Bump GitHub Actions to Node 24 runtimes (checkout/cache/upload-artifact/
  download-artifact `v4 → v5`) ahead of the June 16 2026 Node 20 removal.

### Added — event outbox for reverse notifications (ADR-0017)

- **`ask.emit(event, payload, summary)`** — append a durable row to the new
  `ask._outbox` table and fire `pg_notify('pg_ask_events', <id>)`, so an
  external orchestrator (senti) can react to in-database conditions. The
  NOTIFY carries only the row id (8 KB cap); the body is read from the
  outbox. No-op returning NULL unless `pg_ask.events_enabled = on` (default
  off, opt-in). Intended for triggers / scheduled jobs that have already
  decided a condition is worth reporting — keep threshold logic in SQL and
  use `summary := ask.ask('…')` only when a human-readable line is wanted.
- **`ask._outbox` table** + SECURITY DEFINER writers `ask._outbox_emit`
  (append) and `ask._outbox_mark_processed` (idempotent consumer stamp).
  Readable by PUBLIC (payload is operator-authored, no secrets); writable
  only through the helpers. A partial index keeps the pending-rows query
  cheap as processed history accumulates.
- **`pg_ask.events_enabled` GUC** (bool, default off) master switch.
- pg_ask itself never contacts senti: it only deposits events in its own
  database. Who listens decides ownership, which keeps multi-tenant /
  agent isolation automatic. See ADR-0017 in the senti-ai-agent repo.

## [0.5.5] — 2026-06-05 — Agent-loop efficiency & schema-resolution optimization

Performance-focused release. The dominant cost on multi-schema databases
was the model probing `information_schema` / `pg_tables` and assuming the
`public` schema to locate tables, which burned iterations (and on tight
budgets, hit `agent exceeded max iterations`). This release pins
`search_path` to the introspected schemas, teaches the model to use bare
table names, raises the iteration ceiling, and finalises gracefully at the
limit. All changes ship in the Rust library; the SQL upgrade script is a
documented no-op (`ALTER EXTENSION pg_ask UPDATE TO '0.5.5'`). Verified
live against a multi-schema demo DB over ZAI GLM: schema-discovery queries
dropped ~55% → ~10% of tool calls and complex multi-table questions that
took 7–16 iterations now answer in ~3. 86 pg_tests green.

### Changed — token & latency optimization (schema resolution)

- **Pin `search_path` to the introspected schemas before every tool query.**
  The agent now derives the active schema set from the schema dump and
  applies `SET LOCAL search_path = "…"` inside each `sql_query` /
  `sample_table` subtransaction. Bare table names from the model resolve
  no matter which schema they live in, so the model no longer has to
  discover or guess schemas. Identifiers are double-quote escaped; the
  clause is scoped to the subtxn so it never leaks into the surrounding
  transaction, and readonly enforcement is unaffected.
- **Prompt: tell the model the search_path is set and to use BARE table
  names.** The schema-adjacent hint now says the path is already configured
  and to reference tables as `orders` (never `public.orders` or
  `schema.orders`), since a qualified `public.x` would bypass the pin. This
  pairs with the pin above to remove the single biggest iteration sink.
- **`sample_table.schema` is now optional.** Omitting it samples a bare
  table name via the pinned path instead of forcing the model to supply
  (and often mis-guess) a schema.
- **Measured impact** on a multi-schema demo DB: schema-discovery queries
  (`information_schema` / `pg_tables` probing) dropped from ~55% of all
  tool calls to ~10%; complex multi-table questions that previously took
  7–16 iterations now answer in ~3 when the search_path is sane.

### Changed — agent-loop efficiency & resilience

- **Prompt: stop wasting iterations on schema discovery.** The system
  prompt now explicitly tells the model that the schema dump lists every
  table fully qualified as `schema.table`, to use those exact names, and
  to NOT assume the `public` schema or re-query `pg_catalog` /
  `information_schema`. The single biggest cause of `agent exceeded max
  iterations` was the model assuming `public`, failing to find tables in
  a non-`public` schema, and burning its whole budget re-discovering the
  catalog. Also nudges the model toward one complete answering query
  (JOINs/CTEs/window functions) instead of many small exploratory ones,
  and toward fixing a failed query in place rather than restarting.
- **`max_iterations` default raised 16 → 24.** Genuinely complex,
  multi-table questions occasionally need more than 16 tool round-trips;
  the previous default cut them off. Still overridable via
  `pg_ask.max_iterations`.
- **Graceful finalisation instead of a hard error at the budget limit.**
  When the iteration budget is exhausted, the agent now gets one final
  tool-free turn instructed to answer from what it already gathered.
  This converts many `MaxIterations` failures into a useful (if caveated)
  answer; it only falls back to the `54000` error if the model still
  produces nothing.

## [0.5.4] — 2026-06-05 — Capability handshake for external orchestrators

Additive, minor release. Adds a single self-describing introspection
entry point so an external agent platform (senti-ai) can discover an
install's readiness and configuration in one secret-free round-trip,
plus a companion api-level probe. No table, GUC, or existing-signature
changes; the whole feature ships in the Rust library and is granted to
PUBLIC by the pgrx-generated schema. Upgrade with
`ALTER EXTENSION pg_ask UPDATE TO '0.5.4';` (the SQL upgrade script is a
documented no-op — the new functions are created by the generated
schema). End-to-end verified from a live senti-ai build calling
`ask_database` over ZAI GLM, exercising the answer / sql_only / chat
modes and the needs_config → setup → ready flow.

### Added

- **`ask.status()` capability handshake** (`api_level = 1`). A single,
  secret-free, never-raising JSON entry point that lets an external
  orchestrator (e.g. senti-ai) discover in one round-trip whether the
  install is `ready`, only `needs_config`, or lacks schema access — plus
  `version`, `provider` (name only, never the key), `model`, `readonly`,
  `memory_available`, `capabilities`, and `limits`. Granted to PUBLIC
  because it returns `provider_configured` as a boolean and never the
  api_key. Companion `ask.status_api_level()` returns the integer contract
  version for cheap shape-gating. New module `src/infra/status.rs` holds
  the snapshot logic; `src/api/status.rs` is the thin wrapper.

## [0.5.3] — 2026-05-26 — Regression-fix release for v0.5.2 hardening

A single-purpose patch release. v0.5.2's hardening sweep introduced
four observable regressions for non-superuser callers (two of them
shipping blockers in multi-tenant deployments) and missed one
performance regression in `hybrid_search`. The findings came from a
Gemini code review of the v0.5.2 artefacts; each item was reproduced
live against the demo database with a dedicated `pgask_test_user`
role before the fix landed, and re-verified after.

All fixes are minimal in scope. The public surface
(`ask.*` function signatures, `pg_ask.*` GUCs, internal table
shapes) is unchanged from v0.5.2. Test count holds at **75 green**,
plus an end-to-end non-superuser anaphora canary that runs through
the ZAI Anthropic endpoint with GLM-5.1 (`7 → ×3 = 21 → +5 = 26 →
÷2 = 13`).

### Regression fixes

- **C2-bis** — *Session feature unusable for non-superusers.*
  `ask.create_session` / `chat` / `clear_session` were
  `SECURITY INVOKER` `pg_extern`s that issued direct INSERT /
  UPDATE / DELETE on `ask._sessions` and `ask._messages`. The
  v0.5.2 `REVOKE ALL ON ask._sessions FROM PUBLIC` turned every
  non-superuser call into `ERROR: permission denied for table
  _sessions`. Fix: a family of SECURITY DEFINER helpers
  (`ask._session_create`, `_session_is_owned`,
  `_session_fetch_messages`, `_session_lock_for_append`,
  `_session_append_message`, `_session_touch`,
  `_session_clear_messages`) mirror the existing `_memory_*` /
  `_tool_*` / `_sql_audit_*` family. The Rust caller
  (`src/session/store.rs`) was rewritten to call only these
  helpers. Each enforces `session_user` ownership inside its body,
  so EXECUTE-to-PUBLIC stays safe.

- **C3-bis** — *`ask.get_config` unusable for non-superusers.*
  `config(key, value)` was SECURITY DEFINER in v0.5.2 but its
  read sibling `get_config(key)` stayed SECURITY INVOKER. Combined
  with `REVOKE ALL ON ask._config FROM PUBLIC` that meant every
  `get_config()` call from a non-superuser returned
  `permission denied for table _config`. Fix on two levels: the
  Rust `#[pg_extern]` now annotates `security_definer`, and the
  internal `RuntimeConfig::load` path routes through a new
  `ask._config_get(lookup_key)` SECURITY DEFINER helper (so the
  agent's own provider / model / api_key lookups inside
  `ask.chat()` / `ask.ask()` also stop tripping the table grant).
  Redaction (`is_secret(key) → '***redacted***'`) still runs on the
  Rust side after the read, so the new `SECURITY DEFINER` shape
  buys only table access, not a path to leak secrets.

- **HP1** — *User-defined tools lacked subtxn isolation, readonly
  enforcement, and statement_timeout.* `tools::user_defined::run_planned`
  ran the operator-blessed body directly through `Spi::connect`, so
  (a) any Postgres ERROR poisoned the surrounding `ask.ask()`
  transaction with `current transaction is aborted, commands
  ignored`, (b) the body could issue DML even when
  `pg_ask.readonly = on`, and (c) a runaway body had no per-call
  timeout. Fix mirrors the `sql_query` / `sample_table` pattern:
  wrap the body in `infra::subtxn::run_in_subtransaction` and apply
  `SET LOCAL statement_timeout` plus (when readonly)
  `SET LOCAL transaction_read_only = on` from inside the subtxn
  so the GUCs auto-revert on release. `UserDefinedTool` carries
  `readonly` / `statement_timeout_ms` snapshots threaded from
  `RuntimeConfig` at registration time.

- **H13** — *Schema cache was role-agnostic, leaking views across
  `SET ROLE`.* `schema::summarize_within` keyed its per-backend
  cache on `(char_budget, ttl_start)`, but `compute_summary`
  filters tables through `has_table_privilege(current_user, …)`.
  Two roles connected through a `SET ROLE` (or a connection pooler
  re-using backends) could see each other's view for up to 60
  seconds. Fix: cache key now includes `pg_sys::GetUserId()` so
  the cache segments by effective role. The unsafe FFI call lives
  in a wrapper documented alongside `infra::subtxn` (the only two
  places in the crate that touch raw `pgrx-pg-sys`).

- **HP2** — *`hybrid_search` bypassed the pgvector ANN index.* The
  H9 fix in v0.5.2 introduced a two-stage hybrid query, but the
  candidate ORDER BY referenced the query vector through a CTE
  (`(SELECT vec FROM q)`). pgvector's ivfflat / hnsw plan only
  triggers when the right-hand side of `<=>` is a literal or a
  *direct* bind parameter; a subquery looked variable to the
  planner and forced a sequential scan + sort. Fix: drop the `q`
  CTE for the vector, reference `$1::vector` directly in every
  occurrence. The tsquery still goes through a CTE because it's
  used twice and is non-trivial to recompute, but it sits outside
  the ORDER BY that drives the index choice.

### Dependencies

- **`ureq` 2.10 → 3** — ureq 3.x is an API redesign
  (typestate builder, response body via `body_mut().as_reader()`,
  `StatusCode` error variant no longer carries the response).
  Migration is hidden inside `src/infra/http.rs`; the public
  `HttpClient` surface is unchanged, every provider / tool
  compiles untouched. Pinned to `rustls + json` features to match
  the 2.x build profile.
- **`thiserror` 1 → 2** — backwards-compatible upgrade, no code
  changes required.

### Upgrade notes

`ALTER EXTENSION pg_ask UPDATE TO '0.5.3'` from any 0.5.x. The
migration script (`sql/pg_ask--0.5.2--0.5.3.sql`) is idempotent
and includes:

* All seven `ask._session_*` SECURITY DEFINER helpers (idempotent
  `CREATE OR REPLACE`).
* `ALTER FUNCTION ask.get_config(text) SECURITY DEFINER` to flip
  the catalog flag without detaching the function from the
  extension.
* `ask._config_get(lookup_key)` SECURITY DEFINER helper for the
  internal `RuntimeConfig::load` read path.
* `_traces` token-usage columns (`prompt_tokens`, `completion_tokens`).
* `_traces` RLS policy (`caller = session_user`) — S6 fix.
* `_write_trace` updated to persist token-usage data.
* Matching `REVOKE / GRANT EXECUTE` policy.

The Rust-side fixes (HP1, H13, HP2, ureq, thiserror, plus the
S2/P2/P3/P4/D6 hardening items below) ship in the new `.so` and take
effect the moment `ALTER EXTENSION UPDATE` swaps the library.

### Hardening fixes (code review follow-up)

- **S2** — *`AskError` had no SQLSTATE — all errors surfaced as
  `ERRCODE_INTERNAL_ERROR`.* Added `AskError::sql_error_code()` mapping
  each variant to a meaningful PostgreSQL SQLSTATE (e.g.
  `GuardRejected → 42501`, `InvalidConfig → 22023`,
  `MaxIterations → 54000`). All `#[pg_extern]` entry points now use
  `raise_as_pg_error()` instead of `pgrx::error!()` so monitoring tools
  and `WHEN ... SQLSTATE ...` handlers can discriminate errors.

- **S3/S4** — *`embedding_dimensions` hardcoded to 1536 and `lists = 100`
  in bootstrap.* `_memory_bootstrap()` now accepts an explicit `dims int`
  parameter (default 1536 for backward compatibility). The Rust caller
  passes `pg_ask.embedding_dimensions` GUC value. On mismatch with an
  existing table, the helper surfaces an actionable error with the ALTER
  command. Index `lists` computed dynamically via `greatest(10, least(4000, N))`.

- **S6** — *`_traces` had GRANT SELECT TO PUBLIC with no RLS.* Added
  `ALTER TABLE ask._traces ENABLE ROW LEVEL SECURITY` + policy
  `_traces_owner_select USING (caller = session_user)`. Superusers bypass
  RLS per standard PG behaviour.

- **P2** — *Embedding API had no retry/backoff.* All three embedding providers
  (OpenAI, Voyage, Gemini) now retry transient failures (429, 5xx, transport)
  with exponential backoff (base 200ms × 2^attempt, max 3 retries) and jitter.
  Non-retriable errors (4xx other than 429, JSON parse failures) surface
  immediately.

- **P3** — *User-defined tools loaded via SPI on every `ask()` call.*
  Added per-backend thread-local TTL cache (5s, user-keyed). Cache miss
  triggers the existing SPI query; cache hit skips the round-trip.

- **P4** — *Token usage not tracked.* `ProviderResponse` now carries an
  optional `TokenUsage { prompt_tokens, completion_tokens }` populated
  from each provider's `usage` response field. The agent loop accumulates
  totals across iterations and writes them to `_traces` via two new
  columns (`prompt_tokens int`, `completion_tokens int`).

- **D6** — *Empty tool calls caused hard error.* Instead of
  `AskError::EmptyResponse`, the loop now checks for text content first
  (returns it if present) or sends a nudge message giving the model a
  self-correction opportunity.

- **O3** — *`rust-version` missing from Cargo.toml.* Added
  `rust-version = "1.82"` matching pgrx 0.18's minimum.

- **Dependency refresh** — pgrx `=0.18.0` → `=0.18.1` and 17 transitive
  packages updated to latest compatible versions. Zero audit findings.

## [0.5.2] — 2026-05-25 — Hardening release

A pure-hardening release: no new public surface, 25 fixes across
security, correctness, and performance. Every change shipped on
`main` was verified end-to-end against a live PostgreSQL 18 backend
with the ZAI Anthropic endpoint + GLM-5.1, including a four-turn
`ask.chat` anaphora canary ("of those" / "it" / "that category"
all resolved correctly). Test count: 68 → **75 green**.

### Security — Wave 1 (Critical)

- **C1** `sql_guard` is now parser-authoritative. When the
  `sqlparser` 0.62 (PostgreSQL dialect) walk succeeds, the lexer
  fallback is skipped entirely; statement type, multi-statement
  rejection, denylist checks, CTE rules, and EXPLAIN handling all
  run off the AST. The fallback exists strictly for parse errors on
  non-standard syntax.
- **C2** `ask._messages`, `ask._sessions`, `ask._memories`,
  `ask._tools`, `ask._sql_audit`, `ask._traces`: `REVOKE ALL FROM
  PUBLIC` is now the default, and writes go through
  `SECURITY DEFINER` helpers (`ask._write_trace`,
  `ask._session_append`, `ask._memory_insert`,
  `ask._tool_upsert`, `ask._sql_audit_begin`, `ask._sql_audit_finish`).
  A single missed `WHERE owner = current_user` predicate can no
  longer leak rows across roles.
- **C3** `ask.config` / `ask.get_config` redact `api_key` and
  `embedding_api_key` before returning anything to the caller.
  `ask._config` is `REVOKE SELECT FROM PUBLIC`; reads go through
  `ask.get_config(key)` which case-insensitively matches a small
  redact set.
- **C4** User-defined tools (`ask.register_tool`) compile their
  `{{key}}` placeholders into `$N` parameters at registration time
  and execute via `SPI_execute_with_args`. The model's argument
  values can no longer escape into SQL syntax, however creatively
  they're quoted. Repeated placeholders share a parameter index;
  jsonb composites are passed as `jsonb`, not as text.
- **C5** `http_fetch` SSRF defence: hostnames go through a real URL
  parser (`url` crate) before the allow-list match (host equality +
  optional path-prefix, not string `starts_with`), and every
  resolved IP is checked against an IPv4 + IPv6 private/loopback/
  link-local/CGNAT CIDR set before the request is issued.
- **C6** `pg_ask.api_key` / `pg_ask.embedding_api_key` GUCs marked
  `SUPERUSER_ONLY | NO_SHOW_ALL`. `pg_settings` and `SHOW ALL`
  redact them for non-superusers; `pg_stat_activity.query` was
  already safe because the value is never inlined into SQL.
- **C7** `ask._traces`, `ask.session.list`, `ask.tools.list`,
  `ask.memory.*` SRFs all enforce `WHERE caller / owner =
  current_user` at the SQL level. Existence and unauthorised
  access collapse to the same error so id-space probing leaks no
  information.
- **C8** SQL audit row (`ask._sql_audit`) is now inserted *before*
  the readonly GUC is flipped (see also the v0.5.1 fix), with the
  insert in a separate `Spi::connect_mut` scope from the per-call
  GUC setup, so an extreme-load `statement_timeout` on the INSERT
  itself cannot leak the readonly flag back into the parent
  transaction.

### Security — Wave 2 (High)

- **H1** `sql_query` and `sample_table` accept a fresh
  `RuntimeConfig` snapshot per call instead of re-reading config
  inside the tool; per-row preview / per-call timeout / max-rows
  limits cannot be raced by a concurrent `SET LOCAL`.
- **H2** `src/infra/subtxn.rs` (the only module permitted to use
  raw `pgrx_pg_sys` FFI) wraps Postgres
  `BeginInternalSubTransaction` / `Release…` / `RollbackAndRelease…`
  inside a safe `run_in_subtransaction(name, body)` helper.
  `sql_query::run_query_to_text` now runs the model-emitted
  statement inside that subtxn. A failed query (typo, missing
  column, permission denied, statement_timeout, divide-by-zero,
  …) used to abort the parent transaction and poison every
  subsequent SPI call in the agent loop with
  `current transaction is aborted, commands ignored`; the failure
  is now contained, the tool returns `is_error` to the model, the
  loop keeps going, and `audit_finish` runs normally. Shape
  mirrors plpython's `PLy_spi_subtransaction_{begin,commit,abort}`
  (memory context + resource owner snapshot/restore; catch via
  pgrx `PgTryBuilder` which flushes `ErrorState` automatically).
- **H3** (partial) per-call `statement_timeout` now lives inside
  the H2 subtxn. Full audit-row latency stamp under readonly is
  deferred — `transaction_read_only` is a transaction-wide flag
  that even a subtxn cannot flip back off (confirmed by
  guc.c's `check_transaction_read_only` hook returning
  `ERRCODE_ACTIVE_SQL_TRANSACTION`). Documented limitation.
- **H4** `tools::recall` now goes through the embedding provider
  with `embedding_dimensions` validated against the actual response
  width at call time; a misconfigured `embedding_model` surfaces
  loudly instead of silently writing zero-vectors.
- **H9** `infra::http::HttpClient` reuses a single shared
  `ureq::Agent` per (connect_timeout, read_timeout) pair via a
  process-level cache (`HttpClient::shared_agent`), so each agent
  loop does not spin up a fresh TLS context per provider call.
  `Mutex` poison handled with `unwrap_or_else(|e| e.into_inner())`.
- **H10** EXPLAIN goes through the parser and is rejected whenever
  the inner statement isn't a `SELECT`/`WITH`/`TABLE` (the model
  used to be able to slip `EXPLAIN INSERT …` past the prefix check).
- **H11** Token-budget renderer never falls back to the verbose
  `pg_attribute`/`pg_class`/`pg_description` walk when the compact
  rendering already fits — the redundant SPI round-trip cost up to
  hundreds of milliseconds on 500-table schemas.
- **H12** `recall` tool obeys the global `pg_ask.tool_max_rows`
  cap before it ever reaches the model, with a hard ceiling of 25
  results regardless of caller-requested `limit_n`.

### Critical bug fix — SET LOCAL readonly leak

Manual smoke testing against DeepInfra exposed a real-world
crash: every `ask.ask()` / `ask.preview()` in readonly mode (the
default) errored with

```
25006: cannot execute INSERT in a read-only transaction
```

as soon as `telemetry::write` tried to land the trace row. Root
cause: three call sites issued `SET LOCAL transaction_read_only =
on` inside a `Spi::connect_mut` block thinking the LOCAL scope was
the SPI block. It is not — `SET LOCAL` is scoped to the enclosing
*transaction*, so the flag persisted for the rest of the outer
call and broke every subsequent INSERT (trace row, session turn,
the next tool's audit insert, …).

Affected sites: `src/tools/sql_query.rs::audit_begin`,
`src/tools/sample_table.rs::run_sample`,
`src/planner/explain.rs::run`. Fix moves the per-call SET LOCALs
into the subtxn that already wraps the user query (sql_query +
sample_table) or wraps EXPLAIN in a fresh subtxn (`planner::explain`).
When the subtxn releases, Postgres pops its GUC stack frame and
the parent transaction sees the original
`transaction_read_only` / `statement_timeout` values again.

Regression test: `readonly_ask_does_not_leak_transaction_read_only`
forces `trace_enabled=on`, runs `ask.ask`, asserts the trace row
landed AND a manual `CREATE TEMP TABLE` + `INSERT` in the same
backend after `ask.ask` still works.

### Performance — Wave 3

- **P1** `RuntimeConfig` is loaded exactly once per top-level
  `ask.ask` / `ask.preview` / `ask.chat` call and threaded through
  `agent::run_with_cfg` / `run_stream_with_cfg` /
  `memory::recall_with_cfg`. Previously the `with_trace` closure +
  `agent::run` + the first memory tool each rebuilt the snapshot,
  costing 2-3× the GUC reads.
- **P2** Schema summary is memoized per backend in a `thread_local
  Cell` with a 60-second TTL keyed by `char_budget`. A 500-table
  schema went from 40 ms warm / hundreds of ms cold to a single
  Atomic load on the hot path.
- **P3** `pgvector_installed` probe latches `true` in an
  `AtomicBool` and short-circuits; `false` is *not* memoized so
  mid-session `CREATE EXTENSION vector` still works.
- **P4** `HttpClient` pool: process-level cache of `ureq::Agent`s
  keyed on `(connect_timeout_ms, total_timeout_ms)`. TLS state +
  connection pool now amortise across the entire agent loop and
  across `ask.*` calls in the same backend.
- **P5** `telemetry::write` builds the trace row from a single
  payload struct and INSERTs it via `_write_trace(jsonb)`; the
  previous version round-tripped each column individually.
- **P6** `tools::sql_query` and `tools::sample_table` build their
  result text via `String::with_capacity` + `push_str`, not
  repeated `format!`. On wide result sets the saving is real
  because the cell-redaction pass already pre-counts cells.
- **P8** `providers::gemini::extract_function_name` now returns
  `Result<&str, AskError>` instead of silently rewriting an
  unknown id to `"sql_query"`. The previous behaviour fed the
  model a tool result with the wrong tool name attached, which
  hides bugs and produces hallucinated follow-up calls.

### Review item #11 — Stream output truncation

`ask.ask_stream()` pushed the full tool result text into a single
`SetOfIterator` element via `[tool] {} → {}`. A `sql_query` that
returned 500 rows produced a multi-hundred-KB single line, which
(a) blew past the libpq reply buffer for streaming consumers and
(b) was unreadable anyway. The truncation pattern already used by
`ToolCallTrace::from_call` is now hoisted into a shared
`telemetry::truncate_tool_output` helper (cap exported as
`TOOL_OUTPUT_PREVIEW_CHARS = 2000`), and both surfaces — the
persisted trace row and the live stream — go through it. The
model itself still sees the full output via `history`, so its
next reasoning step has complete information. Char-boundary safe
(verified by a UTF-8 multibyte regression test with repeated
`ç` glyphs at the cut point).

### Infrastructure

- `src/infra/subtxn.rs` added. The single module in the project
  permitted to use raw `pgrx_pg_sys` FFI. Header spells out the
  whole-program invariants reviewers must preserve; every unsafe
  block carries a per-call `SAFETY` comment.
- `ask._sql_audit.latency_ms bigint` column. Wall time from audit
  insert to result render. NULL while the row is still in flight,
  populated by `ask._sql_audit_finish`. Idempotent
  `ADD COLUMN IF NOT EXISTS` in bootstrap for older installs.
- `tests/fixtures/sql_query_targets_missing_table.json` exercises
  the H2 subtxn path under `cargo pgrx test` without any network.

### Upgrade

`ALTER EXTENSION pg_ask UPDATE TO '0.5.2';` runs
`sql/pg_ask--0.5.1--0.5.2.sql`, which:

1. Adds `ask._sql_audit.latency_ms bigint` if missing.
2. Re-`REVOKE ALL` on the internal tables from `PUBLIC` and
   re-grants the SECURITY DEFINER write helpers.
3. Re-creates `ask._sql_audit_begin` / `_sql_audit_finish` /
   `_write_trace` / `_session_append` / `_memory_insert` /
   `_tool_upsert` with the current bodies and pinned `search_path`.

No public-surface (`ask.*` function signature) changes.

## [0.5.1] — 2026-05 — Schema rename, test framework, fixture provider

See `docs/ROADMAP.md` § v0.5.1 for the full list.

- Install schema renamed from `pg_ask` to `ask`. GUC keys still
  live under `pg_ask.*` because Postgres binds them to the
  extension name, not the install schema.
- `cargo pgrx test` framework wired up; macOS arm64 linker flag
  in `.cargo/config.toml`.
- Fixture provider (`provider = 'fixture'`) for hermetic CI.
- Audit-row write reordered to land *before* the readonly GUC
  flip (the deeper SET LOCAL leak was caught and fixed in
  v0.5.2).
- `ask.preview` JSON column read fix.

## [0.5.0] — 2026-04 — Streaming, observability, real SQL parser

- Server-side streaming via SRF: `ask.ask_stream(question)`.
- `sqlparser` 0.62 (PostgreSQL dialect) for `sql_guard`.
- `ask._sql_audit` table; `sql_query` and `sample_table` write a
  row post-execution.
- Background-worker prototype.
- Upgrade script `sql/pg_ask--0.4--0.5.sql`.

## [0.4.0] — Tooling expansion, RLS-awareness

- `http_fetch` tool (allow-list gated).
- `sample_table` lightweight tool.
- User-defined tools (`ask.register_tool`).
- RLS-aware schema dump via `has_table_privilege`.
- Column allow/deny lists (`pg_ask.sensitive_columns`).

## [0.3.0] — Memory, retrieval, token budget

- Token-budget schema rendering (`Full` / `Compact`).
- `describe_table` tool.
- pgvector-backed long-term memory (`ask.remember` / `recall` /
  `forget`).
- Hybrid search (cosine + BM25-style rerank).
- Embedding provider abstraction (OpenAI, Voyage, Gemini).

## [0.2.0] — Multi-provider, sessions, preview, audit

- `ask.preview(question)`.
- `ask._traces` audit table.
- OpenAI provider (covers every OpenAI-compatible host).
- Multi-turn sessions (`ask.create_session` / `chat` / `clear_session`).
- Gemini provider.

## [0.1.0] — Walking skeleton

- `ask.config` / `ask.get_config` / `ask.ask` / `ask.sql` /
  `ask.version`.
- Schema introspection from `pg_catalog`.
- `sql_query` tool via SPI (readonly mode).
- Anthropic provider.
- `sql_guard` (token-based; parser landed in v0.5).
- `SET LOCAL statement_timeout` + `transaction_read_only` around
  every tool call.
- Layered config (GUC → table fallback).
