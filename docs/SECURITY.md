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

## Defence in depth

### Layer 1 — SECURITY INVOKER + Postgres GRANTs

`pg_ask.ask` and friends run as the calling role. Standard Postgres
permissions and Row-Level Security apply unchanged. The agent cannot read
a table the caller cannot read. This is the **primary** defence; everything
else is belt-and-braces.

Recommendation: grant `EXECUTE` on `pg_ask.ask` only to roles that should
be able to ask. Revoke from `PUBLIC` by default.

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
   SELECT pg_ask.ask('…');
   ```

2. **Role-scoped**, persisted in `pg_db_role_setting` (not in your dump):

   ```sql
   ALTER ROLE app_reader SET pg_ask.api_key = 'sk-ant-…';
   ```

3. **Table fallback** in `pg_ask._config`. Convenient for dev; in
   production, `REVOKE ALL ON pg_ask._config FROM PUBLIC` (already in
   bootstrap) and grant only to a setup role.

The string GUC is registered with `GucFlags::SUPERUSER_ONLY | NO_SHOW_ALL`
so `SHOW pg_ask.api_key` and `SELECT * FROM pg_settings` redact the value
for non-superusers.

### Layer 6 — Audit log (v0.2)

`pg_ask._traces` records every `ask()` call: caller, db, question,
generated tool calls, executed SQL, row counts, latency, provider, tokens,
errors. This is the operator's eye into what the model has been doing and
what it has been told.

`REVOKE ALL ON pg_ask._traces FROM PUBLIC`; grant `SELECT` to your
auditing role. Writes happen via a `SECURITY DEFINER` helper so callers
without `INSERT` rights can still produce trace rows.

## Hardening checklist for production

- [ ] Set `pg_ask.api_key` via `ALTER ROLE` or `SET LOCAL`. Drop the row
      from `pg_ask._config`.
- [ ] `REVOKE EXECUTE ON FUNCTION pg_ask.ask(text), pg_ask.sql(text),
      pg_ask.preview(text), pg_ask.chat(uuid, text) FROM PUBLIC`.
- [ ] Grant `EXECUTE` only to the roles that should ask.
- [ ] Keep `pg_ask.readonly = true`.
- [ ] Set `pg_ask.tool_statement_timeout_ms` to your normal interactive
      ceiling (e.g. 5000).
- [ ] If you don't need network tools (v0.4), keep `pg_ask.allow_http =
      false`.
- [ ] Monitor `pg_ask._traces` (v0.2) — unusual question rate, repeated
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
  `pg_ask.preview()` (v0.2) when the answer matters and have a human
  glance at the SQL first.
