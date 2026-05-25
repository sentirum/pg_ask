//! Background worker prototype (v0.5).
//!
//! Decouples long-running agent work from the calling backend by running
//! the loop in a separate background worker process. The caller will
//! eventually submit a question to `ask._jobs` and poll
//! `ask._job_results` for the answer. This is a stub — the full loop
//! wiring lands in v0.6.

use pgrx::bgworkers::*;
use pgrx::prelude::*;
use std::time::Duration;

/// Register the background worker. Called from `_PG_init` when the
/// extension is loaded via `shared_preload_libraries`.
pub fn register() {
    // Workers can only be registered while Postgres is processing
    // shared_preload_libraries. If the extension is loaded dynamically
    // (e.g. `LOAD 'pg_ask'`), we silently skip registration.
    if unsafe { !pgrx::pg_sys::process_shared_preload_libraries_in_progress } {
        return;
    }
    BackgroundWorkerBuilder::new("pg_ask worker")
        .set_function("pg_ask_worker_main")
        .set_library("pg_ask")
        .enable_spi_access()
        .load();
}

/// Main entry point for the background worker process.
#[pg_guard]
#[no_mangle]
pub extern "C-unwind" fn pg_ask_worker_main(_arg: pg_sys::Datum) {
    BackgroundWorker::attach_signal_handlers(
        SignalWakeFlags::SIGHUP | SignalWakeFlags::SIGTERM,
    );
    BackgroundWorker::connect_worker_to_spi(Some("postgres"), None);

    log!("pg_ask worker started");

    while BackgroundWorker::wait_latch(Some(Duration::from_secs(5))) {
        if BackgroundWorker::sighup_received() {
            // In v0.6: reload GUCs / config here.
        }
        // v0.5 stub: heartbeat only. v0.6 will poll ask._jobs,
        // run agent::run() against pending rows, and write results back.
        log!("pg_ask worker heartbeat");
    }

    log!("pg_ask worker shutting down");
}
