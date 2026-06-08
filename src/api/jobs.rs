//! Async job queue SQL surface (v0.5.9 / ADR-0018).
//!
//! Thin `#[pg_extern]` wrappers. Enqueue is one SECURITY DEFINER call;
//! status/result are owner-scoped SELECTs; the drain entry point delegates
//! to [`crate::jobs`] (the use-case layer). No business logic here.

use crate::infra::config::{RuntimeConfig, JOBS_BATCH};
use crate::infra::errors::raise_as_pg_error;
use crate::jobs;
use pgrx::prelude::*;
use pgrx::Uuid;

/// Enqueue a question for asynchronous execution. Returns the new job id
/// immediately (the agent loop runs later in a worker), or NULL when the job
/// queue is disabled (`pg_ask.jobs_enabled = off`, the default).
///
/// `kind` is `'ask'` (full agent loop, default) or `'sql'` (generate-only).
/// Poll the result with `ask.job_status(id)` / `ask.job_result(id)`, or
/// `LISTEN pg_ask_jobs_done` for a low-latency wake-up.
///
/// ```sql
/// SELECT ask.ask_async('top 5 customers by revenue');
/// SELECT ask.ask_async('count orders today', 'sql');
/// ```
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn ask_async(question: &str, kind: default!(&str, "'ask'")) -> Option<Uuid> {
    match Spi::get_one_with_args::<Uuid>(
        "SELECT ask._job_submit($1, $2)",
        &[kind.into(), question.into()],
    ) {
        Ok(id) => id,
        Err(e) => raise_as_pg_error(&crate::infra::errors::AskError::from(e)),
    }
}

/// Current status of a job you own: `pending` / `running` / `done` /
/// `failed` / `cancelled`, or NULL if no such job belongs to you (the
/// NotFound == Unauthorized collapse the rest of pg_ask uses, so id-space
/// probing leaks nothing).
#[pg_extern(schema = "ask", stable, parallel_safe)]
fn job_status(job_id: Uuid) -> Option<String> {
    // Scalar subquery so the outer SELECT always returns exactly one row
    // (the value or NULL). A bare `SELECT ... WHERE` returns zero rows when
    // nothing matches, which Spi::get_one surfaces as a "positioned before
    // the start" error instead of the clean None we want for the
    // NotFound == Unauthorized collapse.
    match Spi::get_one_with_args::<String>(
        "SELECT (SELECT status FROM ask._jobs WHERE id = $1 AND owner = session_user)",
        &[job_id.into()],
    ) {
        Ok(s) => s,
        Err(e) => raise_as_pg_error(&crate::infra::errors::AskError::from(e)),
    }
}

/// The answer text of a completed job you own. NULL while the job is still
/// pending/running, if it failed (use `ask.job_error`), or if no such job
/// belongs to you.
#[pg_extern(schema = "ask", stable, parallel_safe)]
fn job_result(job_id: Uuid) -> Option<String> {
    match Spi::get_one_with_args::<String>(
        "SELECT (SELECT answer FROM ask._jobs \
         WHERE id = $1 AND owner = session_user AND status = 'done')",
        &[job_id.into()],
    ) {
        Ok(s) => s,
        Err(e) => raise_as_pg_error(&crate::infra::errors::AskError::from(e)),
    }
}

/// The error text of a failed job you own, or NULL if it didn't fail / isn't
/// yours. Separate from `job_result` so a caller can tell "no answer yet"
/// from "answer is empty".
#[pg_extern(schema = "ask", stable, parallel_safe)]
fn job_error(job_id: Uuid) -> Option<String> {
    match Spi::get_one_with_args::<String>(
        "SELECT (SELECT error FROM ask._jobs \
         WHERE id = $1 AND owner = session_user AND status = 'failed')",
        &[job_id.into()],
    ) {
        Ok(s) => s,
        Err(e) => raise_as_pg_error(&crate::infra::errors::AskError::from(e)),
    }
}

/// Cancel a job you own that is still pending or running. Returns true if it
/// was cancelled, false if it was already terminal or not yours. An
/// in-flight worker's result is discarded (the completion helpers refuse to
/// transition a non-running row).
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn cancel_job(job_id: Uuid) -> bool {
    match Spi::get_one_with_args::<bool>("SELECT ask._job_cancel($1)", &[job_id.into()]) {
        Ok(ok) => ok.unwrap_or(false),
        Err(e) => raise_as_pg_error(&crate::infra::errors::AskError::from(e)),
    }
}

/// Synchronously drain up to `pg_ask.jobs_batch` pending jobs in the current
/// database, running each agent loop in this transaction, and return how
/// many were processed. Intended for installs without the background worker
/// (e.g. driven by pg_cron) or for tests. Returns 0 immediately when the
/// queue is disabled.
///
/// Also recovers orphaned `running` jobs (from a crashed worker) first, so a
/// pg_cron-only deployment still gets crash recovery.
///
/// ```sql
/// SELECT cron.schedule('pg_ask-drain', '10 seconds', $$SELECT ask.run_pending_jobs()$$);
/// ```
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn run_pending_jobs() -> i64 {
    let cfg = match RuntimeConfig::load() {
        Ok(c) => c,
        Err(e) => raise_as_pg_error(&e),
    };
    // Recover orphans first so a pg_cron-only deployment self-heals.
    if let Err(e) = jobs::recover_orphans() {
        raise_as_pg_error(&e);
    }
    let max = JOBS_BATCH.get().max(1) as u32;
    match jobs::drain(&cfg, max) {
        Ok(n) => n as i64,
        Err(e) => raise_as_pg_error(&e),
    }
}

/// Prune terminal (done/failed/cancelled) jobs older than `older_than`
/// (interval literal, e.g. `'7 days'`), in batches of `batch_size`
/// (default 10000; `0` = single DELETE). Pending/running jobs are never
/// removed. Operator-only (not granted to PUBLIC; see finalize.sql).
#[pg_extern(schema = "ask", volatile, parallel_unsafe)]
fn prune_jobs(older_than: &str, batch_size: default!(i32, 10000)) -> i64 {
    match Spi::get_one_with_args::<i64>(
        "SELECT ask._jobs_prune($1::interval, $2)",
        &[older_than.into(), batch_size.into()],
    ) {
        Ok(n) => n.unwrap_or(0),
        Err(e) => raise_as_pg_error(&crate::infra::errors::AskError::from(e)),
    }
}
