# pg_ask architecture

This document describes how `pg_ask` is structured internally, the layering
contract between modules, and the conventions every new piece of code must
respect. The goal is a small, **boring**, Postgres-native codebase that other
people can pick up in an afternoon.

## Layered overview

```
   ┌───────────────────────────────────────────────────────────┐
   │  SQL surface  (src/api/)                                  │
   │     ask / sql / preview / chat / config / version         │
   │     ▲ thin #[pg_extern] wrappers — no business logic      │
   └───────────────────────────────────────────────────────────┘
                       │
                       ▼
   ┌───────────────────────────────────────────────────────────┐
   │  Agent core  (src/agent/)                                 │
   │     loop · system prompt builder · tool dispatcher        │
   │     ▲ pure orchestration; no SQL strings, no HTTP         │
   └───────────────────────────────────────────────────────────┘
       │                │                │              │
       ▼                ▼                ▼              ▼
   ┌────────┐     ┌────────────┐    ┌─────────┐   ┌──────────┐
   │schema  │     │ providers/ │    │ tools/  │   │telemetry │
   │introspec│    │  Provider  │    │  Tool   │   │ _traces  │
   │ pg_cat │     │  trait     │    │  trait  │   │  writer  │
   └────────┘     └────────────┘    └─────────┘   └──────────┘
                                          │
                                          ▼
                                    ┌──────────────┐
                                    │ sql_guard    │
                                    │ readonly +   │
                                    │ denylist +   │
                                    │ statement_   │
                                    │ timeout      │
                                    └──────────────┘

   ┌───────────────────────────────────────────────────────────┐
   │  Infra  (src/infra/)                                      │
   │     config (GUC + table)  ·  http (shared ureq Agent)     │
   │     errors  ·  spi helpers                                │
   └───────────────────────────────────────────────────────────┘
```

### Layer rules — enforced by code review, not the compiler

1. **api/** never builds prompts, never speaks HTTP, never executes SQL.
   It maps `#[pg_extern]` signatures to one call into `agent::run_*` and
   converts `Result` into `error!`.
2. **agent/** depends on traits (`Provider`, `Tool`). It does not know which
   provider or which tool is registered. Adding OpenAI or `http_fetch` must
   not touch `agent/`.
3. **tools/sql_query** and **tools/sample_table** are the *only* modules
   allowed to call `Spi::*` for model-driven queries. All SQL strings
   passed to `sql_query` go through `sql_guard::validate` first.
   `sample_table` builds its own safe `SELECT` shape internally and does
   not accept raw SQL from the model.
4. **infra/http** is the *only* module allowed to construct or own a
   `ureq::Agent`. Providers receive a `&HttpClient` handle.
5. **infra/config** is the *only* module allowed to read a config key.
   Everything else takes typed values via function arguments or a
   `RuntimeConfig` snapshot built once per `ask()` invocation.

## Directory layout (target)

```
src/
  lib.rs                     # _PG_init, pg_module_magic!, module tree, GUC registry
  api/
    mod.rs                   # re-exports
    ask.rs                   # ask.ask  / ask.sql
    preview.rs               # ask.preview            (v0.2)
    chat.rs                  # ask.chat               (v0.2)
    config.rs                # ask.config / get_config / set_local_config
    tools.rs                 # ask.register_tool / unregister_tool / list_tools (v0.4)
    version.rs               # ask.version
  agent/
    mod.rs
    loop.rs                  # run_agent(...)
    prompt.rs                # build_system_prompt(...)
    dispatch.rs              # dispatch_tool(...)
    stream.rs                # ask_stream stateful iterator (v0.5)
  providers/
    mod.rs                   # Provider trait + ProviderResponse + factory
    anthropic.rs             # Anthropic Messages API
    openai.rs                # OpenAI Chat Completions + every compat host
    gemini.rs                # Google Gemini generateContent v1beta
    wire.rs                  # canonical Message / ToolCall types
  tools/
    mod.rs                   # Tool trait + default_toolset + load_user_tools
    sql_query.rs             # SPI executor (uses sql_guard; sensitive_columns redaction)
    describe_table.rs        # per-table pg_catalog lookup (compact-schema mode)
    recall.rs                # memory hybrid-search tool (compact menu when memory_ready)
    http_fetch.rs            # HTTP GET, allow-list gated (v0.4)
    sample_table.rs          # SELECT * FROM t LIMIT n, same defence layers as sql_query (v0.4)
    user_defined.rs          # operator-registered SQL snippets with {{key}} interpolation (v0.4)
  embeddings/
    mod.rs                   # EmbeddingProvider trait + factory
    openai.rs                # /v1/embeddings (also Together, vLLM, llama.cpp, ...)
    voyage.rs                # Voyage AI native (input_type-aware in future)
    gemini.rs                # Google :batchEmbedContents v1beta
  memory/
    mod.rs                   # remember / recall / forget; pgvector-aware
    store.rs                 # SPI primitives; hybrid_search SQL
  sql_guard/
    mod.rs                   # validate(sql, mode) -> Result<ValidatedSql>
    lexer.rs                 # tokenization (fallback + denylist)
    rules.rs                 # SELECT-only, deny-list, multi-statement check
    # v0.5: sqlparser (PostgreSQL dialect) handles statement-type
    # classification; lexer kept for non-standard syntax and function denylist.
  schema/
    mod.rs                   # summarize_within(budget) -> (text, mode)
    introspect.rs            # pg_catalog queries (full / per-table / table comments)
    render.rs                # full + compact text renderers
  session/
    mod.rs                   # create / load_history / append / clear
    store.rs                 # SPI primitives (parameterised; owner check)
  bgworker.rs                # BackgroundWorker prototype (v0.5)
  telemetry/
    mod.rs                   # TraceRecord + writer; no-op if _traces missing
  infra/
    mod.rs
    config.rs                # GUC + table layered lookup
    http.rs                  # shared ureq::Agent factory with timeouts
    errors.rs                # AskError + From impls
    spi.rs                   # tiny SPI helpers (single_text, exec_unit, ...)
sql/
  bootstrap.sql              # schemas, _config, _sessions, _messages, _traces, _tools, _sql_audit
  pg_ask--0.4--0.5.sql       # upgrade script (v0.5)
  pg_ask--0.5--0.5.1.sql     # upgrade: rename install schema pg_ask → ask
docs/
  ARCHITECTURE.md
  SECURITY.md
  ROADMAP.md
```

The v0.1 codebase will land on this layout in the refactor that accompanies
this document. Nothing here is aspirational — files that don't exist yet are
called out explicitly with the milestone in which they appear.

## Schema vs. GUC namespace

Since v0.5.1 the extension installs into the `ask` schema (functions,
tables, indexes). The GUC namespace is still `pg_ask.*` because Postgres
binds a GUC's first segment to the extension *name*, not its install
schema. So:

- `SELECT ask.ask('…')`, `SELECT * FROM ask._traces`, etc.
- `SET pg_ask.provider = 'anthropic'`, `ALTER ROLE x SET pg_ask.api_key = '…'`.

Upgrading from 0.5.0 runs `ALTER SCHEMA pg_ask RENAME TO ask`; existing
GUC values, RLS policies, and grants follow automatically because the
namespace OID is unchanged.

## Request lifecycle: `ask.ask(question)`

```
SQL caller
   │
   ▼
api::ask::ask(question)
   │
   ▼
agent::loop::run_agent(question, mode=Execute)
   │
   ├─ infra::config::snapshot()           ← reads GUC → table fallback
   │      => RuntimeConfig { provider, model, readonly, max_iter, timeouts… }
   │
   ├─ schema::summarize(budget)
   │      => SchemaSummary (compact text + token estimate)
   │
   ├─ agent::prompt::build_system_prompt(&summary, mode, readonly)
   │
   ├─ providers::factory(&runtime)        ← Box<dyn Provider>
   │
   ├─ tools::default_toolset(readonly)    ← Vec<Box<dyn Tool>>
   │
   ├─ loop iteration 0..max_iter:
   │      check_for_interrupts!()
   │      provider.complete(system, history, specs)
   │      match response:
   │         Final  → write trace, return
   │         Tools  → for each call:
   │                     sql_guard::validate(...)        ← if sql_query
   │                     tool.invoke(args)
   │                     append tool_result to history
   │
   └─ telemetry::write(TraceRecord { … })  ← single insert, best-effort
```

The loop holds **no Postgres pointers across an HTTP call**. Schema
introspection, tool execution, and trace writes each take a fresh `Spi::connect`
scope. HTTP happens outside any SPI scope. This is enforced by the
`run_agent` shape — there is no place to leak a tuple table into an HTTP
future.

## Configuration model

Three sources, checked in order. First hit wins.

1. **Session GUC** — `SHOW pg_ask.api_key` (`SET LOCAL pg_ask.api_key = '…'`).
   Most flexible, most secure: keys never touch disk, scoped to a transaction.
2. **Role / database GUC** — `ALTER ROLE alice SET pg_ask.api_key = '…'` or
   `ALTER DATABASE prod SET pg_ask.api_key = '…'`. Survives reconnects,
   stored in `pg_db_role_setting`, redacted from `pg_settings` for non-owners.
3. **Table fallback** — `ask._config(key, value)`. Convenient default for
   developers; should be revoked in production and replaced with GUCs.

A `RuntimeConfig` snapshot is produced once at the top of every `ask()`,
`sql()`, `preview()`, `chat()` call. Inside the agent loop nothing reads
config again — this guarantees a stable view for the entire run.

Known keys:

| Key                              | Type    | Default              | Source priority |
|----------------------------------|---------|----------------------|-----------------|
| `pg_ask.provider`                | text    | (required)           | GUC → table     |
| `pg_ask.api_key`                 | text    | (required)           | GUC → table     |
| `pg_ask.model`                   | text    | `claude-sonnet-4-5`  | GUC → table     |
| `pg_ask.base_url`                | text    | provider default     | GUC → table     |
| `pg_ask.max_tokens`              | int     | `4096`               | GUC → table     |
| `pg_ask.max_iterations`          | int     | `16`                 | GUC → table     |
| `pg_ask.readonly`                | bool    | `true`               | GUC → table     |
| `pg_ask.http_connect_timeout_ms` | int     | `10000`              | GUC → table     |
| `pg_ask.http_total_timeout_ms`   | int     | `120000`             | GUC → table     |
| `pg_ask.tool_statement_timeout_ms` | int   | `10000`              | GUC → table     |
| `pg_ask.tool_max_rows`           | int     | `200`                | GUC → table     |
| `pg_ask.trace_enabled`           | bool    | `true`               | GUC → table     |
| `pg_ask.schema_char_budget`      | int     | `16000`              | GUC → table     |
| `pg_ask.embedding_provider`      | text    | (required for memory)| GUC → table     |
| `pg_ask.embedding_api_key`       | text    | (required for memory)| GUC → table     |
| `pg_ask.embedding_model`         | text    | `text-embedding-3-small` | GUC → table |
| `pg_ask.embedding_base_url`      | text    | provider default     | GUC → table     |
| `pg_ask.embedding_dimensions`    | int     | `1536`               | GUC → table     |
| `pg_ask.memory_search_alpha`     | float   | `0.7`                | GUC → table     |
| `pg_ask.memory_enabled`          | bool    | `true`               | GUC → table     |
| `pg_ask.allow_http`              | bool    | `false`              | GUC → table     |
| `pg_ask.http_allow_list`         | text    | (empty = deny all)   | GUC → table     |
| `pg_ask.sensitive_columns`       | text    | (empty = none)       | GUC → table     |

API keys are marked with `GucFlags::SUPERUSER_ONLY | NO_SHOW_ALL` so
`SHOW ALL` and `pg_settings` don't leak them to ordinary roles.

## Trait contracts

### `Provider`

```rust
pub trait Provider {
    fn complete(
        &self,
        system: &str,
        history: &[Message],
        tools: &[ToolSpec],
    ) -> Result<ProviderResponse>;
}
```

Provider implementations:

- own no mutable state besides immutable config copies;
- never call SPI;
- use the shared `HttpClient` (timeouts set there, not per-request);
- return `ProviderResponse::Final` or `::ToolCalls`, never `Stream` (v0.1).

### `Tool`

```rust
pub trait Tool {
    fn spec(&self) -> ToolSpec;
    fn invoke(&self, args: &serde_json::Value) -> Result<ToolOutput>;
}
```

Tool implementations:

- declare a JSON-schema input via `ToolSpec`;
- return errors as `ToolOutput { is_error: true, text: "..." }`, *not* as
  `Err(...)`. This lets the agent feed the failure back to the model.
- never panic; pgrx already converts longjmp ↔ panic, so `catch_unwind`
  is forbidden inside tools (see `docs/SECURITY.md`).

## Error model

`infra::errors::AskError` is the single error type. Every fallible boundary
in the codebase returns `Result<T, AskError>`. Conversions live as
`impl From<…> for AskError`. The only place an `AskError` becomes a
PostgreSQL `ERROR` is at the `#[pg_extern]` boundary in `src/api/*`, via
`pgrx::error!`. This keeps panics out of the SPI machinery.

## Function volatility & parallelism

| Function              | Volatility | Parallel            |
|-----------------------|------------|---------------------|
| `ask.version()`       | IMMUTABLE  | parallel_safe       |
| `ask.get_config(key)` | STABLE     | parallel_restricted |
| `ask.config(k,v)`     | VOLATILE   | parallel_unsafe     |
| `ask.ask(q)`          | VOLATILE   | parallel_unsafe     |
| `ask.sql(q)`          | VOLATILE   | parallel_unsafe     |
| `ask.preview(q)`      | VOLATILE   | parallel_unsafe     |
| `ask.chat(s,m)`       | VOLATILE   | parallel_unsafe     |

Every function that performs HTTP or writes is `volatile + parallel_unsafe`.
The pgrx attribute is mandatory in the macro call so the generated SQL
matches.

## Test strategy

- **Unit tests (Rust)** for `sql_guard`, prompt builder, response parser.
  No PG needed.
- **`#[pg_test]` integration tests** for SPI helpers, schema introspection,
  config layering, tool dispatch, and full `ask()` against a recorded HTTP
  fixture (provider stub).
- **Recorded HTTP fixtures** live in `tests/fixtures/` and are replayed by
  a `Provider` impl that wraps a JSON file. No live network in CI.

See `docs/ROADMAP.md` for milestone-by-milestone feature plan and
`docs/SECURITY.md` for the threat model and hardening checklist.
