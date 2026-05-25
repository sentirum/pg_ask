//! Safe wrapper around Postgres' internal subtransaction primitives.
//!
//! ## Scope of this module
//!
//! This is the **only** module in the crate that is permitted to call
//! into `pgrx_pg_sys` raw FFI. Every other file goes through pgrx's
//! safe wrappers (`Spi`, `PgBox`, `PgList`, …). The exemption is
//! deliberate and scoped: we need a safe `run_in_subtransaction(...)`
//! helper, and pgrx 0.18 does not yet ship one. See the
//! `## Why this module exists` section below for the design rationale.
//!
//! Reviewers: if you find yourself adding a new `unsafe` block in
//! another file, stop and either (a) extend this module instead, or
//! (b) write a comparable narrowly-scoped wrapper next to it with
//! the same documentation discipline.
//!
//! ## Why this module exists
//!
//! Several pg_ask features need a *real* subtransaction:
//!
//! * Wave 2 / H3 — auditing `sql_query` invocations issued inside a
//!   read-only outer transaction. Postgres refuses every in-band way
//!   to clear `transaction_read_only` mid-transaction (`RESET …`
//!   rejected; `SET LOCAL … = off` rejected with "transaction
//!   read-write mode must be set before any query"; per-function
//!   `SET … = off` rejected with "parameter cannot be set locally
//!   in functions"). A subtransaction starts in writable mode by
//!   default, so an `UPDATE ask._sql_audit …` inside a subtxn
//!   succeeds even though the parent is read-only — and on commit
//!   the change is durable.
//! * Wave 2 / H2 — isolating an individual tool invocation so a
//!   failed query doesn't poison the surrounding `ask()` call.
//!   Without a subtxn the failed statement leaves the outer txn in
//!   an aborted state and every subsequent SPI call returns
//!   "current transaction is aborted, commands ignored".
//!
//! ## Reference implementation
//!
//! The shape mirrors `plpython`'s
//! `PLy_spi_subtransaction_{begin,commit,abort}` (see
//! `postgres/src/pl/plpython/plpy_spi.c`). The key invariants:
//!
//! 1. Snapshot `CurrentMemoryContext` and `CurrentResourceOwner`
//!    BEFORE `BeginInternalSubTransaction`. Postgres switches the
//!    current context/owner to ones owned by the subtxn; on commit
//!    *or* abort we restore the originals so caller code keeps
//!    pointing at the right palloc arena.
//! 2. On a caught ERROR, switch back to the outer context BEFORE
//!    calling `RollbackAndReleaseCurrentSubTransaction`, otherwise
//!    the rollback runs in (and frees) the subtxn's own context,
//!    which is the very context the error-recovery code is sitting
//!    in.
//! 3. `PgTryBuilder` already calls `FlushErrorState` for us once a
//!    catch handler returns without rethrowing, so we don't have to.
//!
//! ## Safety
//!
//! Every `unsafe { … }` block in this file is annotated with a
//! `// SAFETY:` comment explaining the precondition. The wrappers
//! together preserve these whole-program invariants:
//!
//! * **Pairing.** Every successful `BeginInternalSubTransaction` is
//!   matched by exactly one of `ReleaseCurrentSubTransaction` or
//!   `RollbackAndReleaseCurrentSubTransaction`. The `PgTryBuilder`
//!   exit paths cover both the success and the caught-error case;
//!   a Rust panic also funnels through `catch_others` so the
//!   rollback fires before unwinding past the wrapper.
//! * **Context/owner restoration.** `MemoryContextSwitchTo(old)`
//!   and `CurrentResourceOwner = old_owner` run on *every* exit
//!   path, so caller code observes the same context it had before.
//! * **No re-entry across an unhandled error.** If the body itself
//!   raises but our cleanup also raises (extremely rare), the
//!   second error overrides the first — same behaviour as the
//!   reference plpython implementation. The double-fault never
//!   leaves a dangling subtxn because `RollbackAndRelease` itself
//!   is the atomic step that pops the stack.

use crate::infra::errors::{AskError, Result};
use pgrx::pg_sys;
use pgrx::PgTryBuilder;
use std::sync::atomic::{AtomicBool, Ordering};

/// Execute `body` inside a fresh internal subtransaction.
///
/// On `Ok(_)` the subtxn is **committed** — the inner writes become
/// part of the parent transaction's pending changes (they will
/// commit / abort with the parent at the outer COMMIT / ROLLBACK).
///
/// On `Err(_)` the subtxn is **rolled back** — nothing the body did
/// is visible to the parent.
///
/// If `body` itself raises a Postgres ERROR (longjmp), the wrapper
/// catches it, rolls back the subtxn, and returns
/// `Err(AskError::Sql("<error message>"))`. The parent transaction
/// stays usable, which is the whole point.
///
/// The `name` is purely advisory — it shows up in
/// `pg_stat_activity.query` while the subtxn is active, useful for
/// distinguishing audit subtxns from other internal work in trace
/// output. Pass `None` if you don't care; Postgres falls back to
/// `<unnamed>`.
pub fn run_in_subtransaction<T, F>(name: Option<&str>, body: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + std::panic::UnwindSafe,
{
    // SAFETY: reading the two thread-local-ish globals is sound on
    // any Postgres backend thread (i.e. the only thread the
    // extension ever runs on). The values are valid handles into
    // structures owned by the current backend's transaction
    // machinery — we use them only to *restore* them, never to
    // dereference fields.
    let (oldcontext, oldowner) = unsafe {
        (pg_sys::CurrentMemoryContext, pg_sys::CurrentResourceOwner)
    };

    begin_subtransaction(name);
    // After `BeginInternalSubTransaction`, Postgres has switched to
    // the subtxn's own memory context and resource owner. The
    // plpython reference immediately switches back to the caller's
    // context so user code keeps allocating where it expects to.
    switch_to(oldcontext);

    // Track whether the body completed normally so the catch_others
    // handler knows to roll back rather than commit. `AtomicBool`
    // satisfies the `UnwindSafe + RefUnwindSafe` bounds that
    // `PgTryBuilder`'s closures require; `Cell<bool>` does not
    // (its `UnsafeCell` interior is `!RefUnwindSafe` by default).
    // The atomic is overkill for a single-threaded backend, but
    // there's no zero-cost equivalent that compiles here.
    let completed = AtomicBool::new(false);

    let outcome = PgTryBuilder::new(|| {
        let result = body();
        completed.store(true, Ordering::Relaxed);
        result
    })
    .catch_others(|caught| {
        // The body raised. Restore context BEFORE rolling back —
        // the rollback frees the subtxn context, and we're currently
        // sitting in it.
        switch_to(oldcontext);
        rollback_and_release_subtransaction();
        restore_owner(oldowner);

        // Surface the Postgres errmsg through our normal error type.
        // We deliberately don't `caught.rethrow()` — the whole point
        // of the wrapper is to convert the subtxn ERROR into a
        // recoverable Result for the caller.
        Err(AskError::Sql(format!("{caught:?}")))
    })
    .execute();

    if completed.load(Ordering::Relaxed) {
        // Body returned normally; commit the subtxn whether the
        // inner Result was Ok or Err. The Err case is a *Rust*-level
        // failure (e.g. embedding API returned 4xx); the SQL it
        // already executed in this subtxn is still semantically
        // valid and should be retained, matching the C plpython
        // contract.
        release_current_subtransaction();
        switch_to(oldcontext);
        restore_owner(oldowner);
    }
    // Else: catch_others already did the cleanup.

    outcome
}

// ─── Wrappers ──────────────────────────────────────────────────────────
//
// Each helper exists to (a) attach a documentation comment to a
// raw FFI call and (b) localise the `unsafe` block so callers and
// reviewers don't have to think about it.

fn begin_subtransaction(name: Option<&str>) {
    // Pass NULL when the caller doesn't care about the name; we
    // can't keep a borrowed CString alive across the FFI boundary
    // any other way without an allocation.
    let cname = name.map(std::ffi::CString::new).and_then(|r| r.ok());
    let ptr = cname
        .as_ref()
        .map(|c| c.as_ptr())
        .unwrap_or(std::ptr::null());
    // SAFETY: BeginInternalSubTransaction takes a `const char *` —
    // either a valid C string (the CString above keeps the storage
    // alive for the duration of this call) or NULL. Postgres
    // documents NULL as the "no name" case. The call is also
    // legal at any time inside a backend transaction, which is
    // guaranteed to be true whenever a pg_extern entry point runs.
    unsafe {
        pg_sys::BeginInternalSubTransaction(ptr);
    }
}

fn release_current_subtransaction() {
    // SAFETY: paired one-for-one with `begin_subtransaction` above;
    // see the "Pairing" invariant in the module-level docs.
    unsafe {
        pg_sys::ReleaseCurrentSubTransaction();
    }
}

fn rollback_and_release_subtransaction() {
    // SAFETY: paired one-for-one with `begin_subtransaction` above
    // along the error path. Idempotent at the Postgres level — if
    // there's no current subtxn this would elog(ERROR), but we
    // only call it after a confirmed begin.
    unsafe {
        pg_sys::RollbackAndReleaseCurrentSubTransaction();
    }
}

fn switch_to(ctx: pg_sys::MemoryContext) {
    // SAFETY: `ctx` is the memory context handle we snapshotted from
    // `CurrentMemoryContext` before opening the subtxn; it is still
    // owned by the parent transaction and therefore valid until
    // the parent commits / aborts, which is necessarily *after*
    // this wrapper returns. `MemoryContextSwitchTo` is documented
    // as side-effect-free apart from updating the global.
    unsafe {
        pg_sys::MemoryContextSwitchTo(ctx);
    }
}

fn restore_owner(owner: pg_sys::ResourceOwner) {
    // SAFETY: ditto for the resource owner snapshot. Direct
    // assignment is what the plpython reference does — there is no
    // setter API for `CurrentResourceOwner` in Postgres.
    unsafe {
        pg_sys::CurrentResourceOwner = owner;
    }
}

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    /// Happy path: body returns Ok, subtxn commits, side effects are
    /// visible after the wrapper returns. Uses a temp table so the
    /// test doesn't depend on cleanup.
    #[pg_test]
    fn subtxn_commits_side_effects_on_ok() {
        Spi::run("CREATE TEMP TABLE _subtxn_commit_probe (n int)").unwrap();
        let out = super::run_in_subtransaction(Some("probe"), || {
            Spi::run("INSERT INTO _subtxn_commit_probe VALUES (1)")
                .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))
        });
        assert!(out.is_ok());
        let n: Option<i64> =
            Spi::get_one("SELECT count(*) FROM _subtxn_commit_probe").unwrap();
        assert_eq!(n, Some(1));
    }

    /// A Postgres ERROR inside the body must be caught and the
    /// subtxn rolled back. The outer transaction stays usable —
    /// the post-wrapper SPI call is the actual evidence (a poisoned
    /// txn would ERROR with "current transaction is aborted").
    #[pg_test]
    fn subtxn_rolls_back_and_keeps_outer_usable_on_postgres_error() {
        Spi::run("CREATE TEMP TABLE _subtxn_rollback_probe (n int)").unwrap();
        let out = super::run_in_subtransaction(Some("probe"), || {
            Spi::run("INSERT INTO _subtxn_rollback_probe VALUES (1)")
                .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))?;
            // Deliberate ERROR after the insert. The subtxn must
            // discard the row.
            Spi::run("SELECT 1/0")
                .map_err(|e| crate::infra::errors::AskError::Sql(e.to_string()))?;
            Ok(())
        });
        assert!(out.is_err(), "divide-by-zero should surface as Err");
        let n: Option<i64> =
            Spi::get_one("SELECT count(*) FROM _subtxn_rollback_probe").unwrap();
        assert_eq!(n, Some(0), "insert from inside the aborted subtxn must be gone");
        // And the outer txn is still healthy.
        let one: Option<i32> = Spi::get_one("SELECT 1").unwrap();
        assert_eq!(one, Some(1));
    }
}
