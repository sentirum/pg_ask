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
use std::collections::HashMap;
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

    // Per-database worker handles, kept so we can check liveness each
    // reconcile and RESPAWN a worker that has died. A dynamic worker is
    // created with set_restart_time(None) (the postmaster won't restart it),
    // so respawning is the launcher's job: if `handle.pid()` no longer
    // reports `Started`, the worker exited (crash, OOM, or extension drop)
    // and we drop the handle so the spawn loop below recreates it.
    //
    // Restart safety (B3): when the postmaster restarts THIS launcher, the
    // old launcher's dynamic workers keep running (Postgres does not
    // parent-kill them) but this fresh process starts with an empty map. To
    // avoid spawning a duplicate worker per database on every launcher
    // restart, the spawn loop also checks `pg_stat_activity` for an existing
    // 'pg_ask worker: {db}' backend and skips databases that already have a
    // live worker. On a clean shutdown we additionally terminate our own
    // workers so they don't outlive the launcher.
    let mut workers: HashMap<String, DynamicBackgroundWorker> = HashMap::new();

    while BackgroundWorker::wait_latch(Some(Duration::from_millis(LAUNCHER_RECONCILE_MS))) {
        if BackgroundWorker::sighup_received() {
            // Nothing cached from GUCs here; reconcile picks up changes.
        }

        // Drop handles for workers that are no longer running so they get
        // respawned below. `pid()` returns Ok(Started) only while alive.
        workers.retain(|db, handle| {
            let alive = handle.pid().is_ok();
            if !alive {
                log!("pg_ask launcher: worker for '{db}' is gone; will respawn");
            }
            alive
        });

        let dbs = match list_pgask_databases() {
            Ok(dbs) => dbs,
            Err(e) => {
                log!("pg_ask launcher: database scan failed: {e}");
                continue;
            }
        };

        for db in dbs {
            if workers.contains_key(&db) {
                continue; // a live worker (ours) already owns this database
            }
            // B3: a worker from a previous launcher lifetime may still be
            // running this database. Don't spawn a duplicate.
            match db_worker_running(&db) {
                Ok(true) => continue,
                Ok(false) => {}
                Err(e) => {
                    log!("pg_ask launcher: worker-presence check for '{db}' failed: {e}");
                    continue; // be conservative: skip this round, retry next
                }
            }
            match spawn_db_worker(&db) {
                Ok(handle) => {
                    log!("pg_ask launcher: spawned worker for database '{db}'");
                    workers.insert(db, handle);
                }
                Err(()) => {
                    log!("pg_ask launcher: failed to spawn worker for '{db}' (will retry)");
                }
            }
        }
    }

    // Clean shutdown (SIGTERM): terminate our dynamic workers so they don't
    // outlive the launcher and get duplicated by the next launcher instance.
    for (db, handle) in workers.drain() {
        log!("pg_ask launcher: terminating worker for '{db}'");
        let _ = handle.terminate();
    }
    log!("pg_ask launcher shutting down");
}

/// Is there already a live 'pg_ask worker: {db}' backend? Used to avoid
/// spawning a duplicate when this launcher restarted while the previous
/// launcher's dynamic workers are still running (B3). Matches the bgw name
/// we set in `spawn_db_worker`.
fn db_worker_running(db: &str) -> Result<bool, String> {
    BackgroundWorker::transaction(|| {
        let present: Option<bool> = Spi::get_one_with_args(
            "SELECT EXISTS (SELECT 1 FROM pg_stat_activity \
             WHERE backend_type = $1)",
            &[format!("pg_ask worker: {db}").into()],
        )
        .map_err(|e| e.to_string())?;
        Ok::<_, String>(present.unwrap_or(false))
    })
}

/// List databases that allow connections AND have the pg_ask extension
/// installed. Run in the launcher's maintenance-DB connection.
fn list_pgask_databases() -> Result<Vec<String>, String> {
    BackgroundWorker::transaction(|| {
        // The launcher runs in the 'postgres' maintenance DB, where the
        // pg_ask extension (and the `ask` schema) is NOT installed — so we
        // CANNOT call any `ask.*` helper here. We discover pg_ask-enabled
        // databases inline:
        //
        //   * pg_extension is per-database, so we use dblink (standard
        //     contrib) to probe each connectable database's catalog and keep
        //     only the ones with pg_ask. This stops the launcher from
        //     endlessly respawning a short-lived worker in a database that
        //     will never have the extension (e.g. 'postgres' itself).
        //   * Each probe runs in its own subtransaction so an unreachable
        //     database (permissions, conn cap) is skipped, not fatal.
        //   * If dblink is not installed in this maintenance DB we cannot
        //     probe; fall back to every connectable database and rely on the
        //     per-DB worker's own extension_present() check (noisier:
        //     respawn churn for non-pg_ask DBs, but correct).
        //
        // dblink must be installed in the launcher's database (CREATE
        // EXTENSION dblink in 'postgres'); the Docker image's initdb hook
        // does this automatically.
        Spi::connect(|client| {
            let have_dblink = client
                .select(
                    "SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'dblink')",
                    Some(1),
                    &[],
                )?
                .first()
                .get::<bool>(1)?
                == Some(true);

            let rows = client.select(
                "SELECT datname::text FROM pg_database \
                 WHERE datallowconn AND NOT datistemplate ORDER BY datname",
                None,
                &[],
            )?;
            let mut candidates = Vec::new();
            for row in rows {
                if let Some(name) = row.get::<String>(1)? {
                    candidates.push(name);
                }
            }

            // No dblink: return all candidates (worker self-check is backstop).
            if !have_dblink {
                return Ok::<_, pgrx::spi::SpiError>(candidates);
            }

            // dblink available: keep only databases that truly have pg_ask.
            let mut installed = Vec::new();
            for db in candidates {
                // Each probe in its own subtransaction so a connect failure
                // doesn't abort the whole scan.
                let probed =
                    crate::infra::subtxn::run_in_subtransaction(Some("pgask_probe"), || {
                        // quote_LITERAL, not quote_ident: this is a libpq
                        // conninfo value, not a SQL identifier. quote_ident
                        // produces dbname="MyDb" which libpq reads as a DB
                        // literally named '"MyDb"' (quotes included) and fails
                        // — silently skipping every DB whose name has uppercase
                        // or special chars. quote_literal yields dbname='MyDb',
                        // the correct conninfo form.
                        let ok: Option<bool> = Spi::get_one_with_args(
                            "SELECT ok FROM dblink('dbname=' || quote_literal($1), \
                             'SELECT EXISTS (SELECT 1 FROM pg_extension \
                             WHERE extname = ''pg_ask'')') AS t(ok bool)",
                            &[db.clone().into()],
                        )?;
                        Ok(ok.unwrap_or(false))
                    })
                    .unwrap_or(false);
                if probed {
                    installed.push(db);
                }
            }
            Ok::<_, pgrx::spi::SpiError>(installed)
        })
    })
    .map_err(|e| e.to_string())
}

/// Spawn a dynamic per-database worker bound to `db`. The database name is
/// passed via `set_extra` so the worker knows where to connect. Returns the
/// handle so the launcher can check liveness and respawn on the next
/// reconcile if the worker dies.
fn spawn_db_worker(db: &str) -> Result<DynamicBackgroundWorker, ()> {
    BackgroundWorkerBuilder::new(&format!("pg_ask worker: {db}"))
        .set_function("pg_ask_db_worker_main")
        .set_library("pg_ask")
        .set_extra(db)
        .enable_spi_access()
        // No auto-restart: the launcher owns respawn (it checks pid() each
        // reconcile), which lets us also stop respawning a DB that has
        // dropped the extension.
        .set_restart_time(None)
        .load_dynamic()
        .map_err(|_| ())
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

/// One drain pass. The three phases each get their OWN transaction so the
/// durability guarantees are real (see the `src/jobs` module docs):
///
///   1. one txn: check the switch, recover orphans, snapshot config
///   2. per job, in SEPARATE transactions:
///        a. claim   — commits `running` (durable + visible) before slow work
///        b. execute — agent loop (no _jobs lock held), then complete/fail
///
/// Splitting claim (2a) from execute (2b) is what makes orphan recovery
/// meaningful: a crash during the agent loop leaves a committed `running`
/// row that `_job_recover_orphans` can return to `pending`. A combined
/// claim+execute txn would roll the claim back on crash, so `running` would
/// never be visible and recovery would be dead code.
///
/// SIGTERM responsiveness: the flag is checked between every job (before each
/// claim), so a shutdown takes effect within at most ONE job's runtime. A
/// shutdown that arrives mid-agent-loop still waits for that single job's
/// LLM call to return or hit `pg_ask.http_total_timeout_ms` — a background
/// worker's SIGTERM sets `ShutdownRequestPending`, which does NOT trip the
/// `check_for_interrupts!()` path the agent loop uses, so we cannot abort the
/// in-flight HTTP call cleanly. Operators who need a hard upper bound on
/// shutdown latency should keep `http_total_timeout_ms` modest. No-op
/// (cheap) when jobs are disabled.
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

    // Step 2: each job in its own claim txn + execute txn.
    let max = JOBS_BATCH.get().max(1);
    for _ in 0..max {
        // Stop promptly on shutdown, leaving the rest pending.
        if BackgroundWorker::sigterm_received() {
            break;
        }

        // 2a. Claim in its own transaction so `running` is COMMITTED before
        //     the agent loop runs. Returns None when the queue is empty.
        let claimed =
            BackgroundWorker::transaction(|| crate::jobs::claim_one().map_err(|e| e.to_string()))?;
        let Some(job) = claimed else {
            break; // queue drained
        };

        // 2b. Execute + complete/fail in a separate transaction. If the
        //     worker crashes here, the committed `running` row from 2a is
        //     reclaimed by orphan recovery on the next pass / restart.
        BackgroundWorker::transaction(|| {
            crate::jobs::execute_claimed(&cfg, &job).map_err(|e| e.to_string())
        })?;
    }
    Ok(())
}
