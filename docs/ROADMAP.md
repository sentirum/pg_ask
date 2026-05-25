# Roadmap

Milestone-level plan. Each release ships behind a `vX.Y` tag and an
upgrade script in `sql/pg_ask--A.B--X.Y.sql`. Items move only when their
checkbox is genuinely true on `main`.

## v0.1 — Walking skeleton + production-grade defaults

The first cut that a careful operator could put in front of a real DB.

- [x] pgrx 0.18, PG 14–18 build matrix
- [x] `pg_ask.config(key, value)` / `pg_ask.get_config(key)` (table-backed)
- [x] `pg_ask.ask(question)` — single-shot agent loop
- [x] `pg_ask.sql(question)` — generate-only
- [x] Schema introspection from `pg_catalog`
- [x] `sql_query` tool via SPI (readonly mode)
- [x] Anthropic provider
- [x] Cooperative cancellation (`check_for_interrupts!()`)
- [x] Repo layered into `api/ agent/ providers/ tools/ sql_guard/ schema/ infra/ telemetry/`
- [x] `sql_guard` module: SELECT-only, multi-statement reject, function denylist
- [x] `SET LOCAL statement_timeout` + `transaction_read_only` around every `sql_query` call
- [x] Shared `ureq::Agent` with connect + total timeouts in `infra::http`
- [x] Explicit volatility / parallel-safety annotations on every `#[pg_extern]`
- [x] `pg_ask.version()` (IMMUTABLE)
- [x] GUC registry in `_PG_init`: provider, api_key (SUPERUSER_ONLY), model,
      base_url, max_tokens, max_iterations, readonly, http_connect_timeout_ms,
      http_total_timeout_ms, tool_statement_timeout_ms, tool_max_rows,
      trace_enabled
- [x] Layered config: GUC → table fallback; `RuntimeConfig` snapshot per call
- [x] Remove `std::panic::catch_unwind` from SPI paths (pgrx already handles it)
- [x] `From<SpiError>` for `AskError`; drop hand-rolled `e.to_string()` glue
- [x] README install note: macOS PG18 needs `brew install icu4c` before `cargo pgrx init`
- [x] `docs/ARCHITECTURE.md`, `docs/SECURITY.md`
- [ ] Local `cargo pgrx run pg18` end-to-end with a recorded provider fixture
- [ ] sql_guard unit tests passing under `#[pg_test]` (today they pass under `cargo test`)

## v0.2 — Multi-provider, sessions, preview, audit

In-progress milestone. Order of attack:
`preview()` → `_traces` → OpenAI provider → `chat()` + ownership.

- [ ] **`pg_ask.preview(question) → table(generated_sql text, est_rows bigint, tables text[], warnings text[])`**
      Produces SQL + `EXPLAIN (FORMAT JSON)` summary without executing the
      query. Strips any leading `EXPLAIN`/`ANALYZE` the model emits so we
      never accidentally execute; runs the EXPLAIN inside a readonly
      sub-transaction. Postgres-native differentiator.
- [ ] `pg_ask._traces` audit table — single insert per `ask()` / `chat()` /
      `preview()`. Writer is `SECURITY DEFINER`. Columns: id, ts, caller, db,
      question, iterations, tool_calls jsonb, final_text, provider, model,
      prompt_tokens, completion_tokens, latency_ms, error.
- [ ] `pg_ask.trace_enabled` GUC honoured by writer (already registered).
- [ ] OpenAI provider (works with OpenAI-compatible endpoints: Groq,
      Together, Ollama, vLLM via `base_url`).
- [ ] Gemini provider.
- [ ] Multi-turn sessions backed by `pg_ask._sessions` / `_messages`.
- [ ] `pg_ask.chat(session_id, message)` — owner check, per-call config overrides as `jsonb`.
- [ ] `pg_ask._sessions.owner name NOT NULL DEFAULT current_user` + check on every chat().

## v0.3 — Memory, retrieval, token budget

- [ ] pgvector-backed long-term memory (`pg_ask.remember`, `pg_ask.recall`)
- [ ] Hybrid search: cosine + `tsvector` BM25-ish ranking
- [ ] Per-row metadata filters
- [ ] Embedding provider abstraction (OpenAI, Voyage, local llama.cpp)
- [ ] **Token-budget schema rendering**: when full schema exceeds budget,
      drop to tables-only listing + offer `describe_table` tool. Configurable
      via `pg_ask.schema_token_budget` (default 4000 tokens).
- [ ] Table-level comments (`pg_description.objsubid = 0`) in schema render.

## v0.4 — Tooling expansion, RLS-awareness

- [ ] `http_fetch` tool, gated by `pg_ask.allow_http = false` GUC + URL
      allow-list GUC.
- [ ] `describe_table` / `sample_table` lightweight tools (cheaper than
      full schema in prompt).
- [ ] User-defined tools registered from SQL
      (`pg_ask.register_tool(name, jsonb_spec, plpgsql_body)`).
- [ ] **RLS-aware schema dump**: filter out tables / columns the caller
      cannot `SELECT`. Run introspection as `SECURITY INVOKER`.
- [ ] **Column allow/deny lists**: `pg_ask.sensitive_columns` GUC; matching
      column values are returned to the model as `<redacted>`.

## v0.5 — Streaming, observability, hardening

- [ ] Server-side streaming via SRF (`SETOF text`) where the provider
      supports it.
- [ ] Real SQL parser for `sql_guard` (replace token matcher).
- [ ] Audit hooks for SQL the agent runs (post-execution).
- [ ] Real Claude / GPT / Gemini integration tests against recorded fixtures.
- [ ] Background-worker prototype for long-running questions (decouples
      LLM latency from the calling backend).
- [ ] Upgrade-script policy documented; `pg_ask--0.4--0.5.sql` ships.

## Non-goals (for now)

- Streaming directly to the client mid-iteration (Postgres protocol-level
  work; might land via a sidecar in v0.6+).
- Local embedded LLM (llama.cpp) inside the backend — too heavy. Belongs
  in a sidecar.
- Voice / Telegram / multi-agent. Out of scope; this is a database extension.
