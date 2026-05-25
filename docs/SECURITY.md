# pg_ask security model

`pg_ask` runs an LLM **inside the database backend**. The model can propose
SQL that gets executed in the caller's transaction and can read schema
metadata. This document spells out the threat model, the defences that ship
in v0.1, and the hardening checklist for production deployments.

## Threat model

| Threat                                              | Mitigation (release introduced)        |
|-----------------------------------------------------|----------------------------------------|
| Model writes data (INSERT/UPDATE/DELETE/DROP/…)     | `readonly = true` default; SQL guard rejects non-SELECT; per-call `SET LOCAL transaction_read_only = on` scoped inside an internal subtxn (v0.5.2). |
| Model reads sensitive data via SELECT               | Runs as caller (`SECURITY INVOKER`); RLS + GRANTs enforced. |
| Model bypasses guard via multi-statement payload    | `sqlparser` 0.62 walk classifies statements; lexer fallback rejects `;` in non-trailing position (v0.5). |
| Model calls `pg_sleep`, `pg_read_file`, `dblink`, … | Function denylist (AST walker + lexer). |
| Model uses `COPY ... TO/FROM`                       | Denylisted; also rejected by readonly mode. |
| Long-running model query holds locks                | `SET LOCAL statement_timeout` per tool call. |
| Provider HTTP hang holds backend                    | Shared `ureq::Agent` with connect + total timeouts; process-level pool (v0.5.2). |
| API keys exposed in `pg_settings` / dumps           | GUC marked `SUPERUSER_ONLY | NO_SHOW_ALL`; `ask.get_config('api_key')` returns `<redacted>`; table fallback `REVOKE ALL FROM PUBLIC` (v0.5.2). |
| Cross-tenant session / memory / tool theft          | `_sessions.owner = current_user` (v0.2); same predicate on `_memories` (v0.3) and `_tools` (v0.4). All writes through `SECURITY DEFINER` helpers that pin `session_user` (v0.5.2). |
| Prompt injection through data ("ignore previous…")  | Tool output framed as quoted block; system prompt is authoritative; readonly limits blast radius. |
| Backend panic crashes Postgres                      | All Rust `Result`s funnel to `pgrx::error!`; no `catch_unwind` over SPI; raw FFI confined to `src/infra/subtxn.rs`. |
| One tool failure poisons the agent loop             | `tools::sql_query` runs the model-emitted statement inside an internal subtxn (v0.5.2 H2). Errors return `is_error` to the model; the parent txn stays usable. |
| Stale `transaction_read_only` leaks into trace write | Per-call `SET LOCAL`s live inside the subtxn; releasing the subtxn pops the GUC stack frame (v0.5.2 critical fix). |
| Model reads sensitive column (password, SSN, token) | `pg_ask.sensitive_columns` redacts matching cells to `<redacted>` (v0.4). |
| Model fetches arbitrary external URLs (SSRF)        | `pg_ask.allow_http = false` default + URL-parser host-equality allow-list + IPv4/IPv6 private/loopback/link-local/CGNAT CIDR rejection before connect (v0.5.2 C5). |
| Malicious user-defined tool exfiltrates data        | `ask._tools` owner-scoped; only creator (or superuser) can delete; `{{key}}` placeholders compiled to `$N` parameters at registration (v0.5.2 C4). |
| Streamed tool output exceeds libpq reply buffer     | `telemetry::truncate_tool_output(…, TOOL_OUTPUT_PREVIEW_CHARS=2000)` shared by `ask.ask_stream` and the trace row writer (v0.5.2 review #11). |

## Defence in depth

### Layer 1 — SECURITY INVOKER + Postgres GRANTs

`ask.ask` and friends run as the calling role. Standard Postgres
permissions and Row-Level Security apply unchanged. The agent cannot read
a table the caller cannot read. This is the **primary** defence; everything
else is belt-and-braces.

Recommendation: grant `EXECUTE` on `ask.ask` only to roles that should
be able to ask. Revoke from `PUBLIC` by default.

> Naming note: SQL identifiers live in the `ask` schema (since v0.5.1).
> GUC keys still live under `pg_ask.*` (`SET pg_ask.readonly = on`),
> because Postgres ties GUC namespaces to the extension name, not its
> install schema.

### Layer 2 — Readonly transaction guard

When `pg_ask.readonly = true` (default), the `sql_query` tool wraps each
execution in `SET LOCAL transaction_read_only = on`. This blocks writes
even if the SQL guard misses something — `transaction_read_only` is
enforced by Postgres itself, not by string matching.

### Layer 3 — SQL guard

`src/sql_guard/` validates every string the model wants to execute.
Since v0.5 the guard is **parser-authoritative**: the `sqlparser`
0.62 walk (PostgreSQL dialect) classifies statement types, walks
the AST for banned functions, and handles CTEs / EXPLAIN bodies
properly. The lexer fallback is engaged only when `sqlparser`
returns a parse error (non-standard syntax); when both fire, the
strictest verdict wins. Rules as of v0.5.2:

1. Must be a single `SELECT`/`WITH`/`TABLE` statement. `EXPLAIN`
   is allowed only when the inner statement itself passes the same
   check (v0.5.2 H10 closed the `EXPLAIN INSERT …` bypass).
2. Multi-statement rejection. A `;` followed by any non-whitespace,
   non-comment token is a hard reject.
3. Function denylist (case-insensitive token match + AST walker):

   ```
   pg_sleep, pg_read_file, pg_read_binary_file, pg_ls_dir, pg_stat_file,
   lo_import, lo_export, dblink, dblink_connect, dblink_exec,
   pg_terminate_backend, pg_cancel_backend, pg_reload_conf,
   pg_promote, pg_logfile_rotate, current_setting (write form),
   set_config, pg_read_server_files
   ```

4. `COPY` rejected at the start of any statement (covers
   `COPY ... FROM PROGRAM`).

The guard is **one layer**, never the only layer. Defence below it
(readonly txn, subtxn isolation, SPI grants, RLS) catches the cases
it doesn't.

### Layer 4 — Resource limits + subtxn isolation

Per tool call (`sql_query`, `sample_table`, `planner::explain`):

- A fresh internal subtransaction wraps the model-emitted statement.
  The per-call `SET LOCAL statement_timeout` and
  `SET LOCAL transaction_read_only = on` are issued **inside** the
  subtxn so the GUC stack frame pops cleanly when the subtxn
  releases; the parent transaction never sees a stale flag.
- `SET LOCAL statement_timeout = pg_ask.tool_statement_timeout_ms`
  (default 10s).
- `LIMIT` not auto-injected (parser-fragile); instead, hard row cap
  at `pg_ask.tool_max_rows` (default 200) — extra rows dropped
  before they reach the model.
- Per-cell cap at 500 characters.
- Tool output exposed to the streaming surface and the persisted
  trace row is truncated to `TOOL_OUTPUT_PREVIEW_CHARS = 2000`
  characters with UTF-8 char-boundary safety. The model itself
  receives the full output via `history` so its next reasoning step
  has complete information.

Subtxn isolation (`src/infra/subtxn.rs`) is the single module
permitted to use raw `pgrx_pg_sys` FFI. It wraps Postgres's
`BeginInternalSubTransaction` / `Release…` /
`RollbackAndRelease…` inside a safe
`run_in_subtransaction(name, body)` helper. A failed model query
(typo, missing column, permission denied, statement_timeout,
divide-by-zero, …) no longer aborts the parent transaction and no
longer poisons the agent loop with
`current transaction is aborted, commands ignored`; the tool
returns `is_error` to the model, the loop keeps going,
`audit_finish` runs normally.

Known limitation: `transaction_read_only` is a transaction-wide
flag and even a subtxn cannot flip it back off mid-transaction
(`guc.c::check_transaction_read_only` rejects with
`ERRCODE_ACTIVE_SQL_TRANSACTION`). Audit row `latency_ms` therefore
stays NULL under readonly until v0.6 introduces a dblink / bgworker
/ autonomous-tx based writer.

Per agent run:

- `pg_ask.max_iterations` ceiling (default 16). Stops runaway tool loops.
- `pg_ask.http_total_timeout_ms` per provider call (default 120s).
- `check_for_interrupts!()` every iteration; `pg_cancel_backend` works.

### Layer 4b — Internal-table write isolation (v0.5.2)

All internal tables — `_config`, `_sessions`, `_messages`,
`_memories`, `_tools`, `_sql_audit`, `_traces` — are
`REVOKE ALL FROM PUBLIC`. Reads are exposed only where they're part
of the documented contract (`_traces`, `_tools`, `_sql_audit`,
`_memories` get `GRANT SELECT TO PUBLIC`; the per-row predicate
`WHERE caller / owner = current_user` is in the SQL itself, not
left to the Rust caller). Writes go strictly through
`SECURITY DEFINER` helpers:

- `ask._write_trace(payload jsonb)`
- `ask._sql_audit_insert(query, row_count, readonly, tool_name)`
- `ask._sql_audit_finish(id, latency_ms, error)`
- `ask._memory_insert(content, namespace, metadata, embedding)`
- `ask._memory_delete_owned(id)`
- `ask._tool_register(name, spec, body)`
- `ask._tool_unregister(name)`

Each helper pins `search_path = pg_catalog` (memory helpers also
include `ask` because they touch `ask._memories` directly) and
enforces `session_user` ownership inside the body. A single missed
`WHERE owner = current_user` predicate in Rust can no longer leak
rows across roles — the SQL itself refuses.

### Layer 5 — Secrets handling

API keys should live in a GUC, not the table. Order of preference:

1. **Session-local**, never persisted:

   ```sql
   SET LOCAL pg_ask.api_key = 'sk-ant-…';
   SELECT ask.ask('…');
   ```

2. **Role-scoped**, persisted in `pg_db_role_setting` (not in your dump):

   ```sql
   ALTER ROLE app_reader SET pg_ask.api_key = 'sk-ant-…';
   ```

3. **Table fallback** in `ask._config`. Convenient for dev; in
   production, `REVOKE ALL ON ask._config FROM PUBLIC` (already in
   bootstrap) and grant only to a setup role.

The string GUC is registered with `GucFlags::SUPERUSER_ONLY | NO_SHOW_ALL`
so `SHOW pg_ask.api_key` and `SELECT * FROM pg_settings` redact the value
for non-superusers.

### Layer 5b — Memory ownership (v0.3)

`ask._memories` carries an `owner name NOT NULL DEFAULT current_user`
column. Every read, write, and delete in the memory layer goes through
`WHERE owner = current_user` — the SQL itself, not just the Rust caller.
The `recall` tool the agent sees is therefore *automatically scoped to
the role that invoked `ask.ask`*; a session as role A cannot leak
context into a future session as role B.

`ask.forget(id)` returns `false` for both "unknown id" and "belongs
to someone else". Same NotFound collapse as sessions: an attacker
cannot probe id space for existence.

Embeddings themselves are sensitive — they can leak information about
the stored text. The `_memories.embedding` column inherits the same
GRANT story as the rest of the table (default `PUBLIC` SELECT, gated by
the `owner` predicate at the SQL level). Operators with stricter needs
should `REVOKE SELECT ON ask._memories FROM PUBLIC` and grant on a
role-by-role basis; the memory functions still work because they run
as `SECURITY INVOKER` and respect whatever you set.

The embedding-provider API key lives in its **own** GUC
(`pg_ask.embedding_api_key`), marked `SUPERUSER_ONLY | NO_SHOW_ALL`.
This lets operators mix providers (e.g. OpenAI embeddings + Anthropic
chat) without leaking either key between subsystems.

### Layer 5c — Column redaction (v0.4)

The `pg_ask.sensitive_columns` GUC accepts a comma-separated list of
patterns (`schema.table.column` or bare `column`). Before the `sql_query`
and `sample_table` tools return a result set to the model, every cell is
checked against the list; matches are replaced with `<redacted>`. The
column name stays visible in the header so the model knows the column
exists and can formulate queries that avoid it.

This is a **presentation-layer** filter — the SQL still executes and the
model still sees the row count, but it cannot learn the actual sensitive
values. Combine with RLS for defence in depth.

### Layer 5d — User-defined tools (v0.4)

Operators can register custom SQL snippets via
`ask.register_tool(name, spec, body)`. The body supports `{{key}}`
placeholder interpolation from the model's jsonb arguments at invocation
time. Each tool row carries an `owner = current_user` column;
`ask.unregister_tool(name)` deletes only the caller's own rows,
with the same NotFound==Unauthorized collapse used for sessions and
memories.

User-defined tools are loaded into the agent toolset dynamically on every
turn, so adding a tool does not require a server restart. The spec is a
JSON Schema object that the model sees alongside built-in tools.

Security note: the body is raw SQL executed as the calling role. There is
no sql_guard on user-defined tools because the operator explicitly opted
in to the snippet. Register tools only from audited, version-controlled
SQL migrations.

### Layer 6 — Audit log

`ask._traces` records every `ask()` / `sql()` / `preview()` /
`chat()` call: caller, db, kind, question, generated tool calls
(arguments + truncated output), iteration count, latency, provider,
model, error. This is the operator's eye into what the model has
been doing and what it has been told.

Lockdown is the *opposite* of the other internals — we deliberately
grant `SELECT` to `PUBLIC` so any logged-in role can audit its own
activity. The only insert path is `ask._write_trace(jsonb)`, a
`SECURITY DEFINER` helper that fixes `search_path` and uses
`gen_random_uuid()` for ids.

Writing happens after every entry-point call regardless of success:
errors land in the `error` column. The writer is fire-and-forget —
any failure here becomes a `WARNING` and never fails the user's
query (telemetry must not break the application). The
`pg_ask.trace_enabled` GUC (default `on`) lets a caller opt out
per-session with `SET LOCAL pg_ask.trace_enabled = off;`.

## Hardening checklist for production

- [ ] Set `pg_ask.api_key` via `ALTER ROLE` or `SET LOCAL`. Drop the row
      from `ask._config`.
- [ ] `REVOKE EXECUTE ON FUNCTION ask.ask(text), ask.sql(text),
      ask.preview(text), ask.chat(uuid, text), ask.ask_stream(text)
      FROM PUBLIC`.
- [ ] Grant `EXECUTE` only to the roles that should ask.
- [ ] Keep `pg_ask.readonly = true`.
- [ ] Set `pg_ask.tool_statement_timeout_ms` to your normal interactive
      ceiling (e.g. 5000).
- [ ] If you don't need network tools, keep `pg_ask.allow_http = false`.
      When you do enable it, narrow `pg_ask.http_allow_list` to specific
      hostnames (host equality, not prefix).
- [ ] Set `pg_ask.sensitive_columns` to redact known-sensitive columns
      (e.g. `users.password, orders.cvv`).
- [ ] Audit `ask._tools` periodically — user-defined tools execute raw
      SQL (with `{{key}}` placeholders compiled to `$N` parameters) and
      bypass the sql_guard.
- [ ] Monitor `ask._traces` and `ask._sql_audit` — unusual question
      rate, repeated `is_error` tool calls, large row counts, or
      rows stuck at `row_count = -1` (in-flight) are early signals of
      abuse or backend instability.
- [ ] Run the extension owner as a non-superuser role with the minimum
      grants it needs. The SECURITY DEFINER helpers run as the
      extension owner, so the privilege boundary is the owner role,
      not the calling role.
- [ ] After upgrading from 0.5.0/0.5.1, verify that the writer helpers
      and their grants are present:

      ```sql
      \df+ ask._write_trace ask._sql_audit_insert ask._sql_audit_finish \
           ask._memory_insert ask._memory_delete_owned \
           ask._tool_register ask._tool_unregister
      ```

## What we explicitly do not promise

- **No protection against a malicious extension owner.** If a superuser
  installs `pg_ask`, they could read your secrets anyway. We make life
  easier for honest operators, not harder for hostile ones.
- **No prompt-injection-proof guarantees.** A clever model + clever data
  may produce surprising tool calls. The readonly + RLS combination is
  what keeps that from becoming a data breach.
- **No protection against the model giving wrong answers.** Use
  `ask.preview()` (v0.2) when the answer matters and have a human
  glance at the SQL first.
