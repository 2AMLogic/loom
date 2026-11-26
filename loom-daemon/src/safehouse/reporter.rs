//! The merged-PR reporter task (#13946).
//!
//! Two producers feed the one report in [`super::merge_report`]:
//!
//! - **The host that ran the merge posts.** This daemon subscribes to the event
//!   bus; a sweep merge phase naming a PR ([`Event::SweepPhase`]) or a Champion merge step
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
//! The captain keeps a watermark and any unfinished search in memory for each
//! workspace. The watermark advances after all ranges have been searched and
//! every merge has been accounted for. A search may span several ticks, but
//! only merges still inside the catch-up window are eligible for delivery.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use chrono::{DateTime, SubsecRound, Utc};

use super::merge_report::{self, Outcome, PrReport, ReportTarget};
use super::merge_source::{self, MergedPr};
use super::{
    forge_gate, resolve_socket, set_state, RecentMessages, SafehouseConfig, SafehouseState,
};
use crate::event_bus::{EventBus, RecvError, Subscription};
use crate::observability::ops::feed::{self, PassOutcome, Trigger};
use crate::types::Event;
use crate::workspace_registry::WorkspaceRegistry;

/// Captain scan cadence, in milliseconds. Test/operator override.
const SCAN_INTERVAL_ENV: &str = "LOOM_SAFEHOUSE_RECONCILE_INTERVAL_MS";
const DEFAULT_SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// How far back the captain will report a merge nobody reported, in seconds.
/// Bounds both staleness (a fresh install must not backfill old history into the
/// room) and cost. Also the freshness bound on a host's own report: a merge
/// older than this is not the one the exiting sweep just made.
const CATCHUP_ENV: &str = "LOOM_SAFEHOUSE_REPORT_CATCHUP_SECS";
const DEFAULT_CATCHUP: chrono::Duration = chrono::Duration::hours(6);

/// How long the captain leaves a fresh merge to the host that made it, in
/// seconds.
const GRACE_ENV: &str = "LOOM_SAFEHOUSE_REPORT_GRACE_SECS";
const DEFAULT_GRACE: chrono::Duration = chrono::Duration::minutes(10);

/// A scan starts this far before its watermark, because the forge's search index
/// trails a merge by seconds to minutes.
const WATERMARK_SLACK: chrono::Duration = chrono::Duration::minutes(30);

/// Most queries one workspace scan will make when the forge keeps
/// returning full pages.
const MAX_PAGES: u32 = 5;

/// Maximum local report keys. Confirmed keys may be evicted at capacity.
const REPORTED_CAP: usize = 10_000;

fn env_window(name: &str, default: chrono::Duration) -> chrono::Duration {
    super::env_nonempty(name)
        .and_then(|raw| raw.parse::<u64>().ok())
        .and_then(|secs| chrono::Duration::from_std(Duration::from_secs(secs)).ok())
        .unwrap_or(default)
}

fn scan_interval() -> Duration {
    super::env_nonempty(SCAN_INTERVAL_ENV)
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|ms| *ms > 0)
        .map_or(DEFAULT_SCAN_INTERVAL, Duration::from_millis)
}

/// The previous local outcome decides whether a new send needs a fresh read.
#[derive(Clone, Copy)]
enum ReportState {
    InFlight,
    Uncertain(DateTime<Utc>),
    Reported,
}

/// Start the merged-PR reporter on `runtime` when enabled. Returns the task
/// handle, or `None` (a byte-for-byte no-op: no bus subscription, no socket)
/// when disabled or when no socket path can be resolved. `state` is set to the
/// resolved config-only state immediately and updated by each report's connect.
///
/// The captain reads `fleet.captain` from `workspace_root` and scans that root
/// when the workspace registry is empty.
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
    /// Captain gate root and empty-registry fallback.
    default_root: PathBuf,
    /// Delivery state shared by sweep, step and captain reports for each `owner/repo#N`.
    reported: Mutex<HashMap<String, ReportState>>,
    /// Per-workspace scan watermarks (see the module docs).
    watermarks: Mutex<HashMap<PathBuf, DateTime<Utc>>>,
    /// Unfinished searches resume at the next tick, with their original end time.
    pending: Mutex<HashMap<PathBuf, ScanWindow>>,
    /// A scan is running; the next tick skips instead of overlapping it.
    scan_in_flight: AtomicBool,
    catchup: chrono::Duration,
    grace: chrono::Duration,
}

impl Reporter {
    pub(super) fn new(target: ReportTarget, default_root: PathBuf) -> Self {
        Self {
            target,
            default_root,
            reported: Mutex::new(HashMap::new()),
            watermarks: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            scan_in_flight: AtomicBool::new(false),
            catchup: env_window(CATCHUP_ENV, DEFAULT_CATCHUP),
            grace: env_window(GRACE_ENV, DEFAULT_GRACE),
        }
    }

    fn catchup_start(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now.checked_sub_signed(self.catchup)
            .unwrap_or_else(|| now - DEFAULT_CATCHUP)
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
            .get(root)
            .copied()
    }

    /// Reserve the key across awaits. A second worker cannot send while the
    /// first worker is deciding whether an earlier send landed.
    async fn deliver(&self, report: &PrReport, recent: Option<&RecentMessages>) -> Outcome {
        let key = report.key();
        let visible_in_room = recent.is_some_and(|room| report.is_reported_in(&room.bodies));
        let started = Utc::now();
        let previous = {
            let mut states = self.reported.lock().unwrap_or_else(PoisonError::into_inner);
            match states.get(&key) {
                Some(ReportState::Reported) => return Outcome::AlreadyReported,
                Some(ReportState::InFlight) => return Outcome::Failed("report in flight".into()),
                _ => {}
            }
            if !visible_in_room && recent.is_some_and(|room| !room.covers(report.merged_at)) {
                return Outcome::Failed("room history does not reach the merge".into());
            }
            if states.len() >= REPORTED_CAP {
                let cutoff = self.catchup_start(Utc::now());
                states.retain(|_, state| match state {
                    ReportState::InFlight => true,
                    ReportState::Uncertain(at) => *at >= cutoff,
                    ReportState::Reported => false,
                });
                if states.len() >= REPORTED_CAP && !states.contains_key(&key) {
                    return Outcome::Failed("local report cache full of pending sends".into());
                }
            }
            states.insert(key.clone(), ReportState::InFlight)
        };
        let since = match previous {
            Some(ReportState::Uncertain(at)) => at,
            _ => started,
        };
        let outcome = if visible_in_room {
            Outcome::AlreadyReported
        } else if matches!(previous, Some(ReportState::Uncertain(_))) {
            merge_report::post_after_read(&self.target, report, since).await
        } else {
            merge_report::post(&self.target, report).await
        };
        let mut states = self.reported.lock().unwrap_or_else(PoisonError::into_inner);
        match &outcome {
            Outcome::Posted | Outcome::AlreadyReported => {
                states.insert(key, ReportState::Reported);
            }
            Outcome::Uncertain(_) => {
                states.insert(key, ReportState::Uncertain(since));
            }
            Outcome::Failed(_) => {
                states.remove(&key);
            }
        }
        outcome
    }

    /// The host path: post the line for a merge this host just saw confirmed.
    /// An earlier ambiguous attempt requires a room read before sending again.
    pub(super) async fn report_host_merge(&self, pr: &MergedPr, trigger: Trigger) {
        if pr.merged_at < self.catchup_start(Utc::now()) {
            log::debug!(
                "safehouse: PR #{} merged at {} is outside the report window; skipping stale merge",
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
        match self.deliver(&report, None).await {
            Outcome::Posted => {
                feed::record_completions(trigger, false, 1);
                log::info!("safehouse: reported merged PR {key}");
            }
            Outcome::AlreadyReported => {
                log::debug!("safehouse: {key} was already in the room; not re-sent");
            }
            Outcome::Failed(why) | Outcome::Uncertain(why) => {
                log::warn!("safehouse: could not report merged PR {key}: {why}; continuing");
            }
        }
    }

    /// One captain catch-up pass over every scan target. A no-op unless this
    /// host is the armed fleet captain. Returns the number of lines posted.
    pub(super) async fn captain_pass(&self) -> usize {
        self.captain_pass_at(Utc::now()).await
    }

    pub(super) async fn captain_pass_at(&self, now: DateTime<Utc>) -> usize {
        let host = crate::sweep_registry::host_identity();
        let gate = crate::fleet_captain::resolve_gate_for_root(&self.default_root, &host);
        if !gate.is_armed() {
            log::trace!("safehouse: {}", gate.message("safehouse merged-PR catch-up"));
            return 0;
        }
        let roots: BTreeSet<_> = match WorkspaceRegistry::load_default() {
            Ok(registry) => registry
                .effective_roots(&self.default_root)
                .into_iter()
                .collect(),
            Err(err) => {
                log::warn!(
                    "safehouse: could not load workspace registry ({err:#}); deferring captain catch-up"
                );
                feed::record_reconcile_pass(PassOutcome::Failed);
                return 0;
            }
        };
        // Deregistered checkouts may later be reused for a different repository.
        self.watermarks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|root, _| roots.contains(root));
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|root, _| roots.contains(root));
        let mut recent = match merge_report::read_room(&self.target).await {
            Ok(recent) if recent.complete => recent,
            Ok(_) => {
                log::warn!("safehouse: incomplete fleet room read; deferring captain catch-up");
                feed::record_reconcile_pass(PassOutcome::Failed);
                return 0;
            }
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
        for root in roots {
            posted += self.scan_workspace(&root, &mut recent, now).await;
        }
        posted
    }

    /// Scan one workspace and post the merges the room lacks. Advances the
    /// workspace's watermark only when the pass was complete (see module docs).
    async fn scan_workspace(
        &self,
        root: &Path,
        recent: &mut RecentMessages,
        now: DateTime<Utc>,
    ) -> usize {
        if forge_gate::suppressed("pr list (scan)", root) {
            feed::record_reconcile_pass(PassOutcome::Deferred);
            return 0;
        }
        let cutoff = self.catchup_start(now);
        let horizon = now
            .checked_sub_signed(self.grace)
            .unwrap_or_else(|| now - DEFAULT_GRACE);
        let since = {
            let marks = self
                .watermarks
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            marks
                .get(root)
                .copied()
                .map_or(cutoff, |mark| mark.max(cutoff))
        };

        let mut window = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(root)
            .filter(|window| window.until >= cutoff)
            .unwrap_or_else(|| ScanWindow::new(since, horizon));
        if !window.search(root, cutoff).await {
            self.pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(root.to_owned(), window);
            feed::record_reconcile_pass(PassOutcome::Failed);
            return 0;
        }
        let mut rows = window.rows;
        rows.sort_by(|a, b| (a.merged_at, a.number, &a.url).cmp(&(b.merged_at, b.number, &b.url)));
        rows.dedup_by(|a, b| a.url == b.url);
        let mut posted = 0;
        let mut failures = Vec::new();
        for pr in rows
            .iter()
            .filter(|pr| (cutoff..=window.until).contains(&pr.merged_at))
        {
            let Some(report) = PrReport::from_merged(pr) else {
                continue;
            };
            let key = report.key();
            match self.deliver(&report, Some(recent)).await {
                Outcome::Posted => {
                    recent.bodies.push(report.body());
                    posted += 1;
                    log::info!("safehouse: captain reported merged PR {key} that no host reported");
                }
                Outcome::AlreadyReported => {}
                Outcome::Failed(why) | Outcome::Uncertain(why) => {
                    failures.push(format!("{key}: {why}"));
                }
            }
        }
        if posted > 0 {
            feed::record_completions(Trigger::Reconcile, false, posted);
        }
        if window.complete && failures.is_empty() {
            self.watermarks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(
                    root.to_owned(),
                    window
                        .until
                        .checked_sub_signed(WATERMARK_SLACK)
                        .unwrap_or(DateTime::<Utc>::MIN_UTC),
                );
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

/// Keep the exact remaining ranges across ticks. Deliver the discovered rows
/// together: posting the first full page early could evict the room history
/// that a later page needs to decide whether a hidden PR was reported.
struct ScanWindow {
    until: DateTime<Utc>,
    intervals: VecDeque<(DateTime<Utc>, DateTime<Utc>)>,
    rows: Vec<MergedPr>,
    complete: bool,
}

impl ScanWindow {
    fn new(since: DateTime<Utc>, until: DateTime<Utc>) -> Self {
        Self {
            until,
            intervals: VecDeque::from([(since.trunc_subsecs(0), until.trunc_subsecs(0))]),
            rows: Vec::new(),
            complete: true,
        }
    }

    /// Search at most MAX_PAGES this tick. Retry a failed query on a later
    /// tick. An unsplittable full page keeps the completed pass's watermark open.
    async fn search(&mut self, root: &Path, cutoff: DateTime<Utc>) -> bool {
        for _ in 0..MAX_PAGES {
            let Some((start, end)) = self.intervals.pop_front() else {
                break;
            };
            let start = start.max(cutoff.trunc_subsecs(0));
            if start > end {
                continue;
            }
            let Some(page) = merge_source::merged_prs_between(root, start, end).await else {
                self.intervals.push_back((start, end));
                break;
            };
            if page.saturated && start < end {
                let midpoint = start + chrono::Duration::seconds((end - start).num_seconds() / 2);
                self.intervals.push_back((start, midpoint));
                self.intervals
                    .push_back((midpoint + chrono::Duration::seconds(1), end));
            } else {
                // Leaf ranges are disjoint. A full single second can only
                // contribute its visible rows and must hold the watermark.
                self.rows.extend(page.rows);
                self.complete &= !page.saturated;
            }
        }
        self.intervals.is_empty()
    }
}

/// Only a completed merge naming a PR authorizes the host lookup. An issue
/// branch can be reused, and a sweep exit (including a cancellation) has no PR.
fn host_merge_hint(event: &Event) -> Option<(&str, u32, Trigger)> {
    match event {
        Event::SweepPhase {
            phase,
            pr_number: Some(pr),
            repo: Some(root),
            ..
        } if phase == "merge" => u32::try_from(*pr)
            .ok()
            .filter(|n| *n > 0)
            .map(|n| (root.as_str(), n, Trigger::SweepMerge)),
        Event::StepEnded {
            item,
            role: crate::step_role::StepRole::ChampionMerge,
            class,
            reason,
            repo: Some(root),
            ..
        } if class == "verdict" && reason == "merged" => Some((root, *item, Trigger::Step)),
        _ => None,
    }
}

/// Act on a bus event without blocking the bus loop on a forge or room read.
pub(super) fn on_event(reporter: &Arc<Reporter>, event: &Event, runtime: &tokio::runtime::Handle) {
    if let Some((root, number, trigger)) = host_merge_hint(event) {
        let root = PathBuf::from(root);
        let reporter = Arc::clone(reporter);
        runtime.spawn(async move {
            if let Some(pr) = merge_source::merged_pr_by_number(&root, number).await {
                reporter.report_host_merge(&pr, trigger).await;
            }
        });
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
    // Give the merging hosts one scan interval before starting catch-up.
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
                Ok(event) => on_event(&reporter, &event, &runtime),
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
    fn host_reports_only_a_merge_that_names_its_pr() {
        let sweep = |phase: &str, pr_number| Event::SweepPhase {
            issue: 9,
            phase: phase.into(),
            pr_number,
            repo: Some("root".into()),
        };
        assert_eq!(
            host_merge_hint(&sweep("merge", Some(50))),
            Some(("root", 50, Trigger::SweepMerge))
        );
        assert_eq!(host_merge_hint(&sweep("merge", None)), None);
        assert_eq!(host_merge_hint(&sweep("judge", Some(36))), None);
        // A cancellation on this issue's reused branch cannot report its earlier PR.
        assert_eq!(
            host_merge_hint(&Event::SweepExited {
                issue: 9,
                exit_code: None,
                duration_sec: 20,
                no_progress: false,
                death_class: None,
                repo: Some("root".into())
            }),
            None
        );
        let step = |role, class: &str, reason: &str| Event::StepEnded {
            item: 50,
            role,
            class: class.into(),
            reason: reason.into(),
            ran_for_sec: 1,
            repo: Some("root".into()),
        };
        assert_eq!(
            host_merge_hint(&step(StepRole::ChampionMerge, "verdict", "merged")),
            Some(("root", 50, Trigger::Step))
        );
        assert_eq!(host_merge_hint(&step(StepRole::ChampionMerge, "verdict", "closed")), None);
        assert_eq!(host_merge_hint(&step(StepRole::ChampionMerge, "noop", "merged")), None);
        assert_eq!(host_merge_hint(&step(StepRole::Builder, "verdict", "merged")), None);
    }
}
