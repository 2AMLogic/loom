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
//! Both are instances of the shared machinery in `env_test_support`
//! (`define_reentrant_env_lock!`); what lives here is this variable's own lock
//! state and surface.
//!
//! # Lock order
//!
//! `serial_test` named keys outermost, then the crate-default key (#14404),
//! then this mutex, then — when both env locks are held — the
//! [`profile_root_env`] mutex innermost: acquire `codex_home_env` *before*
//! `profile_root_env`, release it after. Every helper below follows that
//! chain, so it can never form a cycle with either lock family.

use crate::codex_usage::CODEX_HOME_ENV;
use crate::env_test_support::{EnvLockSite, EnvPin, ReentrantEnvLock};

crate::define_reentrant_env_lock!(CodexHomeSite: CODEX_HOME_ENV);

/// Proof that the current thread holds the crate-wide codex-home lock.
/// `!Send`: it must be dropped on the thread that took it.
pub(crate) type CodexHomeLock = ReentrantEnvLock<CodexHomeSite>;

/// Take the crate-wide `LOOM_CODEX_HOME` lock (re-entrant per thread).
///
/// Multi-variable test env guards that save/clear/restore this variable among
/// others hold one of these for their lifetime, and so does every test that
/// pins the variable directly.
pub(crate) fn lock() -> CodexHomeLock {
    CodexHomeLock::acquire()
}

/// RAII pin of `LOOM_CODEX_HOME`: holds [`lock`] and restores the variable's
/// prior value (or its absence) on drop — including on panic. `set` takes a
/// tempdir home, typically.
pub(crate) type CodexHomeEnv = EnvPin<CodexHomeSite>;
