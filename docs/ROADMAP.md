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

- [x] **`pg_ask.preview(question) → table(generated_sql text, est_rows bigint, tables text[], warnings text[])`**
      Produces SQL + `EXPLAIN (FORMAT JSON)` summary without executing the
      query. Strips any leading `EXPLAIN`/`ANALYZE` the model emits so we
      never accidentally execute; runs the EXPLAIN inside a readonly
      sub-transaction. Landed in 9c7d07c.
- [x] `pg_ask._traces` audit table — single insert per `ask()` / `sql()` /
      `preview()` / `chat()`. Writer `pg_ask._write_trace(jsonb)` is
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
- [x] Multi-turn sessions backed by `pg_ask._sessions` / `_messages`.
- [x] `pg_ask.create_session(label)`, `pg_ask.chat(session_id, message)`,
      `pg_ask.clear_session(session_id)` — ownership-checked on every call.
      Sessions store assistant turns and tool results so the next turn
      replays the full conversation.
- [x] `pg_ask._sessions.owner name NOT NULL DEFAULT current_user`. Existence
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
- [x] pgvector-backed long-term memory: `pg_ask.remember(content, namespace,
      metadata)`, `pg_ask.recall(query, namespace, limit_n)`,
      `pg_ask.forget(id)`. Owner-scoped (NotFound==Unauthorized collapse),
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
      sugar lands with `pg_ask.recall_where(query, filter jsonb)` in v0.4).
- [x] Voyage AI native + Google Gemini `batchEmbedContents` embedding
      providers. Aliases `voyage`, `gemini`/`google`.
- [x] `pg_ask.list_namespaces()` and `pg_ask.list_memories(namespace,
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
      (`pg_ask.register_tool(name, jsonb_spec, body)`). Body supports
      `{{key}}` placeholder interpolation from the model's jsonb arguments.
      Stored in `pg_ask._tools`, owner-scoped (NotFound==Unauthorized collapse
      on delete). Dynamically loaded into the agent toolset every turn.
- [x] **RLS-aware schema dump**: `has_table_privilege(c.oid, 'SELECT')`
      filter added to global introspection queries so invisible tables
      never leak names into the prompt.
- [x] **Column allow/deny lists**: `pg_ask.sensitive_columns` GUC
      (comma-separated `schema.table.column` or bare `column` patterns).
      Matching cells are replaced with `<redacted>` in both `sql_query`
      and `sample_table` output.

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
