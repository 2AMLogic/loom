//! Test-only, crate-wide isolation for `LOOM_OPENCODE_DB` (issue #15204).
//!
//! The variable is process-global, and every `#[test]` in this crate links
//! into one multi-threaded binary. Tests mutate it from two groups of
//! `serial_test` keys that do not exclude each other: `#[serial(opencode_db_env)]`
//! (`opencode_usage`, `step_dispatch`, `usage_source`, `outcome_journal`) and
//! the crate-default `#[serial]` (`step_completion_tests`' `*_body` fns). A
//! default-keyed writer could therefore `remove_var`/`set_var` the variable
//! between a differently-keyed writer's `set_var` and its read — the same
//! defect class #15174 closed for `LOOM_OPENCODE_DB`.
//!
//! [`lock`] closes it the way [`codex_home_env`] (#15174) and
//! [`profile_root_env`] (#9964) did for their variables: one crate-wide lock
//! that **every** test mutating the variable holds for its whole scope,
//! whatever `serial_test` key it also carries. Re-entrant per thread, so a
//! nested helper does not self-deadlock. [`OpencodeDbEnv`] is the RAII form:
//! it holds the lock and restores the prior value on drop, including during a
//! panic unwind.
//!
//! # Lock order
//!
//! `serial_test` named keys outermost, then the crate-default key (#14404),
//! then [`codex_home_env`], then this mutex, then [`profile_root_env`]
//! innermost. Acquire `opencode_db_env` *after* `codex_home_env` and
//! *before* `profile_root_env`; release in the reverse order. Every helper
//! follows that chain, so it can never form a cycle with any of the lock
//! families.
//!
//! [`codex_home_env`]: super::codex_home_env
//! [`profile_root_env`]: super::profile_root_env

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::opencode_usage::OPENCODE_DB_ENV;

static LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Number of live [`OpencodeDbLock`]s on this thread.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    /// The real mutex guard, held while `DEPTH > 0`. Kept here rather than in
    /// any one handle so handles may drop in any order on the owning thread.
    static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
}

/// Proof that the current thread holds the crate-wide opencode-db lock.
/// `!Send`: it must be dropped on the thread that took it.
#[must_use = "the lock is released as soon as this is dropped"]
pub(crate) struct OpencodeDbLock {
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Take the crate-wide `LOOM_OPENCODE_DB` lock (re-entrant per thread).
///
/// Multi-variable test env guards that save/clear/restore this variable among
/// others hold one of these for their lifetime, and so does every test that
/// pins the variable directly.
pub(crate) fn lock() -> OpencodeDbLock {
    if DEPTH.with(Cell::get) == 0 {
        // A test that panicked while holding the lock poisons it; the env it
        // touched was still restored by its guard's `Drop`, so the poison
        // carries no information here.
        let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        HELD.with(|held| *held.borrow_mut() = Some(guard));
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
    OpencodeDbLock {
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for OpencodeDbLock {
    fn drop(&mut self) {
        let remaining = DEPTH.with(|depth| {
            let next = depth.get().saturating_sub(1);
            depth.set(next);
            next
        });
        if remaining == 0 {
            // Take the guard out before dropping it so the `RefCell` borrow is
            // released first.
            let guard = HELD.with(|held| held.borrow_mut().take());
            drop(guard);
        }
    }
}

/// RAII pin of `LOOM_OPENCODE_DB`: holds [`lock`] and restores the variable's
/// prior value (or its absence) on drop — including on panic.
#[must_use = "the pin is undone as soon as this is dropped"]
pub(crate) struct OpencodeDbEnv {
    prior: Option<OsString>,
    // Declared last so it drops after `Drop::drop` restored the variable.
    _lock: OpencodeDbLock,
}

impl OpencodeDbEnv {
    /// Point the variable at `value` (a tempdir `opencode.db`, typically).
    pub(crate) fn set(value: impl AsRef<OsStr>) -> Self {
        let lock = lock();
        let prior = std::env::var_os(OPENCODE_DB_ENV);
        std::env::set_var(OPENCODE_DB_ENV, value);
        Self { prior, _lock: lock }
    }

    /// Remove the variable for the guard's scope.
    pub(crate) fn unset() -> Self {
        let lock = lock();
        let prior = std::env::var_os(OPENCODE_DB_ENV);
        std::env::remove_var(OPENCODE_DB_ENV);
        Self { prior, _lock: lock }
    }
}

impl Drop for OpencodeDbEnv {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(OPENCODE_DB_ENV, value),
            None => std::env::remove_var(OPENCODE_DB_ENV),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lock_is_reentrant_on_one_thread() {
        let outer = lock();
        let inner = lock();
        drop(inner);
        drop(outer);
        // Fully released now: a fresh take must not self-deadlock.
        drop(lock());
    }

    #[test]
    fn the_lock_is_mutually_exclusive_across_threads() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let in_critical = Arc::new(AtomicBool::new(false));
        let raced = Arc::new(AtomicBool::new(false));
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let in_critical = Arc::clone(&in_critical);
            let raced = Arc::clone(&raced);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                let _guard = lock();
                // If another thread ever holds the lock simultaneously, this
                // swap observes it.
                if in_critical.swap(true, Ordering::SeqCst) {
                    raced.store(true, Ordering::SeqCst);
                }
                in_critical.store(false, Ordering::SeqCst);
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(!raced.load(Ordering::SeqCst), "two threads held the lock at once");
    }

    #[test]
    fn the_guard_restores_the_prior_value_after_a_panic_and_frees_the_lock() {
        let outer = tempfile::tempdir().unwrap();
        let inner = tempfile::tempdir().unwrap();
        let outer_path = outer.path().to_path_buf();
        let inner_path = inner.path().to_path_buf();
        // Run in a dedicated thread so the outer pin is the "prior" value and
        // the panic cannot be confused with this test's own outcome.
        let joined = std::thread::spawn(move || {
            let _outer = OpencodeDbEnv::set(&outer_path);
            let caught = std::panic::catch_unwind(|| {
                let _inner = OpencodeDbEnv::set(&inner_path);
                assert_eq!(std::env::var_os(OPENCODE_DB_ENV), Some(inner_path.into_os_string()));
                panic!("boom inside a pinned scope");
            });
            assert!(caught.is_err());
            std::env::var_os(OPENCODE_DB_ENV)
        })
        .join()
        .unwrap();
        assert_eq!(joined, Some(outer.path().as_os_str().to_owned()));
        // Every handle on that thread is gone, so the lock can be taken again.
        drop(lock());
    }

    #[test]
    fn the_lock_is_released_when_a_pinned_thread_panics_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let result = std::thread::spawn(move || {
            let _env = OpencodeDbEnv::set(&path);
            panic!("test body panics while holding the pin");
        })
        .join();
        assert!(result.is_err());
        // The poisoned lock is still acquirable, and the variable is not left
        // pointing at the dead test's tempdir.
        let _lock = lock();
        assert_ne!(std::env::var_os(OPENCODE_DB_ENV), Some(dir.path().as_os_str().to_owned()));
    }
}
