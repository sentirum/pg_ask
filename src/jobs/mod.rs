//! Async job queue domain layer (v0.5.9 / ADR-0018).
//!
//! This is the single home of the "claim → run → complete" business logic
//! for `ask._jobs`. Both drivers reuse it unchanged:
//!
//! * `ask.run_pending_jobs()` (a `#[pg_extern]`) — synchronous manual /
//!   pg_cron-style drain, runs inside the caller's transaction.
//! * the background worker (`crate::bgworker`) — wraps each job in its own
//!   `BackgroundWorker::transaction(...)` so a long LLM round-trip never
//!   holds one transaction open across the whole batch.
//!
//! Keeping the logic here (not in either driver) is the clean-architecture
//! point: the SQL state machine (`ask._job_*` helpers) is the data layer,
//! this module is the use-case layer, and the two drivers are thin
//! delivery mechanisms. Neither driver duplicates a claim/complete rule.
//!
//! ## Transaction shape
//!
//! Each job goes through three SECURITY DEFINER calls:
//!   1. `_job_claim()`   — atomically pending → running (FOR UPDATE SKIP LOCKED)
//!   2. agent loop       — the LLM round-trip + tool SPI (no _jobs lock held)
//!   3. `_job_complete()` / `_job_fail()` — running → done / failed / pending
//!
//! These three are deliberately split across SEPARATE transactions by the
//! background-worker driver, which is what makes the durability story real:
//!
//! * **claim** commits on its own, so the `running` transition (with
//!   `started_at`) is durable and visible to other backends *before* the
//!   slow agent loop begins;
//! * the **agent loop** runs holding no `_jobs` lock;
//! * **complete/fail** commits on its own.
//!
//! If the worker crashes during the agent loop, the job stays `running` with
//! a stale `started_at`, and `_job_recover_orphans` (which only ever sees a
//! *committed* `running` row) returns it to `pending`. This is why claim must
//! commit separately — in a single combined transaction a crash would roll
//! the claim back too, the `running` state would never be visible, and
//! orphan recovery would be unreachable dead code.
//!
//! The synchronous `ask.run_pending_jobs()` driver cannot commit between
//! steps (a plain SQL function runs inside the caller's transaction and may
//! not `COMMIT`), so it runs the whole bounded batch in one transaction. The
//! trade-off there (a re-queued retry must not be re-claimed in the same
//! pass) is handled by [`drain`] tracking the ids it has already attempted.
//! For per-job durability, prefer the background worker; `run_pending_jobs`
//! is the pg_cron / no-preload fallback.
//!
//! This module stays transaction-agnostic: it only issues SPI calls. The
//! background-worker driver wraps [`claim_one`] and [`execute_claimed`] in
//! their own `BackgroundWorker::transaction(...)` blocks; the SQL function
//! driver calls [`drain`] inside the one transaction it already has.

use crate::agent::{self, AgentMode};
use crate::infra::config::{RuntimeConfig, JOBS_MAX_ATTEMPTS, JOBS_ORPHAN_TIMEOUT_MS};
use crate::infra::errors::Result;
use crate::telemetry::{self, TraceKind, TraceRecord};
use pgrx::prelude::*;
use pgrx::Uuid;

/// A job claimed off the queue, ready to run.
pub struct ClaimedJob {
    pub id: Uuid,
    pub kind: AgentMode,
    pub question: String,
    /// The role that enqueued the job. The worker runs the agent loop under
    /// this role (`SET LOCAL ROLE`) so async work has exactly the same
    /// privileges the caller would have had synchronously — not the worker's
    /// superuser rights.
    pub owner: String,
}

/// Atomically claim the oldest pending job for the current database, if any.
///
/// Thin wrapper over `ask._job_claim()` (which does the FOR UPDATE SKIP
/// LOCKED + pending → running transition). Returns `None` when the queue is
/// empty. Maps the textual `kind` column to [`AgentMode`]; an unknown value
/// defaults to `Execute` (the `ask` mode) rather than failing the drain.
pub fn claim_one() -> Result<Option<ClaimedJob>> {
    Spi::connect(|client| {
        // _job_claim() also returns `attempts`; we deliberately select only
        // the three columns the use-case layer needs. The retry policy lives
        // entirely in _job_fail (which reads attempts in SQL), so the Rust
        // layer never needs the count. Selecting a subset is forward-
        // compatible: adding columns to _job_claim won't break this.
        // _job_claim() returns (id, kind, question, attempts, owner). We read
        // id/kind/question/owner; attempts stays in the SQL retry policy.
        let tup = client
            .select(
                "SELECT id, kind, question, owner::text FROM ask._job_claim()",
                Some(1),
                &[],
            )?
            .first();
        if tup.is_empty() {
            return Ok(None);
        }
        let id: Option<Uuid> = tup.get(1)?;
        let kind_txt: Option<String> = tup.get(2)?;
        let question: Option<String> = tup.get(3)?;
        let owner: Option<String> = tup.get(4)?;
        let id = id.ok_or_else(|| crate::infra::errors::AskError::Sql("claim: null id".into()))?;
        let question = question
            .ok_or_else(|| crate::infra::errors::AskError::Sql("claim: null question".into()))?;
        let owner =
            owner.ok_or_else(|| crate::infra::errors::AskError::Sql("claim: null owner".into()))?;
        let kind = match kind_txt.as_deref() {
            Some("sql") => AgentMode::GenerateOnly,
            _ => AgentMode::Execute,
        };
        Ok(Some(ClaimedJob {
            id,
            kind,
            question,
            owner,
        }))
    })
}

/// Run a claimed job to completion: execute the agent loop, then record the
/// outcome via `_job_complete` (success) or `_job_fail` (which retries or
/// marks terminal per `jobs_max_attempts`).
///
/// Never returns `Err` for an agent-level failure — a failed agent run is a
/// normal job outcome routed to `_job_fail`, not a driver error. `Err` is
/// reserved for an SPI failure while recording the outcome, which the driver
/// surfaces. A trace row is written for the run just like a synchronous
/// `ask.ask`, so async jobs are as observable as sync calls.
pub fn execute_claimed(cfg: &RuntimeConfig, job: &ClaimedJob) -> Result<()> {
    let kind = match job.kind {
        AgentMode::Execute => TraceKind::Ask,
        AgentMode::GenerateOnly => TraceKind::Sql,
    };

    // Privilege isolation: run the agent loop under the role that ENQUEUED
    // the job, not the worker's superuser identity. `SET LOCAL ROLE` switches
    // current_user for the rest of this transaction, so every tool SPI query
    // (sql_query, sample_table, …) is checked against the owner's privileges
    // exactly as a synchronous ask.ask() by that role would be. We restore
    // the worker role before recording the outcome so the SECURITY DEFINER
    // completion helpers run as the worker. RESET runs on every path,
    // including the agent-error path, via the explicit reset below.
    set_local_role(&job.owner)?;

    // Run the agent loop with a trace row, mirroring the synchronous path.
    let mut rec = TraceRecord::start(kind, cfg, &job.question);
    let run = agent::run_with_cfg(cfg, &job.question, Vec::new(), job.kind);

    // Back to the worker role for the trusted state-machine writes.
    reset_role();

    match run {
        Ok(outcome) => {
            rec.iterations = outcome.iterations;
            rec.tool_calls = outcome.tool_calls.clone();
            rec.final_text = Some(outcome.text.clone());
            if outcome.prompt_tokens > 0 || outcome.completion_tokens > 0 {
                rec.prompt_tokens = Some(outcome.prompt_tokens);
                rec.completion_tokens = Some(outcome.completion_tokens);
            }
            telemetry::write(&rec);
            complete(
                job.id,
                &outcome.text,
                outcome.prompt_tokens,
                outcome.completion_tokens,
            )?;
            Ok(())
        }
        Err(e) => {
            rec.error = Some(e.to_string());
            telemetry::write(&rec);
            // Route to the retry/terminal state machine. The returned status
            // ("pending" = will retry, "failed" = terminal) is informational.
            let _status = fail(job.id, &e.to_string())?;
            Ok(())
        }
    }
}

/// Switch the current transaction's role to `owner` for the duration of the
/// agent loop (privilege isolation). The role name is escaped with
/// `quote_ident` in SQL so it can't break out of the `SET ROLE` statement.
/// `SET LOCAL` scopes the change to the current transaction; it is undone by
/// [`reset_role`] or automatically on transaction end.
fn set_local_role(owner: &str) -> Result<()> {
    // Use set_config('role', <value>, is_local=true) rather than building a
    // `SET ROLE <ident>` string: the role name is passed as a bound PARAMETER
    // (a value, not spliced SQL text), so there is no identifier-injection
    // surface at all — set_config takes the raw role name and validates it
    // against pg_authid itself. true = SET LOCAL (transaction-scoped).
    Spi::run_with_args(
        "SELECT pg_catalog.set_config('role', $1, true)",
        &[owner.into()],
    )?;
    Ok(())
}

/// Restore the worker's own role after the agent loop. Best-effort: a
/// failure here is non-fatal because the surrounding transaction (worker) or
/// statement (sync drain) will reset `role` on its own boundary anyway.
fn reset_role() {
    let _ = Spi::run("RESET role");
}

/// Mark a running job done. Wrapper over `ask._job_complete`.
fn complete(id: Uuid, answer: &str, prompt_tokens: i64, completion_tokens: i64) -> Result<bool> {
    let ok: Option<bool> = Spi::get_one_with_args(
        "SELECT ask._job_complete($1, $2, $3, $4)",
        &[
            id.into(),
            answer.into(),
            prompt_tokens.into(),
            completion_tokens.into(),
        ],
    )?;
    Ok(ok.unwrap_or(false))
}

/// Mark a running job failed (retry or terminal). Wrapper over
/// `ask._job_fail`; passes `jobs_max_attempts` so the retry policy lives in
/// one place. Returns the resulting status text, or `None` if the row was no
/// longer running (e.g. cancelled mid-flight).
fn fail(id: Uuid, error: &str) -> Result<Option<String>> {
    let max_attempts = JOBS_MAX_ATTEMPTS.get();
    let status: Option<String> = Spi::get_one_with_args(
        "SELECT ask._job_fail($1, $2, $3)",
        &[id.into(), error.into(), max_attempts.into()],
    )?;
    Ok(status)
}

/// Return a running job we just re-claimed back to `pending` WITHOUT
/// consuming an attempt. Wrapper over `ask._job_release`. Used by the
/// synchronous drain's poison-pill guard so a re-queued retry doesn't sit
/// in `running` until orphan recovery.
fn release(id: Uuid) -> Result<bool> {
    let ok: Option<bool> = Spi::get_one_with_args("SELECT ask._job_release($1)", &[id.into()])?;
    Ok(ok.unwrap_or(false))
}

/// Return orphaned `running` jobs (worker died) to `pending`. Wrapper over
/// `ask._job_recover_orphans`, passing the configured orphan timeout.
/// Returns how many were recovered.
pub fn recover_orphans() -> Result<i64> {
    let timeout_ms = JOBS_ORPHAN_TIMEOUT_MS.get();
    let n: Option<i64> =
        Spi::get_one_with_args("SELECT ask._job_recover_orphans($1)", &[timeout_ms.into()])?;
    Ok(n.unwrap_or(0))
}

/// Drain up to `max` pending jobs, each claimed-then-executed in sequence.
/// Returns the number of jobs processed (claimed and run, regardless of
/// success/failure outcome). Stops early when the queue is empty.
///
/// This is the synchronous batch used by `ask.run_pending_jobs()`. The whole
/// batch runs in the caller's transaction; `max` (from `jobs_batch`) bounds
/// how long that transaction stays open.
///
/// Poison-pill guard: because this runs in ONE transaction, a job that
/// `_job_fail` returns to `pending` (a transient failure with retries left)
/// is immediately visible to the next `claim_one` in this same loop — a
/// permanently-failing job could otherwise re-claim itself and burn the
/// whole batch budget, starving other pending jobs. We track the ids we've
/// already attempted this pass and skip a re-queued one, so each distinct
/// job is attempted at most once per drain; its retry happens on the next
/// drain (next worker poll / cron tick), giving fairness to other jobs.
pub fn drain(cfg: &RuntimeConfig, max: u32) -> Result<u32> {
    let mut processed = 0u32;
    let mut attempted: std::collections::HashSet<pgrx::Uuid> = std::collections::HashSet::new();
    for _ in 0..max {
        match claim_one()? {
            None => break,
            Some(job) => {
                if !attempted.insert(job.id) {
                    // Re-claimed a job we already ran this pass (it was
                    // requeued by a retry). `claim_one` already flipped it
                    // back to 'running'; release it to 'pending' (M1) so it
                    // doesn't stall in 'running' until orphan recovery, then
                    // stop — it and anything behind it are picked up on the
                    // next drain. Continuing would just spin on this job.
                    release(job.id)?;
                    break;
                }
                execute_claimed(cfg, &job)?;
                processed += 1;
            }
        }
    }
    Ok(processed)
}
