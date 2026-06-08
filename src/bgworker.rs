//! Background workers for the async job queue (v0.5.9 / ADR-0018).
//!
//! PostgreSQL has no in-backend async: a backend is single-threaded and SPI
//! is not thread-safe. The only correct way to run work "in the background"
//! is a separate process — a `BackgroundWorker`. This module implements the
//! two-tier shape that lets ONE extension serve EVERY database:
//!
//! ```text
//!   launcher  (1 process, started from shared_preload_libraries)
//!     │  connects to the 'postgres' maintenance DB
//!     │  periodically lists databases that have pg_ask installed
//!     ▼
//!   per-DB worker  (1 dynamic process per pg_ask-enabled database)
//!        connects to THAT database, drains ask._jobs in a loop:
//!        recover_orphans → claim → run agent loop → complete/fail
//! ```
//!
//! Why two tiers: a single bgworker binds to exactly one database for its
//! whole life (`connect_worker_to_spi(dbname)`), but pg_ask can be installed
//! in many. The launcher discovers them and spawns a dynamic worker per DB,
//! re-reconciling on an interval so a `CREATE EXTENSION pg_ask` in a new
//! database picks up a worker without a restart.
//!
//! Everything is opt-in: workers only register when the library is in
//! `shared_preload_libraries`, and each per-DB worker no-ops unless
//! `pg_ask.jobs_enabled = on` in its database. An install that doesn't use
//! async pays only the launcher's idle reconcile loop.

use crate::infra::config::{RuntimeConfig, JOBS_BATCH, JOBS_ENABLED, JOBS_POLL_INTERVAL_MS};
use pgrx::bgworkers::*;
use pgrx::prelude::*;
use std::collections::HashSet;
use std::time::Duration;

/// Maintenance DB the launcher connects to in order to enumerate databases.
const LAUNCHER_DB: &str = "postgres";

/// How often the launcher re-scans for pg_ask-enabled databases (ms).
const LAUNCHER_RECONCILE_MS: u64 = 30_000;

/// Register the launcher. Called from `_PG_init` when the extension is
/// loaded via `shared_preload_libraries`. If loaded dynamically
/// (`LOAD 'pg_ask'`), registration is skipped — there is no postmaster
/// slot to attach to, and the synchronous `ask.run_pending_jobs()` path
/// remains available.
pub fn register() {
    if unsafe { !pgrx::pg_sys::process_shared_preload_libraries_in_progress } {
        return;
    }
    BackgroundWorkerBuilder::new("pg_ask launcher")
        .set_function("pg_ask_launcher_main")
        .set_library("pg_ask")
        .set_start_time(BgWorkerStartTime::RecoveryFinished)
        .enable_spi_access()
        // Restart after 10s if it ever exits unexpectedly.
        .set_restart_time(Some(Duration::from_secs(10)))
        .load();
}

/// Launcher entry point. Connects to the maintenance DB, then loops:
/// discover pg_ask-enabled databases and ensure each has a running per-DB
/// worker. Dynamic workers self-terminate if their database loses the
/// extension; the launcher simply stops tracking them.
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn pg_ask_launcher_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    BackgroundWorker::connect_worker_to_spi(Some(LAUNCHER_DB), None);
    log!("pg_ask launcher started");

    // Databases we've already spawned a worker for this launcher lifetime.
    // A dynamic worker with set_restart_time(None) that exits is gone for
    // good, so if a DB's worker dies the next reconcile re-spawns it.
    let mut spawned: HashSet<String> = HashSet::new();

    while BackgroundWorker::wait_latch(Some(Duration::from_millis(LAUNCHER_RECONCILE_MS))) {
        if BackgroundWorker::sighup_received() {
            // Nothing cached from GUCs here; reconcile picks up changes.
        }

        let dbs = match list_pgask_databases() {
            Ok(dbs) => dbs,
            Err(e) => {
                log!("pg_ask launcher: database scan failed: {e}");
                continue;
            }
        };

        for db in dbs {
            if spawned.contains(&db) {
                continue;
            }
            match spawn_db_worker(&db) {
                Ok(()) => {
                    log!("pg_ask launcher: spawned worker for database '{db}'");
                    spawned.insert(db);
                }
                Err(()) => {
                    log!("pg_ask launcher: failed to spawn worker for '{db}' (will retry)");
                }
            }
        }
    }

    log!("pg_ask launcher shutting down");
}

/// List databases that allow connections AND have the pg_ask extension
/// installed. Run in the launcher's maintenance-DB connection.
fn list_pgask_databases() -> Result<Vec<String>, String> {
    BackgroundWorker::transaction(|| {
        Spi::connect(|client| {
            // We cannot see other databases' pg_extension catalogs from here,
            // so we approximate: every connectable, non-template database is a
            // candidate, and the per-DB worker itself checks for the extension
            // on connect (cheap, and the authoritative place). This keeps the
            // launcher's query trivial and avoids dblink.
            // datname is the `name` type (Oid 19); cast to text so it maps to
            // Rust String cleanly.
            let rows = client.select(
                "SELECT datname::text FROM pg_database \
                 WHERE datallowconn AND NOT datistemplate \
                 ORDER BY datname",
                None,
                &[],
            )?;
            let mut out = Vec::new();
            for row in rows {
                if let Some(name) = row.get::<String>(1)? {
                    out.push(name);
                }
            }
            Ok::<_, pgrx::spi::SpiError>(out)
        })
    })
    .map_err(|e| e.to_string())
}

/// Spawn a dynamic per-database worker bound to `db`. The database name is
/// passed via `set_extra` so the worker knows where to connect.
fn spawn_db_worker(db: &str) -> Result<(), ()> {
    let handle = BackgroundWorkerBuilder::new(&format!("pg_ask worker: {db}"))
        .set_function("pg_ask_db_worker_main")
        .set_library("pg_ask")
        .set_extra(db)
        .enable_spi_access()
        // No auto-restart: if it exits (e.g. extension dropped) the launcher
        // decides whether to re-spawn on the next reconcile.
        .set_restart_time(None)
        .load_dynamic()
        .map_err(|_| ())?;
    // We don't wait for startup — the launcher stays responsive. The handle
    // is dropped; the worker keeps running independently.
    let _ = handle;
    Ok(())
}

/// Per-database worker entry point. Connects to the database named in
/// `get_extra()`, verifies pg_ask is installed, then drains the job queue
/// in a loop until the extension goes away or a SIGTERM arrives.
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn pg_ask_db_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM);
    let db = BackgroundWorker::get_extra().to_string();
    if db.is_empty() {
        log!("pg_ask worker: no database in extra; exiting");
        return;
    }
    BackgroundWorker::connect_worker_to_spi(Some(&db), None);

    // The launcher spawns a worker for every connectable database (it can't
    // see other DBs' catalogs to pre-filter). Databases without pg_ask — e.g.
    // the 'postgres' maintenance DB — exit immediately and quietly here, so a
    // worker only runs where the extension actually lives.
    match extension_present() {
        Ok(true) => {}
        Ok(false) => return,
        Err(e) => {
            log!("pg_ask worker for '{db}': extension check failed: {e}; exiting");
            return;
        }
    }
    log!("pg_ask worker for '{db}' started");

    // NB: a background worker CANNOT `LISTEN` — Postgres rejects it with
    // "cannot execute LISTEN within a background process" (async.c gates
    // LISTEN to regular client backends). So the worker is poll-driven: it
    // wakes every `jobs_poll_interval_ms` and drains the queue. The enqueue
    // path still fires pg_notify('pg_ask_jobs', id) — harmless here, and used
    // by any external LISTENer — but the worker's own latency floor is the
    // poll interval (default 5s). Keep that interval modest for snappier
    // async; it is a single indexed query per wake when the queue is empty.
    loop {
        let poll_ms = poll_interval_ms();
        let latched = BackgroundWorker::wait_latch(Some(Duration::from_millis(poll_ms)));
        if !latched {
            // wait_latch returns false only on SIGTERM (shutdown requested).
            break;
        }
        if BackgroundWorker::sighup_received() {
            // GUCs are re-read each pass from the snapshot, nothing to cache.
        }

        // If the extension is later dropped from this DB, the worker exits.
        match extension_present() {
            Ok(true) => {}
            Ok(false) => {
                log!("pg_ask worker for '{db}': extension dropped; exiting");
                break;
            }
            Err(e) => {
                log!("pg_ask worker for '{db}': extension check failed: {e}");
                continue;
            }
        }

        if let Err(e) = drain_once() {
            log!("pg_ask worker for '{db}': drain error: {e}");
        }
    }

    log!("pg_ask worker for '{db}' shutting down");
}

/// Read the poll interval GUC (per pass, so a SIGHUP change is picked up).
/// Falls back to the default if the value is somehow out of range.
fn poll_interval_ms() -> u64 {
    let v = JOBS_POLL_INTERVAL_MS.get();
    if v > 0 {
        v as u64
    } else {
        5_000
    }
}

/// Is the pg_ask extension installed in the worker's current database?
///
/// Uses `SELECT EXISTS(...)` so the query ALWAYS returns exactly one row
/// (true/false). A bare `SELECT true FROM pg_extension WHERE ...` returns
/// zero rows when the extension is absent, which `Spi::get_one` surfaces as
/// "SpiTupleTable positioned before the start or after the end" rather than
/// a clean `None`.
fn extension_present() -> Result<bool, String> {
    BackgroundWorker::transaction(|| {
        let present: Option<bool> =
            Spi::get_one("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'pg_ask')")
                .map_err(|e| e.to_string())?;
        Ok::<_, String>(present.unwrap_or(false))
    })
}

/// One drain pass. Each step gets its OWN transaction so a long LLM round-
/// trip never holds a single transaction open across the whole batch:
///
///   1. one txn: check the switch, recover orphans, snapshot config
///   2. per job: one txn to claim + run + complete/fail
///
/// Committing after each job means a worker crash loses at most the one
/// in-flight job (which orphan recovery then re-queues), and the `running`
/// transition is durable before the slow agent loop begins. No-op (cheap)
/// when jobs are disabled.
fn drain_once() -> Result<(), String> {
    // Step 1: cheap preamble in its own transaction.
    let cfg = BackgroundWorker::transaction(|| {
        if !JOBS_ENABLED.get() {
            return Ok::<_, String>(None);
        }
        let _recovered = crate::jobs::recover_orphans().map_err(|e| e.to_string())?;
        let cfg = RuntimeConfig::load().map_err(|e| e.to_string())?;
        Ok(Some(cfg))
    })?;

    let Some(cfg) = cfg else {
        return Ok(()); // jobs disabled
    };

    // Step 2: claim+run+complete each job in its own transaction.
    let max = JOBS_BATCH.get().max(1);
    for _ in 0..max {
        // SIGTERM mid-batch: stop promptly, leaving the rest pending.
        if BackgroundWorker::sigterm_received() {
            break;
        }
        let did_work = BackgroundWorker::transaction(|| {
            match crate::jobs::claim_one().map_err(|e| e.to_string())? {
                None => Ok::<bool, String>(false),
                Some(job) => {
                    crate::jobs::execute_claimed(&cfg, &job).map_err(|e| e.to_string())?;
                    Ok(true)
                }
            }
        })?;
        if !did_work {
            break; // queue drained
        }
    }
    Ok(())
}
