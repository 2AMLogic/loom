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
//! Both are instances of the shared machinery in `env_test_support`
//! (`define_reentrant_env_lock!`); what lives here is this variable's own lock
//! state and surface.
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

use crate::env_test_support::{EnvLockSite, EnvPin, ReentrantEnvLock};
use crate::opencode_usage::OPENCODE_DB_ENV;

crate::define_reentrant_env_lock!(OpencodeDbSite: OPENCODE_DB_ENV);

/// Proof that the current thread holds the crate-wide opencode-db lock.
/// `!Send`: it must be dropped on the thread that took it.
pub(crate) type OpencodeDbLock = ReentrantEnvLock<OpencodeDbSite>;

/// Take the crate-wide `LOOM_OPENCODE_DB` lock (re-entrant per thread).
///
/// Multi-variable test env guards that save/clear/restore this variable among
/// others hold one of these for their lifetime, and so does every test that
/// pins the variable directly.
pub(crate) fn lock() -> OpencodeDbLock {
    OpencodeDbLock::acquire()
}

/// RAII pin of `LOOM_OPENCODE_DB`: holds [`lock`] and restores the variable's
/// prior value (or its absence) on drop — including on panic. `set` takes a
/// tempdir `opencode.db`, typically.
pub(crate) type OpencodeDbEnv = EnvPin<OpencodeDbSite>;
