//! Test-only, crate-wide isolation for `LOOM_CODEX_PROFILE_ROOT` (issue #9964).
//!
//! The variable is process-global, and every `#[test]` in this crate links into
//! one multi-threaded binary. Tests used to redirect it with bare
//! `set_var`/`remove_var` pairs serialized by a patchwork of *different*
//! `serial_test` keys (default, `codex_home_env`, `codex_profile_root`,
//! `loom_shared_tokens_dir_env`) plus some with no key at all. Distinct keys do
//! not exclude each other, so one test could unset the variable between another
//! test's `set_var` and its write — and the write then fell back to the real
//! `~/.loom/codex-profiles` (a fixture `alice` profile leaked onto an operator
//! Mac). A panic before a trailing `remove_var` leaked the redirect instead.
//!
//! Two pieces close that class:
//!
//! * [`lock`] — one crate-wide lock that **every** test mutating the variable
//!   holds for its whole scope, whatever `serial_test` key it also carries.
//!   Re-entrant per thread, so a test holding a multi-variable env guard can
//!   still call a helper that takes its own [`ProfileRootEnv`].
//! * [`ProfileRootEnv`] — an RAII redirect that holds the lock and restores the
//!   prior value on drop, including during a panic unwind.
//!
//! Both are instances of the shared machinery in `env_test_support`
//! (`define_reentrant_env_lock!`); what lives here is this variable's own lock
//! state and surface.
//!
//! `paths::codex_profile_root()` panics under `cfg(test)` when the variable is
//! unset, so a test that forgets the redirect fails loudly instead of falling
//! back to the operator's home.

use super::paths::CODEX_PROFILE_ROOT_ENV;
use crate::env_test_support::{EnvLockSite, EnvPin, ReentrantEnvLock};

crate::define_reentrant_env_lock!(ProfileRootSite: CODEX_PROFILE_ROOT_ENV);

/// Proof that the current thread holds the crate-wide profile-root lock.
/// `!Send`: it must be dropped on the thread that took it.
pub(crate) type ProfileRootLock = ReentrantEnvLock<ProfileRootSite>;

/// Take the crate-wide `LOOM_CODEX_PROFILE_ROOT` lock (re-entrant per thread).
///
/// Multi-variable test env guards that save/clear/restore this variable among
/// others hold one of these for their lifetime.
pub(crate) fn lock() -> ProfileRootLock {
    ProfileRootLock::acquire()
}

/// RAII redirect of `LOOM_CODEX_PROFILE_ROOT`: holds [`lock`] and restores the
/// variable's prior value (or its absence) on drop — including on panic.
///
/// `set` takes a tempdir, or `""` to disable the root. `unset` is only for
/// tests that assert the unset behavior itself — `codex_profile_root()` panics
/// while unset.
pub(crate) type ProfileRootEnv = EnvPin<ProfileRootSite>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens_pool::paths::codex_profile_root;

    #[test]
    fn codex_profile_root_panics_instead_of_resolving_the_real_home_under_test() {
        let _env = ProfileRootEnv::unset();
        let result = std::panic::catch_unwind(codex_profile_root);
        assert!(result.is_err(), "an unset root must panic under cfg(test), never fall back");
    }

    #[test]
    fn an_explicit_root_and_an_empty_override_still_resolve() {
        let dir = tempfile::tempdir().unwrap();
        {
            let _env = ProfileRootEnv::set(dir.path());
            assert_eq!(codex_profile_root().as_deref(), Some(dir.path()));
        }
        let _env = ProfileRootEnv::set("");
        assert_eq!(codex_profile_root(), None);
    }
}
