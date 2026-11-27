//! Test-only, crate-wide isolation for `LOOM_CODEX_HOME` (issue #15174).
//!
//! The variable is process-global, and every `#[test]` in this crate links
//! into one multi-threaded binary. Tests used to mutate it through two
//! disjoint groups that do not exclude each other: runtime-isolation helpers
//! and inline clear-lists that remove (then restore) `"LOOM_CODEX_HOME"` as
//! one entry in a ~20-variable list while carrying whatever `serial_test`
//! key they happened to carry (often none), and `#[serial(codex_home_env)]`
//! pinners that set the variable directly. Distinct keys — and a key versus
//! no key — do not exclude each other, so a helper could remove the variable
//! between a pinner's `set_var` and its read.
//!
//! [`lock`] closes that class the way `profile_root_env` (#9964) closed the
//! identical defect for `LOOM_CODEX_PROFILE_ROOT`: one crate-wide lock that
//! **every** test mutating the variable holds for its whole scope, whatever
//! `serial_test` key it also carries. Re-entrant per thread, so a test
//! holding a multi-variable env guard can still call a helper that takes its
//! own lock, and a nested helper does not self-deadlock. [`CodexHomeEnv`] is
//! the RAII form: it holds the lock and restores the prior value on drop,
//! including during a panic unwind.
//!
//! # Lock order
//!
//! `serial_test` named keys outermost, then the crate-default key (#14404),
//! then this mutex, then — when both env locks are held — the
//! [`profile_root_env`] mutex innermost: acquire `codex_home_env` *before*
//! `profile_root_env`, release it after. Every helper below follows that
//! chain, so it can never form a cycle with either lock family.

use std::cell::{Cell, RefCell};
use std::ffi::{OsStr, OsString};
use std::sync::{Mutex, MutexGuard, PoisonError};

use crate::codex_usage::CODEX_HOME_ENV;

static LOCK: Mutex<()> = Mutex::new(());

thread_local! {
    /// Number of live [`CodexHomeLock`]s on this thread.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
    /// The real mutex guard, held while `DEPTH > 0`. Kept here rather than in
    /// any one handle so handles may drop in any order on the owning thread.
    static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
}

/// Proof that the current thread holds the crate-wide codex-home lock.
/// `!Send`: it must be dropped on the thread that took it.
#[must_use = "the lock is released as soon as this is dropped"]
pub(crate) struct CodexHomeLock {
    _not_send: std::marker::PhantomData<*const ()>,
}

/// Take the crate-wide `LOOM_CODEX_HOME` lock (re-entrant per thread).
///
/// Multi-variable test env guards that save/clear/restore this variable among
/// others hold one of these for their lifetime, and so does every test that
/// pins the variable directly.
pub(crate) fn lock() -> CodexHomeLock {
    if DEPTH.with(Cell::get) == 0 {
        // A test that panicked while holding the lock poisons it; the env it
        // touched was still restored by its guard's `Drop`, so the poison
        // carries no information here.
        let guard = LOCK.lock().unwrap_or_else(PoisonError::into_inner);
        HELD.with(|held| *held.borrow_mut() = Some(guard));
    }
    DEPTH.with(|depth| depth.set(depth.get() + 1));
    CodexHomeLock {
        _not_send: std::marker::PhantomData,
    }
}

impl Drop for CodexHomeLock {
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

/// RAII pin of `LOOM_CODEX_HOME`: holds [`lock`] and restores the variable's
/// prior value (or its absence) on drop — including on panic.
#[must_use = "the pin is undone as soon as this is dropped"]
pub(crate) struct CodexHomeEnv {
    prior: Option<OsString>,
    // Declared last so it drops after `Drop::drop` restored the variable.
    _lock: CodexHomeLock,
}

impl CodexHomeEnv {
    /// Point the variable at `value` (a tempdir home, typically).
    pub(crate) fn set(value: impl AsRef<OsStr>) -> Self {
        let lock = lock();
        let prior = std::env::var_os(CODEX_HOME_ENV);
        std::env::set_var(CODEX_HOME_ENV, value);
        Self { prior, _lock: lock }
    }

    /// Remove the variable for the guard's scope.
    pub(crate) fn unset() -> Self {
        let lock = lock();
        let prior = std::env::var_os(CODEX_HOME_ENV);
        std::env::remove_var(CODEX_HOME_ENV);
        Self { prior, _lock: lock }
    }
}

impl Drop for CodexHomeEnv {
    fn drop(&mut self) {
        match self.prior.take() {
            Some(value) => std::env::set_var(CODEX_HOME_ENV, value),
            None => std::env::remove_var(CODEX_HOME_ENV),
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
            let _outer = CodexHomeEnv::set(&outer_path);
            let caught = std::panic::catch_unwind(|| {
                let _inner = CodexHomeEnv::set(&inner_path);
                assert_eq!(std::env::var_os(CODEX_HOME_ENV), Some(inner_path.into_os_string()));
                panic!("boom inside a pinned scope");
            });
            assert!(caught.is_err());
            std::env::var_os(CODEX_HOME_ENV)
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
            let _env = CodexHomeEnv::set(&path);
            panic!("test body panics while holding the pin");
        })
        .join();
        assert!(result.is_err());
        // The poisoned lock is still acquirable, and the variable is not left
        // pointing at the dead test's tempdir.
        let _lock = lock();
        assert_ne!(std::env::var_os(CODEX_HOME_ENV), Some(dir.path().as_os_str().to_owned()));
    }
}
