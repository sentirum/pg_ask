# pg_ask security model

`pg_ask` runs an LLM **inside the database backend**. The model can propose
SQL that gets executed in the caller's transaction and can read schema
metadata. This document spells out the threat model, the defences that ship
in v0.1, and the hardening checklist for production deployments.

## Threat model

| Threat                                              | Mitigation in v0.1                    |
|-----------------------------------------------------|----------------------------------------|
| Model writes data (INSERT/UPDATE/DELETE/DROP/…)     | `readonly = true` default; SQL guard rejects non-SELECT |
| Model reads sensitive data via SELECT               | Runs as caller (`SECURITY INVOKER`); RLS + GRANTs enforced |
| Model bypasses guard via multi-statement payload    | Guard rejects `;` in non-trailing position |
| Model calls `pg_sleep`, `pg_read_file`, `dblink`, … | Function denylist in guard            |
| Model uses `COPY ... TO/FROM`                       | Denylisted; also rejected by readonly mode |
| Long-running model query holds locks                | `SET LOCAL statement_timeout` per tool call |
| Provider HTTP hang holds backend                    | Shared `ureq::Agent` with connect + total timeouts |
| API keys exposed in `pg_settings` / dumps           | GUC marked `SUPERUSER_ONLY | NO_SHOW_ALL`; table fallback `REVOKE ALL FROM PUBLIC` |
| Cross-tenant session theft (v0.2)                   | `_sessions.owner = current_user` check on every chat() call |
| Prompt injection through data ("ignore previous…")  | Tool output framed as quoted block; system prompt is authoritative; readonly limits blast radius |
| Backend panic crashes Postgres                      | All Rust `Result`s funnel to `pgrx::error!`; no `catch_unwind` over SPI |
| Model reads sensitive column (password, SSN, token) | `pg_ask.sensitive_columns` redacts matching cells to `<redacted>` (v0.4) |
| Model calls arbitrary external URLs                 | `pg_ask.allow_http = false` default + URL prefix allow-list (v0.4) |
| Malicious user-defined tool exfiltrates data        | `ask._tools` owner-scoped; only the creator (or superuser) can delete (v0.4) |

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
Rules in v0.1:

1. Must start with `SELECT`, `WITH`, `TABLE`, or `EXPLAIN` (case-insensitive,
   ignoring leading whitespace and comments).
2. May contain at most one statement. A `;` followed by any non-whitespace,
   non-comment token is a hard reject.
3. Must not contain (case-insensitive token match) any banned function:

   ```
   pg_sleep, pg_read_file, pg_read_binary_file, pg_ls_dir, pg_stat_file,
   lo_import, lo_export, dblink, dblink_connect, dblink_exec,
   pg_terminate_backend, pg_cancel_backend, pg_reload_conf,
   pg_promote, pg_logfile_rotate, current_setting (write form),
   set_config, pg_read_server_files
   ```

4. Must not contain `COPY ` at the start of any statement (covers
   `COPY ... FROM PROGRAM`).

The guard is intentionally token-based, not parser-based. A real parser is
v0.5 work; until then, the guard is **one layer**, never the only layer.

### Layer 4 — Resource limits

Per tool call:

- `SET LOCAL statement_timeout = pg_ask.tool_statement_timeout_ms` (default 10s)
- `LIMIT` not auto-injected (parser-fragile); instead, hard row cap at
  `pg_ask.tool_max_rows` (default 200) — extra rows dropped before they
  reach the model.
- Per cell cap at 500 characters.

Per agent run:

- `pg_ask.max_iterations` ceiling (default 16). Stops runaway tool loops.
- `pg_ask.http_total_timeout_ms` per provider call (default 120s).
- `check_for_interrupts!()` every iteration; `pg_cancel_backend` works.

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
      ask.preview(text), ask.chat(uuid, text) FROM PUBLIC`.
- [ ] Grant `EXECUTE` only to the roles that should ask.
- [ ] Keep `pg_ask.readonly = true`.
- [ ] Set `pg_ask.tool_statement_timeout_ms` to your normal interactive
      ceiling (e.g. 5000).
- [ ] If you don't need network tools (v0.4), keep `pg_ask.allow_http =
      false`.
- [ ] Set `pg_ask.sensitive_columns` to redact known-sensitive columns
      (e.g. `users.password, orders.cvv`).
- [ ] Audit `ask._tools` periodically — user-defined tools execute raw
      SQL and bypass the sql_guard.
- [ ] Monitor `ask._traces` (v0.2) — unusual question rate, repeated
      tool errors, or large row counts are early signals of abuse.
- [ ] Run the extension owner as a non-superuser role with the minimum
      grants it needs.

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
