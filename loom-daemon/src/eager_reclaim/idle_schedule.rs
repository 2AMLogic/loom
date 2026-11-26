//! The cross-root idle-target pass on both paths (#11192).
//!
//! [`crate::worktree_reaper::reclaim_idle_targets_below_floor`] (#11071) is
//! the one pass that reaches kept worktrees' build caches in **every**
//! registered root. Until #11192 only the eager pass called it, so when the
//! eager trigger stayed disarmed for an afternoon, nothing reclaimed them.
//! The scheduled worktree reaper now runs it too when its root is below the
//! floor ([`scheduled_pressure_tier`]).
//!
//! Both callers go through one host-wide window ([`run_idle_targets_if_due`]):
//! the pass walks every registered root whichever root asked, so running it
//! once per window from whichever path gets there first is enough, and a
//! multi-root reaper tick does not walk the whole host once per root.

use std::path::Path;

use chrono::{DateTime, Utc};

use crate::host_wide_cooldown::{active_since_secs, HostWideCooldown};
use crate::worktree_reaper::ReclaimReport;

static IDLE_COOLDOWN: HostWideCooldown = HostWideCooldown::new();

/// Whether the cross-root pass is due: never run, or `min_interval_secs` or
/// more since it last ran (a clock step backwards counts as due). Pure.
#[must_use]
pub fn idle_pass_due(
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: u64,
) -> bool {
    active_since_secs(last, now, min_interval_secs).is_none()
}

/// Run `idle(trigger_root, floor_gb)` if the host-wide window has elapsed,
/// claiming the window **before** running so a concurrent caller on the
/// other path skips instead of walking the same roots in parallel. `None`
/// when not due.
pub fn run_idle_targets_if_due(
    trigger_root: &Path,
    floor_gb: u64,
    now: DateTime<Utc>,
    min_interval_secs: u64,
    idle: &dyn Fn(&Path, u64) -> ReclaimReport,
) -> Option<ReclaimReport> {
    if !IDLE_COOLDOWN.claim_if_elapsed(now, min_interval_secs) {
        return None;
    }
    Some(idle(trigger_root, floor_gb))
}

/// The scheduled reaper's half, with its probes injected: when `root`'s
/// volume reads below the floor, run the cross-root pass if the shared
/// window has elapsed. `None` when not below the floor (or unmeasurable:
/// never delete for a need nobody can see) or not due.
pub fn scheduled_idle_pass_with(
    root: &Path,
    floor_gb: u64,
    now: DateTime<Utc>,
    min_interval_secs: u64,
    free_gb: &dyn Fn(&Path) -> Option<u64>,
    idle: &dyn Fn(&Path, u64) -> ReclaimReport,
) -> Option<ReclaimReport> {
    let free = free_gb(root)?;
    if free >= floor_gb {
        return None;
    }
    run_idle_targets_if_due(root, floor_gb, now, min_interval_secs, idle)
}

/// The scheduled reaper's pressure tier for `repo_root`: the cross-root
/// idle-target pass when below the floor (#11192), then the primary
/// checkout's deep clean (#5919), in the same tier order the eager pass uses.
/// Blocking.
pub fn scheduled_pressure_tier(repo_root: &Path, floor_gb: u64) {
    let min_interval_secs =
        super::resolve_min_interval_secs(&super::read_eager_reclaim_config(repo_root));
    if let Some(report) = scheduled_idle_pass_with(
        repo_root,
        floor_gb,
        Utc::now(),
        min_interval_secs,
        &crate::disk_headroom::worktree_root_free_gb,
        &crate::worktree_reaper::reclaim_idle_targets_below_floor,
    ) {
        log::warn!(
            "worktree_reaper: {} below the {floor_gb}G floor — ran the cross-root idle \
             worktree target pass on the scheduled path: {} dir(s) ({}) (#11192)",
            repo_root.display(),
            report.removed.len(),
            crate::tmpfs_reclaim::human_size(report.bytes_freed())
        );
        crate::worktree_reaper::log_idle_target_report(repo_root, &report);
    }
    crate::deep_clean::run_for(repo_root, floor_gb);
}

/// Forget when the cross-root pass last ran. Test-only seam.
#[doc(hidden)]
pub fn reset_idle_state_for_test() {
    IDLE_COOLDOWN.reset();
}
