//! The merged-PR reporter task (#13946).
//!
//! Two producers feed the one report in [`super::merge_report`]:
//!
//! - **The host that ran the merge posts.** This daemon subscribes to the event
//!   bus; a sweep that exits ([`Event::SweepExited`]) or a Champion merge step
//!   that ends with a verdict ([`Event::StepEnded`]) is only a *hint*. The merge
//!   itself is confirmed from the forge ([`super::merge_source`]), and only then
//!   is the line posted. An open PR, or a PR that merged long ago, posts
//!   nothing.
//! - **The fleet captain posts the merges no host reported.** On a timer, and
//!   only where [`crate::fleet_captain`] arms this host, the captain scans each
//!   workspace for PRs merged inside a **bounded catch-up window**, reads the
//!   room once, and posts exactly the merges whose line is absent. It leaves
//!   merges younger than a short grace period to the merging host, so the two
//!   normally never race. A merge made by a hosted/manual action, or by a host
//!   that died before posting, is caught here.
//!
//! The scan is stateless apart from an in-memory per-workspace watermark that is
//! advanced **only** after a fully answered, fully read, fully posted pass. A
//! failed forge read, a failed room read, a failed post or a page the forge
//! filled leaves the window as it was, so the unseen range is retried on the next
//! tick (and is bounded below by the catch-up window). The room read-back is what
//! keeps a retried scan from posting a line twice.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::merge_report::{self, Outcome, PrReport, ReportTarget};
use super::merge_source::{self, MergedPr};
use super::{forge_gate, resolve_socket, set_state, SafehouseConfig, SafehouseState};
use crate::event_bus::{EventBus, RecvError, Subscription};
use crate::observability::ops::feed::{self, PassOutcome, Trigger};
use crate::types::Event;

/// Captain scan cadence, in milliseconds. Test/operator override.
const SCAN_INTERVAL_ENV: &str = "LOOM_SAFEHOUSE_RECONCILE_INTERVAL_MS";
const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// How far back the captain will report a merge nobody reported, in seconds.
/// Bounds both staleness (a fresh install must not backfill old history into the
/// room) and cost. Also the freshness bound on a host's own report: a merge
/// older than this is not the one the exiting sweep just made.
const CATCHUP_ENV: &str = "LOOM_SAFEHOUSE_REPORT_CATCHUP_SECS";
const DEFAULT_CATCHUP: Duration = Duration::from_secs(6 * 60 * 60);

/// How long the captain leaves a fresh merge to the host that made it, in
/// seconds.
const GRACE_ENV: &str = "LOOM_SAFEHOUSE_REPORT_GRACE_SECS";
const DEFAULT_GRACE: Duration = Duration::from_secs(10 * 60);

/// A scan starts this far before its watermark, because the forge's search index
/// trails a merge by seconds to minutes.
const WATERMARK_SLACK: chrono::Duration = chrono::Duration::minutes(30);

/// Most pages one workspace scan will walk back through when the forge keeps
/// returning full pages.
const MAX_PAGES: u32 = 5;

/// Cap on the in-process set of reported keys; cleared when exceeded (a
/// duplicate after that is tolerated).
const REPORTED_CAP: usize = 10_000;

fn env_secs(name: &str, default: Duration) -> Duration {
    super::env_nonempty(name)
        .and_then(|raw| raw.parse::<u64>().ok())
        .map_or(default, Duration::from_secs)
}

fn scan_interval() -> Duration {
    super::env_nonempty(SCAN_INTERVAL_ENV)
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_SCAN_INTERVAL, Duration::from_millis)
}

/// Whether a step that ended with this ledger classification may have merged its
/// PR, so the reporter should look. Only a Champion merge step merges: a Builder
/// opens a PR (an open PR is unfinished work, never a report), and Judge,
/// Doctor, Curator and promotion move labels. Of the merge step's outcomes only
/// a `verdict` can be a merge, and its non-merge verdicts are ruled out without
/// a forge call; the forge's `mergedAt`, not the reason, decides the rest. Pure.
#[must_use]
pub(crate) fn step_may_have_merged(
    role: crate::step_role::StepRole,
    class: &str,
    reason: &str,
) -> bool {
    role == crate::step_role::StepRole::ChampionMerge
        && class == "verdict"
        && !matches!(reason, "rejected" | "triaged" | "triage_blocked" | "triage_operator")
}

/// Start the merged-PR reporter on `runtime` when enabled. Returns the task
/// handle, or `None` (a byte-for-byte no-op: no bus subscription, no socket)
/// when disabled or when no socket path can be resolved. `state` is set to the
/// resolved config-only state immediately and updated by each report's connect.
///
/// `workspace_root` is the daemon's default workspace: where the captain gate
/// reads `fleet.captain` and one of the workspaces the captain scans.
#[must_use]
pub fn spawn_merge_reporter(
    config: &SafehouseConfig,
    bus: &EventBus,
    runtime: &tokio::runtime::Handle,
    state: super::SharedSafehouseState,
    workspace_root: PathBuf,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        // Disabled ⇒ do not even subscribe. No syscalls, no behavior change.
        set_state(&state, SafehouseState::NotConfigured);
        return None;
    }
    let Some(socket) = resolve_socket(config) else {
        log::warn!(
            "safehouse: enabled but no socket path resolved \
             (set safehouse.socket, $LOOM_SAFEHOUSE_SOCKET, or $SAFEHOUSED_SOCKET) — merged-PR report off"
        );
        set_state(&state, SafehouseState::NotConfigured);
        return None;
    };
    log::info!(
        "safehouse: merged-PR report enabled (persona={}, socket={})",
        config.persona,
        socket.display()
    );
    // Configured, not yet connected: each report connects on demand.
    set_state(
        &state,
        SafehouseState::Unreachable {
            socket: socket.clone(),
        },
    );
    let target = ReportTarget::new(config, socket, state);
    let reporter = Arc::new(Reporter::new(target, workspace_root));
    let subscription = bus.subscribe(Vec::<String>::new());
    let handle = runtime.clone();
    Some(runtime.spawn(run(reporter, subscription, scan_interval(), handle)))
}

/// The reporter's shared state. One per daemon, behind an `Arc` so every spawned
/// report/scan task reaches it.
pub(super) struct Reporter {
    target: ReportTarget,
    /// The daemon's default workspace (captain gate root + a scan target).
    default_root: PathBuf,
    /// `owner/repo#N` keys already handled by this process (posted, found in the
    /// room, or in flight), so a sweep exit and a merge-step end for one PR post
    /// once.
    reported: Mutex<HashSet<String>>,
    /// Workspaces seen on live events, scanned in addition to the registry.
    known_workspaces: Mutex<BTreeSet<String>>,
    /// Per-workspace scan watermarks (see the module docs).
    watermarks: Mutex<HashMap<String, DateTime<Utc>>>,
    /// A scan is running; the next tick skips instead of overlapping it.
    scan_in_flight: AtomicBool,
    catchup: chrono::Duration,
    grace: chrono::Duration,
}

impl Reporter {
    pub(super) fn new(target: ReportTarget, default_root: PathBuf) -> Self {
        let secs =
            |d: Duration| chrono::Duration::seconds(i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
        Self {
            target,
            default_root,
            reported: Mutex::new(HashSet::new()),
            known_workspaces: Mutex::new(BTreeSet::new()),
            watermarks: Mutex::new(HashMap::new()),
            scan_in_flight: AtomicBool::new(false),
            catchup: secs(env_secs(CATCHUP_ENV, DEFAULT_CATCHUP)),
            grace: secs(env_secs(GRACE_ENV, DEFAULT_GRACE)),
        }
    }

    /// Windows for tests, which must not depend on the process environment.
    #[cfg(test)]
    pub(super) fn with_windows(
        mut self,
        catchup: chrono::Duration,
        grace: chrono::Duration,
    ) -> Self {
        self.catchup = catchup;
        self.grace = grace;
        self
    }

    /// A workspace's scan watermark, if a complete pass has set one.
    #[cfg(test)]
    pub(super) fn watermark(&self, root: &Path) -> Option<DateTime<Utc>> {
        self.watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(root.to_string_lossy().as_ref())
            .copied()
    }

    /// Mark `key` handled by this process; `false` when it already was.
    fn claim(&self, key: &str) -> bool {
        let mut reported = self.reported.lock().unwrap_or_else(PoisonError::into_inner);
        if reported.len() >= REPORTED_CAP {
            reported.clear();
        }
        reported.insert(key.to_owned())
    }

    /// Hand `key` back so a later pass may try it again.
    fn release(&self, key: &str) {
        self.reported
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key);
    }

    fn remember_workspace(&self, root: &str) {
        self.known_workspaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(root.to_owned());
    }

    /// The workspaces the captain scans: the default workspace, every one seen
    /// on a live event, and every one in the on-disk registry (which survives a
    /// restart, unlike the live set).
    fn scan_targets(&self) -> Vec<String> {
        let mut targets: BTreeSet<String> = self
            .known_workspaces
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        targets.insert(self.default_root.to_string_lossy().into_owned());
        if let Ok(registry) = crate::workspace_registry::WorkspaceRegistry::load_default() {
            for root in registry.roots() {
                targets.insert(root.to_string_lossy().into_owned());
            }
        }
        targets.into_iter().collect()
    }

    /// The host path: post the line for a merge this host just saw confirmed.
    /// Sends directly (no read first): the merging host is the normal poster.
    /// Only the post's own retry reads the room.
    pub(super) async fn report_host_merge(&self, pr: &MergedPr, trigger: Trigger) {
        if pr.merged_at < Utc::now() - self.catchup {
            log::debug!(
                "safehouse: PR #{} merged at {} is outside the report window; not the merge this exit just made",
                pr.number,
                pr.merged_at
            );
            return;
        }
        let Some(report) = PrReport::from_merged(pr) else {
            log::debug!("safehouse: PR #{} has no parseable repo URL; not reported", pr.number);
            return;
        };
        let key = report.key();
        if !self.claim(&key) {
            return;
        }
        match merge_report::post(&self.target, &report).await {
            Outcome::Posted => {
                feed::record_completions(trigger, false, 1);
                log::info!("safehouse: reported merged PR {key}");
            }
            Outcome::AlreadyReported => {
                log::debug!("safehouse: {key} was already in the room; not re-sent");
            }
            Outcome::Failed(why) => {
                // Hand the key back: the captain's catch-up scan (or a later
                // event for the same PR) may still deliver it.
                self.release(&key);
                log::warn!("safehouse: could not report merged PR {key}: {why}; continuing");
            }
        }
    }

    /// One captain catch-up pass over every scan target. A no-op unless this
    /// host is the armed fleet captain. Returns the number of lines posted.
    pub(super) async fn captain_pass(&self) -> usize {
        let host = crate::sweep_registry::host_identity();
        let gate = crate::fleet_captain::resolve_gate_for_root(&self.default_root, &host);
        if !gate.is_armed() {
            log::trace!("safehouse: {}", gate.message("safehouse merged-PR catch-up"));
            return 0;
        }
        let mut bodies = match merge_report::read_room(&self.target).await {
            Ok(bodies) => bodies,
            Err(why) => {
                log::warn!(
                    "safehouse: captain catch-up could not read the fleet room ({why}); \
                     the window stays open for the next pass"
                );
                feed::record_reconcile_pass(PassOutcome::Failed);
                return 0;
            }
        };
        let mut posted = 0;
        for root in self.scan_targets() {
            posted += self.scan_workspace(Path::new(&root), &mut bodies).await;
        }
        posted
    }

    /// Scan one workspace and post the merges the room lacks. Advances the
    /// workspace's watermark only when the pass was complete (see module docs).
    async fn scan_workspace(&self, root: &Path, bodies: &mut Vec<String>) -> usize {
        let root_key = root.to_string_lossy().into_owned();
        if forge_gate::suppressed("pr list (scan)", root) {
            feed::record_reconcile_pass(PassOutcome::Deferred);
            return 0;
        }
        let now = Utc::now();
        let horizon = now - self.grace;
        let since = {
            let marks = self
                .watermarks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            marks
                .get(&root_key)
                .copied()
                .map_or(now - self.catchup, |mark| mark.max(now - self.catchup))
        };

        let mut rows: Vec<MergedPr> = Vec::new();
        let mut complete = true;
        let mut until: Option<DateTime<Utc>> = None;
        for page_no in 1..=MAX_PAGES {
            let Some(page) = merge_source::merged_prs_between(root, since, until).await else {
                complete = false;
                break;
            };
            let oldest = page.rows.iter().map(|pr| pr.merged_at).min();
            rows.extend(page.rows);
            if !page.saturated {
                break;
            }
            // A full page: older merges in the window may be hidden. Walk the
            // window back to the oldest row seen. Out of pages, or no progress
            // possible, means the scan is incomplete and must be retried.
            match oldest {
                Some(oldest) if oldest > since && Some(oldest) != until && page_no < MAX_PAGES => {
                    until = Some(oldest);
                }
                _ => {
                    complete = false;
                    log::warn!(
                        "safehouse: catch-up scan of {} filled its pages; merges older than \
                         the oldest row read are not reported until the next pass",
                        root.display()
                    );
                    break;
                }
            }
        }
        // Oldest first, one row per PR (overlapping pages repeat a boundary row),
        // and only merges old enough that the merging host has had its chance.
        // The URL carries `owner/repo/pull/N`, i.e. the report's whole identity.
        rows.sort_by(|a, b| (a.merged_at, a.number, &a.url).cmp(&(b.merged_at, b.number, &b.url)));
        rows.dedup_by(|a, b| a.url == b.url);
        let mut posted = 0;
        let mut failures = Vec::new();
        for pr in rows.iter().filter(|pr| pr.merged_at <= horizon) {
            let Some(report) = PrReport::from_merged(pr) else {
                continue;
            };
            let key = report.key();
            if report.is_reported_in(bodies) {
                self.claim(&key);
                continue;
            }
            // The room read above is the authority; the in-process set also
            // skips a PR whose host-path post settled after that read.
            if !self.claim(&key) {
                continue;
            }
            match merge_report::post(&self.target, &report).await {
                Outcome::Posted => {
                    bodies.push(report.body());
                    posted += 1;
                    log::info!("safehouse: captain reported merged PR {key} that no host reported");
                }
                Outcome::AlreadyReported => bodies.push(report.body()),
                Outcome::Failed(why) => {
                    self.release(&key);
                    failures.push(format!("{key}: {why}"));
                }
            }
        }
        if posted > 0 {
            feed::record_completions(Trigger::Reconcile, false, posted);
        }
        if complete && failures.is_empty() {
            self.watermarks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(root_key, horizon - WATERMARK_SLACK);
            feed::record_reconcile_pass(PassOutcome::Ran);
        } else {
            if !failures.is_empty() {
                log::warn!(
                    "safehouse: captain catch-up over {} left {} merge(s) unreported ({}); \
                     they stay in the window for the next pass",
                    root.display(),
                    failures.len(),
                    failures.join("; ")
                );
            }
            feed::record_reconcile_pass(PassOutcome::Failed);
        }
        posted
    }
}

/// Act on one bus event. Never blocks the bus loop: the forge lookup and the
/// post run on their own task.
fn on_event(reporter: &Arc<Reporter>, event: Event, runtime: &tokio::runtime::Handle) {
    match event {
        Event::SweepExited {
            issue,
            repo: Some(root),
            ..
        } => {
            reporter.remember_workspace(&root);
            let reporter = Arc::clone(reporter);
            runtime.spawn(async move {
                if let Some(pr) = merge_source::merged_pr_for_issue(Path::new(&root), issue).await {
                    reporter.report_host_merge(&pr, Trigger::SweepExit).await;
                }
            });
        }
        Event::StepEnded {
            item,
            role,
            class,
            reason,
            repo: Some(root),
            ..
        } => {
            reporter.remember_workspace(&root);
            if step_may_have_merged(role, &class, &reason) {
                let reporter = Arc::clone(reporter);
                runtime.spawn(async move {
                    if let Some(pr) =
                        merge_source::merged_pr_by_number(Path::new(&root), item).await
                    {
                        reporter.report_host_merge(&pr, Trigger::Step).await;
                    }
                });
            }
        }
        _ => {}
    }
}

/// The reporter loop: bus events (the host path) and the captain scan timer.
async fn run(
    reporter: Arc<Reporter>,
    mut subscription: Subscription,
    interval: Duration,
    runtime: tokio::runtime::Handle,
) {
    let mut timer = tokio::time::interval(interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The first tick fires immediately; skip it so a freshly started daemon does
    // not scan before its workspace registry and live events are known.
    timer.tick().await;
    // One eager connect so `loom-daemon status` and new-host onboarding read the
    // real state (`connected` / `unreachable`) at startup instead of waiting for
    // the first merge. The connection is dropped; each report opens its own.
    match reporter.target.connect().await {
        Ok(_client) => log::info!("safehouse: safehoused reachable; merged-PR report ready"),
        Err(err) => log::warn!(
            "safehouse: safehoused is not reachable yet ({err:#}); reports retry on each merge \
             and the captain's catch-up scan"
        ),
    }
    loop {
        tokio::select! {
            biased;
            received = subscription.recv() => match received {
                Ok(event) => on_event(&reporter, event, &runtime),
                Err(RecvError::Closed) => {
                    log::debug!("safehouse: event bus closed; merged-PR reporter stopping");
                    break;
                }
                // Empty/Lagged are surfaced as events by `recv`; only Closed ends it.
                Err(_) => {}
            },
            _ = timer.tick() => {
                if reporter.scan_in_flight.swap(true, Ordering::AcqRel) {
                    continue; // the previous scan is still running
                }
                let reporter = Arc::clone(&reporter);
                runtime.spawn(async move {
                    reporter.captain_pass().await;
                    reporter.scan_in_flight.store(false, Ordering::Release);
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_role::StepRole;

    #[test]
    fn only_a_champion_merge_verdict_may_have_merged() {
        assert!(step_may_have_merged(StepRole::ChampionMerge, "verdict", "merged"));
        assert!(step_may_have_merged(StepRole::ChampionMerge, "verdict", "closed"));
        assert!(step_may_have_merged(StepRole::ChampionMerge, "verdict", "unclassified"));
        for reason in ["rejected", "triaged", "triage_blocked", "triage_operator"] {
            assert!(!step_may_have_merged(StepRole::ChampionMerge, "verdict", reason));
        }
        for class in ["noop", "refused", "failed"] {
            assert!(!step_may_have_merged(StepRole::ChampionMerge, class, "merged"));
        }
        assert!(!step_may_have_merged(StepRole::Builder, "verdict", "merged"));
    }
}
