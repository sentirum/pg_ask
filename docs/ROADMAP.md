# Roadmap

## v0.1 — Walking skeleton (current)

- [x] pgrx 0.18, PG 14–18 build matrix
- [x] `pg_ask.config(key, value)` / `pg_ask.get_config(key)`
- [x] `pg_ask.ask(question)` — single-shot agent loop
- [x] `pg_ask.sql(question)` — generate-only
- [x] Schema introspection from `pg_catalog`
- [x] `sql_query` tool via SPI (readonly mode)
- [x] Anthropic provider
- [x] Cooperative cancellation (`check_for_interrupts!()`)
- [ ] Local `cargo pgrx run` end-to-end on PG18

## v0.2 — Multi-provider + sessions

- [ ] OpenAI provider (incl. OpenAI-compatible base_url for Groq, Together, Ollama, vLLM)
- [ ] Gemini provider
- [ ] Multi-turn sessions backed by `pg_ask._sessions` / `_messages`
- [ ] `pg_ask.chat(session_id, message)` returning conversational answers
- [ ] Per-call config overrides as `jsonb`

## v0.3 — Memory & retrieval

- [ ] pgvector-backed long-term memory (`pg_ask.remember`, `pg_ask.recall`)
- [ ] Hybrid search: cosine + `tsvector` BM25-ish ranking
- [ ] Per-row metadata filters
- [ ] Embedding provider abstraction (OpenAI, Voyage, local llama.cpp)

## v0.4 — Tooling expansion

- [ ] `http_fetch` tool (with allow-list GUC)
- [ ] `describe_table` / `sample_table` lightweight tools (cheaper than full schema in prompt)
- [ ] User-defined tools registered from SQL (`pg_ask.register_tool(name, jsonb_spec, plpgsql_body)`)

## v0.5 — Streaming, observability, hardening

- [ ] Server-side streaming via SRF (`SETOF text`) where the provider supports it
- [ ] `pg_ask._traces` table — per-iteration tool calls, latency, token counts
- [ ] GUCs: `pg_ask.statement_timeout_ms`, `pg_ask.max_rows`, `pg_ask.allow_http`
- [ ] Audit hooks for SQL the agent runs
- [ ] Real Claude / GPT / Gemini integration tests against recorded fixtures
