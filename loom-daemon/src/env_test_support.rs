//! Library-test fixture: one process-env guard shared by every suite that
//! needs to isolate itself from the ambient environment.
//!
//! The guard snapshots each named variable, clears or overrides it, and puts
//! the snapshot back on drop, so a `#[serial]` test cannot leak its settings
//! into the next one and a fleet host's exported overrides cannot decide the
//! suite. Storage is `OsString`, so non-UTF-8 values round-trip.
//!
//! The env is process-global: keep `#[serial]` (or the owning env lock) on
//! every test that holds one.
//!
//! Suites that pin a variable under a crate-wide lock of their own (rather
//! than a `serial_test` key) build it from [`define_reentrant_env_lock!`],
//! [`ReentrantEnvLock`] and [`EnvPin`], so the lock and restore machinery is
//! written once and each site contributes only its own lock state.

use std::ffi::{OsStr, OsString};
use std::marker::PhantomData;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Restores the variables it touched, in reverse order, when dropped.
pub(crate) struct EnvGuard(Vec<(&'static str, Option<OsString>)>);

impl EnvGuard {
    /// Snapshot `keys` without changing them.
    pub(crate) fn save(keys: &[&'static str]) -> Self {
        Self(keys.iter().map(|k| (*k, std::env::var_os(k))).collect())
    }

    /// Snapshot `keys`, then remove each from the environment.
    pub(crate) fn clear(keys: &[&'static str]) -> Self {
        let guard = Self::save(keys);
        for key in keys {
            std::env::remove_var(key);
        }
        guard
    }

    /// Snapshot `key`, then set it to `value`.
    pub(crate) fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let guard = Self::save(&[key]);
        std::env::set_var(key, value);
        guard
    }

    /// Snapshot `key`, then set (`Some`) or remove (`None`) it. Chainable, so
    /// one guard can own several overrides.
    pub(crate) fn and_set(mut self, key: &'static str, value: Option<&OsStr>) -> Self {
        self.0.push((key, std::env::var_os(key)));
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
        self
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..).rev() {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The lock state of one [`define_reentrant_env_lock!`] site: its own
/// crate-wide mutex, plus its own per-thread handle count and parked guard.
///
/// Nothing is shared between two sites, so locks for different variables
/// never exclude each other; nesting them is governed only by the acquisition
/// order their modules document.
pub(crate) trait EnvLockSite {
    /// The environment variable the site's [`EnvPin`] restores.
    const KEY: &'static str;

    /// The site's own crate-wide mutex.
    fn mutex() -> &'static Mutex<()>;

    /// Number of live [`ReentrantEnvLock`] handles on the calling thread.
    fn depth() -> usize;

    /// Store the calling thread's handle count.
    fn set_depth(depth: usize);

    /// Park the calling thread's mutex guard, so no single handle owns it.
    fn hold(guard: MutexGuard<'static, ()>);

    /// Take the calling thread's parked mutex guard back, if it has one.
    fn take_held() -> Option<MutexGuard<'static, ()>>;
}

/// Proof that the calling thread holds one site's crate-wide env lock.
/// `!Send`: it must be dropped on the thread that took it.
#[must_use = "the lock is released as soon as this is dropped"]
pub(crate) struct ReentrantEnvLock<S: EnvLockSite> {
    _not_send: PhantomData<*const ()>,
    _site: PhantomData<S>,
}

impl<S: EnvLockSite> ReentrantEnvLock<S> {
    /// Take the site's lock (re-entrant per thread).
    pub(crate) fn acquire() -> Self {
        if S::depth() == 0 {
            // A test that panicked while holding the lock poisons it; the env it
            // touched was still restored by its guard's `Drop`, so the poison
            // carries no information here.
            let guard = S::mutex().lock().unwrap_or_else(PoisonError::into_inner);
            S::hold(guard);
        }
        S::set_depth(S::depth() + 1);
        Self {
            _not_send: PhantomData,
            _site: PhantomData,
        }
    }
}

impl<S: EnvLockSite> Drop for ReentrantEnvLock<S> {
    fn drop(&mut self) {
        let remaining = S::depth().saturating_sub(1);
        S::set_depth(remaining);
        if remaining == 0 {
            drop(S::take_held());
        }
    }
}

/// RAII pin of one site's environment variable: holds the site's lock and
/// restores the variable's prior value (or its absence) on drop — including
/// on panic.
#[must_use = "the pin is undone as soon as this is dropped"]
pub(crate) struct EnvPin<S: EnvLockSite> {
    // Declared first so the variable is restored before `_lock` releases it.
    _prior: EnvGuard,
    _lock: ReentrantEnvLock<S>,
}

impl<S: EnvLockSite> EnvPin<S> {
    /// Point the variable at `value`.
    pub(crate) fn set(value: impl AsRef<OsStr>) -> Self {
        let lock = ReentrantEnvLock::acquire();
        Self {
            _prior: EnvGuard::set(S::KEY, value),
            _lock: lock,
        }
    }

    /// Remove the variable for the guard's scope.
    pub(crate) fn unset() -> Self {
        let lock = ReentrantEnvLock::acquire();
        Self {
            _prior: EnvGuard::clear(&[S::KEY]),
            _lock: lock,
        }
    }
}

/// Declare the lock state of one re-entrant env lock site: a `pub(crate)`
/// zero-sized `$site` implementing [`EnvLockSite`] for the variable `$key`,
/// owning its own `static LOCK` and `thread_local!` handle count and parked
/// guard. Crate-visible because the lock and pin types built on it are.
///
/// The site is what [`ReentrantEnvLock`] and [`EnvPin`] are parameterised on;
/// a module declares one and keeps its public surface beside it:
///
/// ```ignore
/// crate::define_reentrant_env_lock!(ProfileRootSite: CODEX_PROFILE_ROOT_ENV);
///
/// pub(crate) type ProfileRootLock = ReentrantEnvLock<ProfileRootSite>;
/// pub(crate) fn lock() -> ProfileRootLock { ProfileRootLock::acquire() }
/// pub(crate) type ProfileRootEnv = EnvPin<ProfileRootSite>;
/// ```
///
/// Exported rather than re-exported with `pub(crate) use`: `main.rs` compiles
/// this file into the binary's test build too, where only this module's own
/// tests invoke the macro, so a re-export would be an unused import there.
#[macro_export]
macro_rules! define_reentrant_env_lock {
    ($site:ident : $key:expr $(,)?) => {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        thread_local! {
            /// Number of live lock handles on this thread.
            static DEPTH: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
            /// The real mutex guard, held while `DEPTH > 0`. Kept here rather than in
            /// any one handle so handles may drop in any order on the owning thread.
            static HELD: std::cell::RefCell<Option<std::sync::MutexGuard<'static, ()>>> =
                const { std::cell::RefCell::new(None) };
        }

        /// This site's lock state; see [`EnvLockSite`].
        pub(crate) struct $site;

        impl EnvLockSite for $site {
            const KEY: &'static str = $key;

            fn mutex() -> &'static std::sync::Mutex<()> {
                &LOCK
            }

            fn depth() -> usize {
                DEPTH.with(std::cell::Cell::get)
            }

            fn set_depth(depth: usize) {
                DEPTH.with(|count| count.set(depth));
            }

            fn hold(guard: std::sync::MutexGuard<'static, ()>) {
                HELD.with(|held| *held.borrow_mut() = Some(guard));
            }

            fn take_held() -> Option<std::sync::MutexGuard<'static, ()>> {
                // The borrow ends with this call, before the caller drops the guard.
                HELD.with(|held| held.borrow_mut().take())
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    const A: &str = "LOOM_ENV_TEST_SUPPORT_A";
    const B: &str = "LOOM_ENV_TEST_SUPPORT_B";

    #[test]
    #[serial]
    fn clear_removes_and_drop_restores() {
        std::env::set_var(A, "ambient");
        std::env::remove_var(B);
        {
            let _g = EnvGuard::clear(&[A, B]);
            assert!(std::env::var_os(A).is_none());
            std::env::set_var(B, "leaked");
        }
        assert_eq!(std::env::var(A).as_deref(), Ok("ambient"));
        assert!(std::env::var_os(B).is_none());
        std::env::remove_var(A);
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn clear_restores_non_utf8_bytes() {
        use std::os::unix::ffi::OsStrExt;

        const KEY: &str = "LOOM_ENV_TEST_SUPPORT_NON_UTF8";
        const ORIGINAL: &[u8] = b"original-\xff-path";
        let _seed = EnvGuard::set(KEY, OsStr::from_bytes(ORIGINAL));
        {
            let _clear = EnvGuard::clear(&[KEY]);
            assert!(std::env::var_os(KEY).is_none());
        }
        assert_eq!(std::env::var_os(KEY).unwrap().as_bytes(), ORIGINAL);
    }

    #[test]
    #[serial]
    fn set_overrides_and_repeated_keys_restore_the_original() {
        std::env::set_var(A, "ambient");
        {
            let _g = EnvGuard::set(A, "one")
                .and_set(A, Some(OsStr::new("two")))
                .and_set(B, None);
            assert_eq!(std::env::var(A).as_deref(), Ok("two"));
        }
        assert_eq!(std::env::var(A).as_deref(), Ok("ambient"));
        std::env::remove_var(A);
    }

    /// A site of this module's own, on a variable no suite reads and behind a
    /// lock no other test takes, so the lock tests below need no `#[serial]`
    /// and never contend with the real sites.
    mod test_site {
        use crate::env_test_support::{EnvLockSite, EnvPin, ReentrantEnvLock};

        pub(super) const KEY: &str = "LOOM_ENV_TEST_SUPPORT_REENTRANT_LOCK";

        define_reentrant_env_lock!(Site: KEY);

        pub(super) type Lock = ReentrantEnvLock<Site>;
        pub(super) type Pin = EnvPin<Site>;
    }

    #[test]
    fn the_lock_is_reentrant_on_one_thread() {
        let outer = test_site::Lock::acquire();
        let inner = test_site::Lock::acquire();
        drop(inner);
        drop(outer);
        // Fully released now: a fresh take must not self-deadlock.
        drop(test_site::Lock::acquire());
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
                let _guard = test_site::Lock::acquire();
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
            let _outer = test_site::Pin::set(&outer_path);
            let caught = std::panic::catch_unwind(|| {
                let _inner = test_site::Pin::set(&inner_path);
                assert_eq!(std::env::var_os(test_site::KEY), Some(inner_path.into_os_string()));
                panic!("boom inside a pinned scope");
            });
            assert!(caught.is_err());
            std::env::var_os(test_site::KEY)
        })
        .join()
        .unwrap();
        assert_eq!(joined, Some(outer.path().as_os_str().to_owned()));
        // Every handle on that thread is gone, so the lock can be taken again.
        drop(test_site::Lock::acquire());
    }

    #[test]
    fn the_lock_is_released_when_a_pinned_thread_panics_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        let result = std::thread::spawn(move || {
            let _env = test_site::Pin::set(&path);
            panic!("test body panics while holding the pin");
        })
        .join();
        assert!(result.is_err());
        // The poisoned lock is still acquirable, and the variable is not left
        // pointing at the dead test's tempdir.
        let _lock = test_site::Lock::acquire();
        assert_ne!(std::env::var_os(test_site::KEY), Some(dir.path().as_os_str().to_owned()));
    }

    #[test]
    fn unset_removes_the_variable_for_its_scope_and_restores_it_after() {
        let dir = tempfile::tempdir().unwrap();
        // Nested in a live pin, like a helper that clears a variable a test set.
        let _outer = test_site::Pin::set(dir.path());
        {
            let _unset = test_site::Pin::unset();
            assert!(std::env::var_os(test_site::KEY).is_none());
        }
        assert_eq!(std::env::var_os(test_site::KEY), Some(dir.path().as_os_str().to_owned()));
    }
}
