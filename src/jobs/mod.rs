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
//! The agent loop runs *between* the two _jobs writes, not inside a row
//! lock, because step 1 commits the `running` transition (when the driver
//! commits) before the slow work begins. The worker driver commits after
//! each job; the manual driver runs the whole batch in one transaction,
//! which is acceptable for a bounded `jobs_batch`.

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
}

/// Atomically claim the oldest pending job for the current database, if any.
///
/// Thin wrapper over `ask._job_claim()` (which does the FOR UPDATE SKIP
/// LOCKED + pending → running transition). Returns `None` when the queue is
/// empty. Maps the textual `kind` column to [`AgentMode`]; an unknown value
/// defaults to `Execute` (the `ask` mode) rather than failing the drain.
pub fn claim_one() -> Result<Option<ClaimedJob>> {
    Spi::connect(|client| {
        let tup = client
            .select(
                "SELECT id, kind, question FROM ask._job_claim()",
                Some(1),
                &[],
            )?
            .first();
        if tup.is_empty() {
            return Ok(None);
        }
        // Single-row, three columns: id (uuid), kind (text), question (text).
        let (id, kind_txt, question) = tup.get_three::<Uuid, String, String>()?;
        let id = id.ok_or_else(|| crate::infra::errors::AskError::Sql("claim: null id".into()))?;
        let question = question
            .ok_or_else(|| crate::infra::errors::AskError::Sql("claim: null question".into()))?;
        let kind = match kind_txt.as_deref() {
            Some("sql") => AgentMode::GenerateOnly,
            _ => AgentMode::Execute,
        };
        Ok(Some(ClaimedJob { id, kind, question }))
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

    // Run the agent loop with a trace row, mirroring the synchronous path.
    let mut rec = TraceRecord::start(kind, cfg, &job.question);
    let run = agent::run_with_cfg(cfg, &job.question, Vec::new(), job.kind);

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
/// how long that transaction stays open. The worker driver instead calls
/// `claim_one` / `execute_claimed` per job inside its own per-job
/// transaction for tighter isolation.
pub fn drain(cfg: &RuntimeConfig, max: u32) -> Result<u32> {
    let mut processed = 0u32;
    for _ in 0..max {
        match claim_one()? {
            None => break,
            Some(job) => {
                execute_claimed(cfg, &job)?;
                processed += 1;
            }
        }
    }
    Ok(processed)
}
