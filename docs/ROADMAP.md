# Roadmap

Milestone-level plan. Each release ships behind a `vX.Y` tag and an
upgrade script in `sql/pg_ask--A.B--X.Y.sql`. Items move only when their
checkbox is genuinely true on `main`.

## v0.1 — Walking skeleton + production-grade defaults

The first cut that a careful operator could put in front of a real DB.

- [x] pgrx 0.18, PG 14–18 build matrix
- [x] `ask.config(key, value)` / `ask.get_config(key)` (table-backed)
- [x] `ask.ask(question)` — single-shot agent loop
- [x] `ask.sql(question)` — generate-only
- [x] Schema introspection from `pg_catalog`
- [x] `sql_query` tool via SPI (readonly mode)
- [x] Anthropic provider
- [x] Cooperative cancellation (`check_for_interrupts!()`)
- [x] Repo layered into `api/ agent/ providers/ tools/ sql_guard/ schema/ infra/ telemetry/`
- [x] `sql_guard` module: SELECT-only, multi-statement reject, function denylist
- [x] `SET LOCAL statement_timeout` + `transaction_read_only` around every `sql_query` call
- [x] Shared `ureq::Agent` with connect + total timeouts in `infra::http`
- [x] Explicit volatility / parallel-safety annotations on every `#[pg_extern]`
- [x] `ask.version()` (IMMUTABLE)
- [x] GUC registry in `_PG_init`: provider, api_key (SUPERUSER_ONLY), model,
      base_url, max_tokens, max_iterations, readonly, http_connect_timeout_ms,
      http_total_timeout_ms, tool_statement_timeout_ms, tool_max_rows,
      trace_enabled
- [x] Layered config: GUC → table fallback; `RuntimeConfig` snapshot per call
- [x] Remove `std::panic::catch_unwind` from SPI paths (pgrx already handles it)
- [x] `From<SpiError>` for `AskError`; drop hand-rolled `e.to_string()` glue
- [x] README install note: macOS PG18 needs `brew install icu4c` before `cargo pgrx init`
- [x] `docs/ARCHITECTURE.md`, `docs/SECURITY.md`
- [x] Local `cargo pgrx run pg18` end-to-end with a recorded provider fixture
      (v0.5.1: `providers::fixture` + `tests/fixtures/*.json`. Drives the
      full agent loop — sql_guard, SPI, tool dispatch, telemetry —
      under `cargo pgrx test` without any network access.)
- [x] sql_guard tests passing under `#[pg_test]` (v0.5.1: the original
      pure-Rust unit tests stayed in place for fast iteration; new
      `pg_sql_guard_blocks_ddl_through_spi` and
      `pg_sql_guard_blocks_multi_statement_through_spi` cover the SPI
      call site against a real backend.)

## v0.2 — Multi-provider, sessions, preview, audit

In-progress milestone. Order of attack:
`preview()` → `_traces` → OpenAI provider → `chat()` + ownership.

- [x] **`ask.preview(question) → table(generated_sql text, est_rows bigint, tables text[], warnings text[])`**
      Produces SQL + `EXPLAIN (FORMAT JSON)` summary without executing the
      query. Strips any leading `EXPLAIN`/`ANALYZE` the model emits so we
      never accidentally execute; runs the EXPLAIN inside a readonly
      sub-transaction. Landed in 9c7d07c.
- [x] `ask._traces` audit table — single insert per `ask()` / `sql()` /
      `preview()` / `chat()`. Writer `ask._write_trace(jsonb)` is
      `SECURITY DEFINER` with fixed `search_path`. Columns: id, ts, caller,
      db, kind, question, iterations, tool_calls jsonb, final_text, provider,
      model, latency_ms, error. SELECT granted to PUBLIC; writes only via
      the helper. `token` columns deferred until provider metadata lands.
- [x] `pg_ask.trace_enabled` GUC honoured by writer; failures `WARNING`
      only — telemetry can never fail the user's call.
- [x] OpenAI provider — includes every OpenAI-compatible endpoint
      (Groq, Together, Mistral, Ollama, vLLM, LM Studio) via `base_url`
      override. Provider aliases recognised: `openai`, `openai-compat`,
      `groq`, `together`, `mistral`, `ollama`, `vllm`, `lmstudio`.
- [x] Multi-turn sessions backed by `ask._sessions` / `_messages`.
- [x] `ask.create_session(label)`, `ask.chat(session_id, message)`,
      `ask.clear_session(session_id)` — ownership-checked on every call.
      Sessions store assistant turns and tool results so the next turn
      replays the full conversation.
- [x] `ask._sessions.owner name NOT NULL DEFAULT current_user`. Existence
      and unauthorized access collapse to the same error so id-space
      probing leaks no information.
- [x] Gemini provider (generateContent v1beta) — systemInstruction +
      `contents[].parts[]` shape, `role: "user"|"model"`, function calls
      via `functionCall` / `functionResponse` parts. Round-trips function
      name in our canonical `tool_call_id` since Gemini matches by name,
      not id. Aliases: `gemini`, `google`, `google-genai`.
- [ ] Per-call config overrides as `jsonb` (deferred; current GUC layer
      already covers session-scoped overrides via `SET LOCAL`).

## v0.3 — Memory, retrieval, token budget

- [x] **Token-budget schema rendering**: when full schema exceeds budget,
      drop to tables-only listing + expose `describe_table` tool.
      Configurable via `pg_ask.schema_char_budget` (default 16 000 chars,
      ~4000 tokens). Implemented as a two-mode renderer (`Full` / `Compact`)
      so the prompt-tuning surface stays one file.
- [x] `describe_table` tool: fetches columns for a single table via
      `has_table_privilege`-filtered `pg_catalog` query. Surfaced only
      when the renderer falls back to compact mode so the function-call
      menu stays tight in the common case.
- [x] Table-level comments (`pg_description.objsubid = 0`) folded into
      both renderers.
- [x] pgvector-backed long-term memory: `ask.remember(content, namespace,
      metadata)`, `ask.recall(query, namespace, limit_n)`,
      `ask.forget(id)`. Owner-scoped (NotFound==Unauthorized collapse),
      namespaces, optional jsonb metadata. Runtime-detected: if pgvector is
      not installed `_memories` is simply skipped at bootstrap and the
      memory.* surface returns an operator-actionable error.
- [x] Hybrid search: `alpha * cosine + (1-alpha) * (1/(1+ts_rank_cd))`,
      blended in SQL. Default alpha = 0.7 via `pg_ask.memory_search_alpha`.
- [x] Embedding provider abstraction: `crate::embeddings::EmbeddingProvider`
      trait + OpenAI implementation (+ every OpenAI-compatible host via
      `pg_ask.embedding_base_url`). Width audit (`embedding_dimensions` vs
      actual response) surfaces misconfiguration loudly at `remember()`.
- [x] `recall` tool exposed to the agent when pgvector + embedding config
      are present (runtime-detected). Hard cap 25 hits.
- [ ] Per-row metadata filters (deferred; jsonb is stored, filtering predicate
      sugar lands with `ask.recall_where(query, filter jsonb)` in v0.4).
- [x] Voyage AI native + Google Gemini `batchEmbedContents` embedding
      providers. Aliases `voyage`, `gemini`/`google`.
- [x] `ask.list_namespaces()` and `ask.list_memories(namespace,
      limit_n, offset_n)` admin SRFs — owner-scoped catalog view of
      what is stored, no embedding round-trip.

## v0.4 — Tooling expansion, RLS-awareness

- [x] `http_fetch` tool, gated by `pg_ask.allow_http = false` GUC + URL
      allow-list GUC. Response body truncated to 8 kB; JSON pretty-printed
      when parseable.
- [x] `sample_table` lightweight tool — returns a few rows from any table
      the caller can `SELECT` from, with the same timeout / readonly /
      redaction layers as `sql_query`.
- [x] User-defined tools registered from SQL
      (`ask.register_tool(name, jsonb_spec, body)`). Body supports
      `{{key}}` placeholder interpolation from the model's jsonb arguments.
      Stored in `ask._tools`, owner-scoped (NotFound==Unauthorized collapse
      on delete). Dynamically loaded into the agent toolset every turn.
- [x] **RLS-aware schema dump**: `has_table_privilege(c.oid, 'SELECT')`
      filter added to global introspection queries so invisible tables
      never leak names into the prompt.
- [x] **Column allow/deny lists**: `pg_ask.sensitive_columns` GUC
      (comma-separated `schema.table.column` or bare `column` patterns).
      Matching cells are replaced with `<redacted>` in both `sql_query`
      and `sample_table` output.

## v0.5 — Streaming, observability, hardening

- [x] Server-side streaming via SRF (`SETOF text`) — `ask.ask_stream(question)`.
      Yields `[thinking]`, `[tool]`, and `[answer]` rows so the client can
      `FETCH 1` repeatedly instead of blocking for the full loop.
- [x] Real SQL parser for `sql_guard` — `sqlparser` 0.62 with PostgreSQL dialect
      now classifies statement types (SELECT, WITH, EXPLAIN, COPY, INSERT, …).
      The token-based lexer is kept as fallback for non-standard syntax and
      for the function-denylist check.
- [x] Audit hooks for SQL the agent runs — `ask._sql_audit` table.
      `sql_query` and `sample_table` tools write a row post-execution with
      the query text, rendered row count, readonly flag, and tool name.
- [ ] Real Claude / GPT / Gemini integration tests against recorded fixtures.
      Stub deferred; the `Provider` trait + `build()` factory are ready for a
      `fixture` alias that replays JSON files from `tests/fixtures/`.
- [x] Background-worker prototype — `pg_ask worker` registered via
      `BackgroundWorkerBuilder` when loaded through `shared_preload_libraries`.
      v0.5 stub heartbeat only; v0.6 will poll `ask._jobs` and run the
      agent loop asynchronously.
- [x] Upgrade-script policy documented; `sql/pg_ask--0.4--0.5.sql` ships.
      New `_sql_audit` table, `_tools.updated_at` backfill, grant re-apply.

## v0.5.1 — Schema rename, test framework, fixture provider, bug sweep

A cleanup release: tightens the public surface, brings the full pgrx
test framework online, and fixes three real bugs that the new tests
uncovered. No new features for end users beyond `provider = 'fixture'`,
which is a tooling primitive.

- [x] Install schema renamed from `pg_ask` to `ask`. Functions and
      tables are now addressed as `ask.ask(…)`, `ask._traces`, etc.
      GUC keys keep the `pg_ask.*` prefix because Postgres binds them
      to the extension name, not the install schema.
- [x] In-place upgrade via `sql/pg_ask--0.5--0.5.1.sql`
      (`ALTER SCHEMA pg_ask RENAME TO ask`). Refuses to run if a schema
      called `ask` already exists.
- [x] `cargo pgrx test` framework wired up. Required four cascading
      fixes: macOS-arm64 linker flag in `.cargo/config.toml`
      (`-Wl,-undefined,dynamic_lookup`), removal of
      `schema = 'ask'` from pg_ask.control (collided with
      `#[pg_schema] mod ask`-emitted `CREATE SCHEMA`), explicit
      `CREATE SCHEMA IF NOT EXISTS ask` at the head of bootstrap.sql,
      and two more `pg_ask.*` references in bootstrap that the rename
      pass had missed (`SCHEMA pg_ask` in a REVOKE and
      `'pg_ask'::regnamespace` in a DO block).
- [x] Background worker (`mod bgworker`) re-enabled now that the
      test framework can verify it loads (v0.5 stub heartbeat only).
- [x] Fixture provider (`providers::fixture`) for tests and CI.
      `provider = 'fixture'`, `model = 'fixture:<scenario>'`, JSON
      script on disk replays one turn per `complete()` call.
      `api_key` becomes optional iff `provider = 'fixture'`.

### Bugs fixed in passing

- [x] `tools::sql_query` and `tools::sample_table` were writing their
      `_sql_audit` row *after* flipping `transaction_read_only = on`,
      so under the default `pg_ask.readonly = on` every single tool
      call self-deadlocked on its own audit insert and poisoned the
      outer transaction. Audit is now written *before* the readonly
      GUC, with `row_count = -1` to mean "in flight".
- [x] `ask.preview` was unconditionally broken: EXPLAIN (FORMAT JSON)
      returns a `json` Datum, but planner::explain was asking SPI for
      `String`. The `.ok().flatten()` masked the type error as a
      generic "no payload" message. Fixed by reading the column as
      `pgrx::Json` directly.
- [x] Three `error!()` strings and one bootstrap REVOKE that the
      schema rename had missed.

## v0.5.6 — Event outbox (current)

Adds the `ask.emit()` event outbox for reverse notifications (ADR-0017),
on top of the v0.5.3–v0.5.5 line below. Test count: **90 green**.
See [`CHANGELOG.md`](../CHANGELOG.md) for the full release-by-release diff.

- **v0.5.6** — `ask._outbox` + `ask.emit()` event outbox (opt-in via
  `pg_ask.events_enabled`) for reverse notifications.
- **v0.5.5** — agent-loop efficiency: pinned `search_path`, bare-name
  prompting, `max_iterations` default raised **16 → 24**, graceful
  budget finalisation.
- **v0.5.4** — `ask.status()` capability handshake (`api_level = 1`) +
  `ask.status_api_level()` for external orchestrators (additive).
- **v0.5.3** — 10 hardening fixes (SQLSTATE-aware errors, token usage
  tracking, embedding retry/backoff, `_traces` RLS, dynamic embedding
  dimensions, user-tool caching, soft empty-response recovery).

## v0.5.2 — Hardening release

A pure-hardening release. No new public surface; 25 fixes across
security, correctness, and performance. End-to-end verified against a
live PG18 backend with ZAI GLM-5.1 over the Anthropic-compatible
endpoint, including a four-turn `ask.chat` anaphora canary. Test
count: 68 → 75 green.

Full diff: [`CHANGELOG.md`](../CHANGELOG.md). Highlights:

### Wave 1 — Critical (security)

- [x] **C1** `sql_guard` parser-authoritative; lexer fallback only on
      parse errors. Statement type, multi-statement rejection,
      denylist, CTE rules, EXPLAIN handling all run off the AST.
- [x] **C2** Internal tables (`_messages`, `_sessions`, `_memories`,
      `_tools`, `_sql_audit`, `_traces`, `_config`) `REVOKE ALL FROM
      PUBLIC`; writes go through `SECURITY DEFINER` helpers
      (`ask._write_trace`, `ask._sql_audit_insert/_finish`,
      `ask._memory_insert/_delete_owned`,
      `ask._tool_register/_unregister`) with pinned `search_path`
      and `session_user` ownership enforcement inside the body.
- [x] **C3** `ask.config` / `ask.get_config` redact `api_key` and
      `embedding_api_key` (case-insensitive match against a small
      redact set in `src/api/config.rs`).
- [x] **C4** User-defined tools (`ask.register_tool`) compile
      `{{key}}` placeholders into `$N` parameters at registration
      time; execution goes through `SPI_execute_with_args`. Model
      argument values can no longer escape into SQL syntax.
      Repeated placeholders share a parameter index; jsonb composites
      pass as `jsonb`, not as text.
- [x] **C5** `http_fetch` SSRF defence: `url` crate hostname parse,
      host-equality allow-list (not `starts_with`), IPv4 + IPv6
      private/loopback/link-local/CGNAT CIDR rejection before the
      request fires.
- [x] **C6** `pg_ask.api_key` / `pg_ask.embedding_api_key` marked
      `SUPERUSER_ONLY | NO_SHOW_ALL`; `pg_settings` and `SHOW ALL`
      redact for non-superusers.
- [x] **C7** `ask._traces` / `ask.session.list` / `ask.tools.list` /
      `ask.memory.*` SRFs enforce `WHERE caller / owner =
      current_user` at the SQL level. NotFound==Unauthorized for
      probing safety.
- [x] **C8** `_sql_audit` insert lands *before* the readonly GUC
      flip, in a separate `Spi::connect_mut` scope from the per-call
      GUC setup; an extreme-load `statement_timeout` on the INSERT
      itself cannot leak the flag back into the parent transaction.

### Wave 2 — High (security + correctness)

- [x] **H1** `sql_query` / `sample_table` accept a per-call
      `RuntimeConfig` snapshot; per-call limits cannot be raced by a
      concurrent `SET LOCAL`.
- [x] **H2** `src/infra/subtxn.rs` — the single module permitted to
      use raw `pgrx_pg_sys` FFI. Wraps Postgres
      `BeginInternalSubTransaction` / `Release…` /
      `RollbackAndRelease…` inside a safe
      `run_in_subtransaction(name, body)` helper.
      `sql_query::run_query_to_text` now runs the model-emitted
      statement inside that subtxn. A failed query (typo, missing
      column, permission denied, statement_timeout, divide-by-zero,
      …) used to abort the parent transaction and poison every
      subsequent SPI call in the agent loop with
      `current transaction is aborted, commands ignored`; the
      failure is now contained, the tool returns `is_error` to the
      model, the loop keeps going, and `audit_finish` runs normally.
      Shape mirrors plpython's
      `PLy_spi_subtransaction_{begin,commit,abort}` (memory context
      + resource owner snapshot/restore; catch via pgrx
      `PgTryBuilder`).
- [x] **H3** (partial) per-call `statement_timeout` now lives inside
      the H2 subtxn. Full audit-row latency stamp under readonly is
      deferred — `transaction_read_only` is a transaction-wide flag
      that even a subtxn cannot flip back off (confirmed by
      guc.c's `check_transaction_read_only` hook returning
      `ERRCODE_ACTIVE_SQL_TRANSACTION`). Documented limitation.
- [x] **H4** `tools::recall` validates `embedding_dimensions`
      against the actual response width at call time.
- [x] **H9** `infra::http::HttpClient` reuses a shared `ureq::Agent`
      per `(connect_timeout, read_timeout)` pair via a process-level
      cache. `Mutex` poison handled.
- [x] **H10** EXPLAIN goes through the parser; the inner statement
      must be `SELECT`/`WITH`/`TABLE` (no more `EXPLAIN INSERT …`
      slipping through the prefix check).
- [x] **H11** Token-budget renderer never falls back to the verbose
      walk when the compact rendering already fits.
- [x] **H12** `recall` tool obeys `pg_ask.tool_max_rows` and a hard
      ceiling of 25 results regardless of caller-requested `limit_n`.

### Critical — SET LOCAL readonly leak

- [x] Three call sites (`tools::sql_query::audit_begin`,
      `tools::sample_table::run_sample`, `planner::explain::run`)
      were issuing `SET LOCAL transaction_read_only = on` inside
      `Spi::connect_mut` thinking the LOCAL scope was the SPI block.
      It is not — `SET LOCAL` is scoped to the enclosing
      *transaction*, so the flag persisted and broke every
      subsequent INSERT (trace row, session turn, the next tool's
      audit insert, …). Fix: per-call SET LOCALs live inside a
      subtxn (sql_query + sample_table reuse the H2 subtxn;
      `planner::explain` opens a fresh one). When the subtxn
      releases, Postgres pops its GUC stack frame.

### Wave 3 — Performance

- [x] **P1** `RuntimeConfig` loaded once per top-level call; threaded
      via `agent::run_with_cfg` / `run_stream_with_cfg` /
      `memory::recall_with_cfg`.
- [x] **P2** Schema summary memoized per backend in a
      `thread_local Cell` with a 60 s TTL keyed by `char_budget`.
- [x] **P3** `pgvector_installed` latched in an `AtomicBool` for
      `true`; `false` not memoized so mid-session `CREATE EXTENSION
      vector` still works.
- [x] **P4** `HttpClient` process-level cache of `ureq::Agent`s.
- [x] **P5** `telemetry::write` builds a single payload jsonb and
      INSERTs via `_write_trace(jsonb)`.
- [x] **P6** Result-text builder uses `String::with_capacity` +
      `push_str` instead of repeated `format!`.
- [x] **P8** `providers::gemini::extract_function_name` returns
      `Result<&str, AskError>` instead of silently rewriting an
      unknown id to `"sql_query"`.

### Review item #11 — Stream output truncation

- [x] `ask.ask_stream()` was pushing the full tool result text into
      a single `SetOfIterator` element via `[tool] {} → {}`. A
      500-row sql_query produced a multi-hundred-KB single line.
      Truncation pattern hoisted into a shared
      `telemetry::truncate_tool_output` helper
      (cap = `TOOL_OUTPUT_PREVIEW_CHARS = 2000`); both surfaces
      (persisted trace row + live stream) go through it. Model
      still sees the full output via `history`. UTF-8 char-boundary
      safe.

### Upgrade

- [x] `sql/pg_ask--0.5.1--0.5.2.sql` ships. Adds
      `_sql_audit.latency_ms`, reapplies SECURITY DEFINER helpers,
      re-locks the internal-table grants. Idempotent.

## v0.6 — Background worker, async jobs, distribution

No public surface yet committed. Candidate work, ordered by
likelihood of landing in this release:

- [ ] Background worker drives `ask._jobs`: `ask.ask_async(question)`
      returns a job id immediately; the worker runs the agent loop
      under its own role and writes the result back. v0.5 ships only
      the heartbeat skeleton.
- [ ] `ask.recall_where(query, filter jsonb)` — jsonb metadata
      filter on the hybrid-search predicate.
- [ ] Server-side streaming directly to the client mid-iteration
      (Postgres protocol-level work; likely a sidecar).
- [ ] Provider streaming on the HTTP boundary so tokens flow into
      the trace as they arrive, not after the response completes.
- [ ] Per-call config overrides as jsonb — still deferred unless a
      concrete operator need surfaces (`SET LOCAL` already covers
      session-scoped overrides).
- [x] **Distribution:** GitHub Actions CI (PR gate: lint +
      90 unit tests, no PG runtime required); release workflow
      (v* tag → GitHub release + prebuilt Linux x86_64 pg18 tarball);
      Docker multi-arch image
      (`ghcr.io/sentirum/pg_ask:VERSION-pg18` + `latest-pg18`)
      via GHCR + docker/buildx QEMU arm64. docker-compose.yml for
      zero-config local try-out with PG_ASK_* env-var provider setup.
- [x] **APT repo:** `.deb` packages for Debian bookworm/trixie +
      Ubuntu jammy/noble (`amd64` + `arm64`), published in parallel to
      GitHub Pages (`sentirum.github.io/pg_ask`) and the Cloudsmith OSS
      repo (`sentirum/pg_ask`,
      [broadcasts.cloudsmith.com/sentirum/pg_ask](https://broadcasts.cloudsmith.com/sentirum/pg_ask)).
- [ ] **Distribution (remaining):** single-source everything on
      Cloudsmith — add **RPM** (RedHat/Fedora) and **APK** (Alpine)
      packages plus the Docker image, then retire the self-hosted
      gh-pages apt repo; `META.json` + PGXN submission; `pgxman`
      manifest; macOS arm64 prebuilt artefact.
- [ ] PG matrix CI expansion. v0.5.2 was production-tested on
      pg18 only; pg17 is the next likely target.

## Non-goals (for now)

- Streaming directly to the client mid-iteration (Postgres protocol-level
  work; might land via a sidecar in v0.6+).
- Local embedded LLM (llama.cpp) inside the backend — too heavy. Belongs
  in a sidecar.
- Voice / Telegram / multi-agent. Out of scope; this is a database extension.
