//! Cooldown windows for cleanup passes. Each pass owns a separate instance.
//! Host-wide passes share one timestamp across roots. Repository-scoped
//! passes track a timestamp for each root they visit.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use chrono::{DateTime, Utc};

/// Seconds since the last evaluation while a pass is in its cooldown.
/// First runs, backwards clock steps and the exact boundary are eligible.
pub(crate) fn active_since_secs(
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: u64,
) -> Option<i64> {
    let since = (now - last?).num_seconds();
    (0..i64::try_from(min_interval_secs).unwrap_or(i64::MAX))
        .contains(&since)
        .then_some(since)
}

fn lock<T>(state: &Mutex<T>) -> MutexGuard<'_, T> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One timestamp for a pass that walks host-wide resources.
pub(crate) struct HostWideCooldown {
    last_evaluated_at: Mutex<Option<DateTime<Utc>>>,
}

impl HostWideCooldown {
    pub(crate) const fn new() -> Self {
        Self {
            last_evaluated_at: Mutex::new(None),
        }
    }

    #[must_use]
    pub(crate) fn elapsed(&self, now: DateTime<Utc>, min_interval_secs: u64) -> bool {
        active_since_secs(*lock(&self.last_evaluated_at), now, min_interval_secs).is_none()
    }

    /// Check and record under the same lock so concurrent callers cannot both start a pass.
    #[must_use]
    pub(crate) fn claim_if_elapsed(&self, now: DateTime<Utc>, min_interval_secs: u64) -> bool {
        let mut last = lock(&self.last_evaluated_at);
        if active_since_secs(*last, now, min_interval_secs).is_some() {
            return false;
        }
        *last = Some(now);
        true
    }

    pub(crate) fn record(&self, now: DateTime<Utc>) {
        *lock(&self.last_evaluated_at) = Some(now);
    }

    pub(crate) fn reset(&self) {
        *lock(&self.last_evaluated_at) = None;
    }
}

/// Independent cooldown windows for one pass that walks individual repository roots.
pub(crate) struct PerRepoCooldown {
    last_run: Mutex<BTreeMap<PathBuf, DateTime<Utc>>>,
}

impl PerRepoCooldown {
    pub(crate) const fn new() -> Self {
        Self {
            last_run: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn active_since_secs(
        &self,
        repo_root: &Path,
        now: DateTime<Utc>,
        min_interval_secs: u64,
    ) -> Option<i64> {
        let last = lock(&self.last_run).get(repo_root).copied();
        active_since_secs(last, now, min_interval_secs)
    }

    pub(crate) fn deferred_reason(
        &self,
        repo_root: &Path,
        now: DateTime<Utc>,
        min_interval_secs: u64,
    ) -> Option<String> {
        self.active_since_secs(repo_root, now, min_interval_secs)
            .map(|since| format!("a pass ran {since}s ago (cooldown {min_interval_secs}s)"))
    }

    pub(crate) fn record(&self, repo_root: &Path, now: DateTime<Utc>) {
        lock(&self.last_run).insert(repo_root.to_path_buf(), now);
    }

    pub(crate) fn reset(&self) {
        lock(&self.last_run).clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t(secs: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_800_000_000 + secs, 0).unwrap()
    }

    #[test]
    fn elapsed_is_true_before_any_evaluation() {
        let cooldown = HostWideCooldown::new();
        assert!(cooldown.elapsed(t(0), 1_800));
    }

    #[test]
    fn a_recent_evaluation_holds_the_cooldown() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        assert!(!cooldown.elapsed(t(60), 1_800));
    }

    #[test]
    fn the_exact_interval_boundary_is_eligible() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        assert!(cooldown.elapsed(t(1_800), 1_800));
    }

    #[test]
    fn the_cooldown_expires() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        assert!(cooldown.elapsed(t(1_801), 1_800));
    }

    #[test]
    fn a_backwards_clock_is_eligible() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(600));
        assert!(cooldown.elapsed(t(0), 1_800));
    }

    #[test]
    fn recording_one_instance_leaves_another_immediately_eligible() {
        let one = HostWideCooldown::new();
        let other = HostWideCooldown::new();
        one.record(t(0));
        assert!(!one.elapsed(t(60), 1_800));
        assert!(other.elapsed(t(60), 1_800));
    }

    #[test]
    fn reset_restores_first_evaluation() {
        let cooldown = HostWideCooldown::new();
        cooldown.record(t(0));
        cooldown.reset();
        assert!(cooldown.elapsed(t(0), 1_800));
    }

    #[test]
    fn per_repo_window_reports_seconds_only_while_active() {
        let pass = PerRepoCooldown::new();
        let repo = Path::new("repo");
        assert_eq!(pass.active_since_secs(repo, t(0), 60), None);
        pass.record(repo, t(0));
        assert_eq!(pass.active_since_secs(repo, t(0), 60), Some(0));
        assert_eq!(pass.active_since_secs(repo, t(59), 60), Some(59));
        for now in [t(60), t(61), t(-1)] {
            assert_eq!(pass.active_since_secs(repo, now, 60), None);
        }
        assert_eq!(pass.active_since_secs(repo, t(0), 0), None);
    }

    #[test]
    fn per_repo_windows_are_isolated_by_repo_and_pass() {
        let pass = PerRepoCooldown::new();
        let other = PerRepoCooldown::new();
        let a = Path::new("repo-a");
        let b = Path::new("repo-b");
        pass.record(a, t(0));
        assert_eq!(pass.active_since_secs(b, t(0), 60), None);
        assert_eq!(other.active_since_secs(a, t(0), 60), None);
        pass.record(b, t(20));
        other.record(a, t(30));
        pass.reset();
        assert_eq!(pass.active_since_secs(a, t(40), 60), None);
        assert_eq!(pass.active_since_secs(b, t(40), 60), None);
        assert_eq!(other.active_since_secs(a, t(40), 60), Some(10));
    }

    #[test]
    fn concurrent_claims_admit_one_caller_per_window() {
        let cooldown = HostWideCooldown::new();
        let start = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let claim = || {
                start.wait();
                cooldown.claim_if_elapsed(t(0), 60)
            };
            let peer = scope.spawn(claim);
            assert_ne!(claim(), peer.join().unwrap());
        });
        assert!(!cooldown.claim_if_elapsed(t(59), 60));
        assert!(cooldown.claim_if_elapsed(t(60), 60));
        assert!(cooldown.claim_if_elapsed(t(0), 60));
    }
}
