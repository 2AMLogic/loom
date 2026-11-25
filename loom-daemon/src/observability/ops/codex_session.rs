//! Per-account Codex session-container state (#10455; Epic #10452).
//!
//! On 2026-10-05 a host lost Codex for ~21.5 h and the only trace was a
//! stream of ~1 s role failures attributed to (repo, role): a host-wide cause
//! read as many repo-level faults, and the role runner's per-root failure
//! machine demotes repeats to DEBUG. This module is the container-level
//! signal that machine cannot give:
//!
//! * the gauge `loom.codex_session.state{account,state,container}` — every
//!   state of the closed vocabulary is emitted each pass (current at 1, the
//!   rest at 0) so a transition never leaves two states active; and
//! * a WARN on every state change (including recovery) plus a bounded
//!   reminder while an account stays not-running.
//!
//! The WARN deliberately lives here, **not** in the role runner's
//! `RootTickLogAction` machine, and never goes through `log::debug!`: it is
//! per account, not per root, and must stay visible however long the
//! container is down.
//!
//! **Two halves, so the WARN does not depend on telemetry.** [`spawn_watch`]
//! is an always-on daemon task (started with the other observers, whether or
//! not any exporter is configured) that every [`WATCH_INTERVAL`] takes one
//! bounded [`session_state::snapshot`], publishes it for other readers
//! ([`session_state::latest`]), feeds the tracker and keeps the observations.
//! [`record`], called from the telemetry collector's pass, only turns the
//! newest observations into gauge points: it never calls docker, so a wedged
//! Docker cannot stall the collector.
//!
//! **Fail open, but loudly.** When docker cannot be queried
//! ([`Snapshot::Unavailable`]) the tracker holds every account's last state
//! and the gauge emits nothing: an unqueryable docker says nothing about any
//! container, so it must never read as `missing`. It is still a host-wide
//! Codex outage for a host that has session-managed accounts (spawn refuses
//! those ticks as `SESSION_DOWN`), so it gets its own WARN on entry, the same
//! [`REMINDER_SECS`] reminder while it lasts, and a WARN when docker answers
//! again.
//!
//! Passes never overlap: a tick is skipped while the previous pass is still
//! running, and the tracker lock is taken only after the snapshot, to fold
//! it.
//!
//! Only session-managed, enabled Codex accounts are read; a pool with none
//! costs zero docker calls. [`Tracker::observe`] and [`points`] are pure.
//!
//! **Every registered root (#10600).** The seats are those of every
//! registered root ([`session_seats::seats`], the reconciler's own rule), not
//! only the daemon's: a host whose daemon root has no session-managed account
//! but another root does is watched too, so that root's selection is not
//! silently blind. The seats are published with [`published_seats`] for
//! `loom-daemon status`.
//!
//! **One drift definition (#10600).** The snapshot is classified with the
//! reconciler's own [`session_state::effective_drift`], denials included, so
//! a container mounting a still-registered `firewall: true` repository reads
//! `stale_mounts` here, not `running`, while the reconciler removes it. An
//! unreadable registry gives no drift verdict (never `stale_mounts`).
//!
//! **Staleness (#10600).** Every reader fails open once the newest snapshot
//! is older than [`session_state::LATEST_MAX_AGE`]. The watch is registered
//! with the daemon's task-liveness tracking ([`crate::task_liveness`],
//! `codex_session_watch`), and each tick WARNs, on the same reminder cadence,
//! while seats exist and the newest snapshot is that old (a pass that keeps
//! overrunning); `loom-daemon status` shows the section `unavailable`.
//!
//! Besides the state gauge, each pass exports, per seat,
//! `loom.codex_session.record{account,container,kind}` (`hold`,
//! `drift_removal`: 1 while that on-disk record stands) and
//! `loom.codex_session.mount_drift{account,container,kind}` (`missing`,
//! `extra`, `denied`: how many paths; only when there is a drift verdict).
//!
//! **Seat signals (#15292).** The same pass also reads, per seat, the facts a
//! held or logged-out seat with quota left needs to be told apart from a
//! healthy one: login (`loom.codex_session.login`), the hold's reason class
//! and age (`hold_age`), hook trust (`hook_trust`) and the newest rate-limit
//! snapshot (`quota_used`, `quota_resets_at`, `quota_observed_at`,
//! `reset_credits`). Login, live limits and the reset-credit count come from
//! one bounded in-container `codex app-server` round trip per running seat
//! ([`codex_probe::probe_seat`], never host-direct); the rest are local files
//! and work while docker is down. **An unreadable source emits nothing** for
//! its field, never a false value, and every value is a number or a member of
//! a closed set: no token, path, account payload or free-form text is kept.

use crate::script_helpers::now_epoch_secs;
use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::telemetry::ops::{MetricName, MetricPoint};
use crate::tokens_pool::codex_check::{self, RateLimitWindow, UsageSnapshot};
use crate::tokens_pool::codex_probe::{self, ProbeOutcome, ProcessProbeTransport, SeatReading};
use crate::tokens_pool::private_workspace::containment;
use crate::tokens_pool::session_seats::{self, Seat};
use crate::tokens_pool::session_state::{
    self, DriftInputs, EffectiveDrift, SessionState, Snapshot,
};
use crate::tokens_pool::{session_drift_removal, session_hold};

/// How often the WARN repeats while an account's container stays not running.
pub const REMINDER_SECS: u64 = 15 * 60;

/// How often the watch takes a snapshot: the WARN lands within one interval.
pub const WATCH_INTERVAL: Duration = Duration::from_secs(60);

/// Observations older than this are not exported (the watch has stopped or
/// docker has been unqueryable since).
const FRESH_FOR: Duration = Duration::from_secs(3 * 60);

/// Per-account last-seen state and when it was last warned about.
#[derive(Debug, Default)]
pub struct Tracker {
    seen: HashMap<String, (SessionState, u64)>,
    /// While docker is unqueryable: when that was last warned about.
    unqueryable_warned: Option<u64>,
}

impl Tracker {
    /// Docker could not be queried this pass. Every account's state is held.
    /// Returns the WARN text on entering this condition and as a reminder
    /// every [`REMINDER_SECS`] while it lasts.
    pub fn unqueryable(&mut self, reason: &str, now: u64) -> Option<String> {
        let message = match self.unqueryable_warned {
            None => format!(
                "Codex session containers cannot be observed ({reason}): Codex ticks on this \
                 host's session-managed accounts will be refused until docker answers; is the \
                 Docker daemon running?"
            ),
            Some(at) if now.saturating_sub(at) >= REMINDER_SECS => format!(
                "Codex session containers still cannot be observed ({reason}; reminder every {} \
                 min)",
                REMINDER_SECS / 60
            ),
            Some(_) => return None,
        };
        self.unqueryable_warned = Some(now);
        Some(message)
    }

    /// Docker answered this pass; the WARN text when it had not before.
    pub fn queryable(&mut self) -> Option<String> {
        self.unqueryable_warned
            .take()
            .map(|_| "Codex session containers are observable again (docker answers)".to_string())
    }

    /// Forget accounts no longer in the inventory (removed or disabled), so a
    /// re-added account is treated as first sight.
    pub fn retain(&mut self, accounts: &[&str]) {
        self.seen
            .retain(|account, _| accounts.contains(&account.as_str()));
    }

    /// Feed one observation. Returns the WARN text when this observation is a
    /// state change worth reporting or a due reminder; `None` otherwise (an
    /// account first seen running, or unchanged inside the reminder window).
    pub fn observe(
        &mut self,
        account: &str,
        container: &str,
        state: SessionState,
        now: u64,
    ) -> Option<String> {
        let previous = self.seen.get(account).map(|(s, _)| *s);
        let last_warned = self.seen.get(account).map_or(0, |(_, at)| *at);
        let message = match previous {
            None if state == SessionState::Running => None,
            Some(prev) if prev == state => (state != SessionState::Running
                && now.saturating_sub(last_warned) >= REMINDER_SECS)
                .then(|| format!("{} (still, reminder every {} min)", describe(account, container, state), REMINDER_SECS / 60)),
            Some(prev) if state == SessionState::Running => Some(format!(
                "Codex session container for account {account} ({container}) recovered: {} -> running",
                prev.as_str()
            )),
            _ => Some(describe(account, container, state)),
        };
        let warned_at = if message.is_some() { now } else { last_warned };
        self.seen.insert(account.to_string(), (state, warned_at));
        message
    }
}

fn describe(account: &str, container: &str, state: SessionState) -> String {
    match state {
        SessionState::Restarting => format!(
            "Codex session container for account {account} ({container}) is restarting \
             (Docker is backing off a crash loop): Codex ticks on this account are refused; \
             see `docker logs {container}`"
        ),
        SessionState::Stopped | SessionState::Missing => format!(
            "Codex session container for account {account} ({container}) is {}: Codex ticks \
             on this account are refused until it runs again. The session reconciler restarts \
             it unless it is held, kept down by a removal record, or opted out; see \
             `loom-daemon status` (manual override: `loom-daemon accounts session start \
             {account} --mount-workspace <checkout parent>`)",
            state.as_str()
        ),
        SessionState::StaleMounts => format!(
            "Codex session container for account {account} ({container}) is running but its \
             workspace mounts no longer match what it may mount (stale_mounts: a registered root \
             is not mounted, a deregistered one still is, or a mount is now denied, #10364); the \
             session reconciler recreates (or, for a denied mount, removes) it once idle; see \
             `loom-daemon status`"
        ),
        SessionState::Running => {
            format!("Codex session container for account {account} ({container}) is running")
        }
    }
}

/// The gauge points for `observed` `(account, container, state)` triples.
#[must_use]
pub fn points(observed: &[(String, String, SessionState)]) -> Vec<MetricPoint> {
    observed
        .iter()
        .flat_map(|(account, container, current)| {
            SessionState::ALL.into_iter().map(move |state| {
                MetricPoint::int(MetricName::CodexSessionState, i64::from(state == *current))
                    .label("account", account.clone())
                    .label("state", state.as_str())
                    .label("container", container.clone())
            })
        })
        .collect()
}

type Observations = Vec<(String, String, SessionState)>;

/// One seat's on-disk records and drift verdict, for the extra gauges.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatFacts {
    pub account: String,
    pub container: String,
    pub held: bool,
    pub drift_removal: bool,
    /// `None`: no drift verdict (missing, private-clone, unlabelled, or the
    /// registry was unreadable).
    pub drift: Option<EffectiveDrift>,
}

/// The `record` and `mount_drift` gauge points for `facts`. Pure.
#[must_use]
pub fn fact_points(facts: &[SeatFacts]) -> Vec<MetricPoint> {
    let mut points = Vec::new();
    for fact in facts {
        let labelled = |point: MetricPoint, kind: &'static str| {
            point
                .label("account", fact.account.clone())
                .label("kind", kind)
                .label("container", fact.container.clone())
        };
        for (kind, on) in [("hold", fact.held), ("drift_removal", fact.drift_removal)] {
            let point = MetricPoint::int(MetricName::CodexSessionRecord, i64::from(on));
            points.push(labelled(point, kind));
        }
        if let Some(drift) = &fact.drift {
            for (kind, n) in [
                ("missing", drift.drift.missing.len()),
                ("extra", drift.extra_not_denied().len()),
                ("denied", drift.denied.len()),
            ] {
                let n = i64::try_from(n).unwrap_or(i64::MAX);
                points
                    .push(labelled(MetricPoint::int(MetricName::CodexSessionMountDrift, n), kind));
            }
        }
    }
    points
}

/// Each seat's facts from one available snapshot and the on-disk records.
#[must_use]
pub fn seat_facts(
    seats: &[Seat],
    snapshot: &Snapshot,
    held: &dyn Fn(&[PathBuf]) -> bool,
    removal: &dyn Fn(&[PathBuf]) -> bool,
) -> Vec<SeatFacts> {
    let Snapshot::Available(map) = snapshot else {
        return Vec::new();
    };
    seats
        .iter()
        .map(|seat| SeatFacts {
            account: seat.account.clone(),
            container: seat.container.clone(),
            held: held(&seat.profiles),
            drift_removal: removal(&seat.profiles),
            drift: map.get(&seat.container).and_then(|o| o.drift.clone()),
        })
        .collect()
}

/// Seats probed at once: the pass stays well inside one watch interval at a
/// dozen seats, without a docker/`codex` burst on a shared host.
const PROBE_PARALLEL: usize = 4;

/// No further probe batch starts once a pass has spent this long probing, so
/// a slow seat delays publication by one probe timeout at most.
const PROBE_BUDGET: Duration = Duration::from_secs(25);

/// Closed classes of a hold's reason (never the record's free text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldReason {
    /// `accounts session stop`, the only reason written today.
    OperatorStop,
    /// Any other text.
    Other,
}

impl HoldReason {
    #[must_use]
    pub fn classify(reason: &str) -> Self {
        if reason == session_hold::HOLD_REASON_OPERATOR_STOP {
            Self::OperatorStop
        } else {
            Self::Other
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OperatorStop => "operator_stop",
            Self::Other => "other",
        }
    }
}

/// A decoded hold: its reason class and how long ago it was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldDetail {
    pub reason: HoldReason,
    pub age_secs: u64,
}

/// One rate-limit window that is still current.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct QuotaWindow {
    /// Utilisation, a 0..1 fraction (a measured zero stays zero).
    pub used: f64,
    /// Reset instant, Unix seconds, when the window reports one.
    pub resets_at: Option<i64>,
}

/// The newest rate-limit reading for a seat, rolled-over windows removed.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quota {
    /// When the reading was taken, Unix seconds.
    pub observed_at: i64,
    /// The ~5 h window.
    pub five_hour: Option<QuotaWindow>,
    /// The ~7 d window.
    pub weekly: Option<QuotaWindow>,
}

/// What one pass learned about one seat beyond its container state. Every
/// field is `None` when its source was unreadable.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatSignals {
    pub account: String,
    pub container: String,
    /// `Some(true)` signed in, `Some(false)` signed out.
    pub login: Option<bool>,
    /// Present only while the seat is held and its record decodes.
    pub hold: Option<HoldDetail>,
    /// `recorded`, `sealed` or `missing`, only for an assessable profile.
    pub hook_trust: Option<&'static str>,
    pub quota: Option<Quota>,
    /// Unused limit resets (`availableCount`).
    pub reset_credits: Option<u64>,
}

/// The local sources a pass reads, injectable so the pass is testable without
/// a filesystem or docker. [`Readers::real`] is the production wiring.
pub struct Readers<'a> {
    /// Whether the seat is held (any hold file, decodable or not).
    pub held: &'a dyn Fn(&[PathBuf]) -> bool,
    /// The newest decodable hold across the seat's profiles.
    pub hold: &'a dyn Fn(&[PathBuf]) -> Option<session_hold::Hold>,
    /// One profile's hook trust; `None` when it could not be assessed.
    pub trust: &'a dyn Fn(&Path) -> Option<&'static str>,
    /// The newest local rate-limit snapshot across the seat's profiles.
    pub local_quota: &'a dyn Fn(&[PathBuf]) -> Option<UsageSnapshot>,
}

impl Readers<'static> {
    /// The on-disk readers.
    #[must_use]
    pub fn real() -> Self {
        Readers {
            held: &session_hold::held_across,
            hold: &session_hold::newest_hold,
            trust: &containment::assessed_trust_state,
            local_quota: &|profiles| {
                profiles
                    .iter()
                    .filter_map(|p| codex_check::latest_usage_snapshot(p))
                    .max_by_key(|s| s.observed_at)
            },
        }
    }
}

/// A seat's hook trust from its profiles' verdicts: every profile must be
/// assessable and agree, since the profile a session runs with cannot be
/// told apart otherwise. Pure.
#[must_use]
pub fn combine_trust(verdicts: &[Option<&'static str>]) -> Option<&'static str> {
    let first = (*verdicts.first()?)?;
    verdicts.iter().all(|v| *v == Some(first)).then_some(first)
}

/// A window's expiry: its reported reset, else the observation plus its own
/// length. `None` when neither is known.
fn window_expiry(window: &RateLimitWindow, observed_at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    window.resets_at.or_else(|| {
        let minutes = i64::try_from(window.window_minutes?).ok()?;
        observed_at.checked_add_signed(chrono::Duration::try_minutes(minutes)?)
    })
}

fn current_window(
    window: Option<&RateLimitWindow>,
    observed_at: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Option<QuotaWindow> {
    let window = window?;
    if !window.used_fraction.is_finite() || window.used_fraction < 0.0 {
        return None;
    }
    // A rolled-over window measures a period that has ended.
    if window_expiry(window, observed_at).is_some_and(|expiry| expiry <= now) {
        return None;
    }
    Some(QuotaWindow {
        used: window.used_fraction.min(1.0),
        resets_at: window.resets_at.map(|at| at.timestamp()),
    })
}

/// The seat's quota: the live reading when the probe measured one, else the
/// newest local snapshot. Pure.
#[must_use]
pub fn quota_from(
    live: Option<&UsageSnapshot>,
    local: Option<&UsageSnapshot>,
    now: DateTime<Utc>,
) -> Option<Quota> {
    let snapshot = live.or(local)?;
    let five_hour = current_window(snapshot.primary.as_ref(), snapshot.observed_at, now);
    let weekly = current_window(snapshot.secondary.as_ref(), snapshot.observed_at, now);
    if five_hour.is_none() && weekly.is_none() {
        return None;
    }
    Some(Quota {
        observed_at: snapshot.observed_at.timestamp(),
        five_hour,
        weekly,
    })
}

/// Each seat's signals from the probe readings (by account) and the local
/// readers. Independent of docker: a seat with no reading still reports its
/// hold, hook trust and local quota. Pure.
#[must_use]
pub fn seat_signals(
    seats: &[Seat],
    readings: &HashMap<String, SeatReading>,
    readers: &Readers<'_>,
    now: DateTime<Utc>,
) -> Vec<SeatSignals> {
    let now_ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
    seats
        .iter()
        .map(|seat| {
            let reading = readings.get(&seat.account);
            let hold = (readers.held)(&seat.profiles)
                .then(|| (readers.hold)(&seat.profiles))
                .flatten()
                .map(|hold| HoldDetail {
                    reason: HoldReason::classify(&hold.reason),
                    age_secs: now_ms.saturating_sub(hold.held_at_unix_ms) / 1000,
                });
            let verdicts: Vec<_> = seat.profiles.iter().map(|p| (readers.trust)(p)).collect();
            let live = reading.and_then(|r| match &r.outcome {
                ProbeOutcome::Measured(snapshot) => Some(snapshot),
                _ => None,
            });
            let local = (readers.local_quota)(&seat.profiles);
            SeatSignals {
                account: seat.account.clone(),
                container: seat.container.clone(),
                login: reading.and_then(|r| r.login),
                hold,
                hook_trust: combine_trust(&verdicts),
                quota: quota_from(live, local.as_ref(), now),
                reset_credits: reading.and_then(|r| r.reset_credits),
            }
        })
        .collect()
}

/// The `login`, `hold_age`, `hook_trust` and quota points for `signals`. Pure.
#[must_use]
pub fn signal_points(signals: &[SeatSignals]) -> Vec<MetricPoint> {
    let int = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
    let mut points = Vec::new();
    for seat in signals {
        let labelled = |point: MetricPoint| {
            point
                .label("account", seat.account.clone())
                .label("container", seat.container.clone())
        };
        if let Some(login) = seat.login {
            points
                .push(labelled(MetricPoint::int(MetricName::CodexSessionLogin, i64::from(login))));
        }
        if let Some(hold) = seat.hold {
            points.push(
                labelled(MetricPoint::int(MetricName::CodexSessionHoldAge, int(hold.age_secs)))
                    .label("reason", hold.reason.as_str()),
            );
        }
        if let Some(current) = seat.hook_trust {
            for state in ["recorded", "sealed", "missing"] {
                let point = MetricPoint::int(
                    MetricName::CodexSessionHookTrust,
                    i64::from(state == current),
                );
                points.push(labelled(point).label("state", state));
            }
        }
        if let Some(quota) = &seat.quota {
            points.push(labelled(MetricPoint::int(
                MetricName::CodexSessionQuotaObservedAt,
                quota.observed_at,
            )));
            for (kind, window) in [("5h", &quota.five_hour), ("7d", &quota.weekly)] {
                let Some(window) = window else { continue };
                let used = MetricPoint::double(MetricName::CodexSessionQuotaUsed, window.used);
                points.push(labelled(used).label("kind", kind));
                if let Some(at) = window.resets_at {
                    let resets = MetricPoint::int(MetricName::CodexSessionQuotaResetsAt, at);
                    points.push(labelled(resets).label("kind", kind));
                }
            }
        }
        if let Some(count) = seat.reset_credits {
            points
                .push(labelled(MetricPoint::int(MetricName::CodexSessionResetCredits, int(count))));
        }
    }
    points
}

/// Probe each running seat once, `PROBE_PARALLEL` at a time and within
/// [`PROBE_BUDGET`], keyed by account. Nothing is probed while docker is
/// unqueryable, and a seat whose probe gave no reading is simply absent.
fn probe_readings(
    seats: &[Seat],
    snapshot: &Snapshot,
    probe: &(dyn Fn(&str) -> Option<SeatReading> + Sync),
    budget: Duration,
) -> HashMap<String, SeatReading> {
    let running: Vec<&Seat> = seats
        .iter()
        .filter(|seat| snapshot.state_of(&seat.container) == Some(SessionState::Running))
        .collect();
    let started = Instant::now();
    let mut readings = HashMap::new();
    for batch in running.chunks(PROBE_PARALLEL) {
        if started.elapsed() >= budget {
            break;
        }
        std::thread::scope(|scope| {
            let handles: Vec<_> = batch
                .iter()
                .map(|seat| (*seat, scope.spawn(|| probe(&seat.container))))
                .collect();
            for (seat, handle) in handles {
                if let Some(reading) = handle.join().ok().flatten() {
                    readings.insert(seat.account.clone(), reading);
                }
            }
        });
    }
    readings
}

/// What one pass leaves for the collector to export.
#[derive(Debug, Default)]
struct Exported {
    observed: Observations,
    facts: Vec<SeatFacts>,
    signals: Vec<SeatSignals>,
}

static TRACKER: Mutex<Option<Tracker>> = Mutex::new(None);
static LAST: Mutex<Option<(Instant, Arc<Exported>)>> = Mutex::new(None);
static SEATS: Mutex<Option<Arc<Vec<Seat>>>> = Mutex::new(None);

/// The seats the newest watch pass found (every registered root), for
/// `loom-daemon status`. `None` before the first pass.
#[must_use]
pub fn published_seats() -> Option<Arc<Vec<Seat>>> {
    SEATS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

fn publish_seats(seats: Vec<Seat>) {
    *SEATS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(seats));
}

/// The staleness WARN's memory: when it last warned.
#[derive(Debug, Default)]
pub struct Staleness {
    warned_at: Option<u64>,
}

impl Staleness {
    /// The WARN text when seats exist and the newest snapshot is older than
    /// [`session_state::LATEST_MAX_AGE`] (on entry, then every
    /// [`REMINDER_SECS`]), or once when it is fresh again. Pure.
    pub fn check(&mut self, seats: bool, age: Option<Duration>, now: u64) -> Option<String> {
        let stale = seats && age.is_some_and(|age| age > session_state::LATEST_MAX_AGE);
        if !stale {
            return self.warned_at.take().map(|_| {
                "Codex session container snapshots are fresh again; selection sees the \
                 containers again"
                    .to_string()
            });
        }
        if self
            .warned_at
            .is_some_and(|at| now.saturating_sub(at) < REMINDER_SECS)
        {
            return None;
        }
        self.warned_at = Some(now);
        Some(format!(
            "the newest Codex session container snapshot is {}s old (over {}s): the session \
             watch is not completing passes, so dispatch selection and `loom-daemon status` \
             cannot see the containers (they fail open); reminder every {} min",
            age.unwrap_or_default().as_secs(),
            session_state::LATEST_MAX_AGE.as_secs(),
            REMINDER_SECS / 60
        ))
    }
}

/// What one pass decided: the observations to export (`None`: export
/// nothing) and the WARN lines.
#[derive(Debug, Default, PartialEq)]
pub struct Pass {
    pub observed: Option<Observations>,
    pub warn: Vec<String>,
}

/// Fold one snapshot into the tracker for `accounts` (`(account, container)`).
/// Pure apart from the tracker.
pub fn fold(
    tracker: &mut Tracker,
    accounts: &[(String, String)],
    snapshot: &Snapshot,
    now: u64,
) -> Pass {
    let names: Vec<&str> = accounts.iter().map(|(a, _)| a.as_str()).collect();
    tracker.retain(&names);
    let mut pass = Pass::default();
    if let Snapshot::Unavailable(reason) = snapshot {
        if !accounts.is_empty() {
            pass.warn.extend(tracker.unqueryable(reason, now));
        }
        return pass;
    }
    pass.warn.extend(tracker.queryable());
    let observed: Observations = accounts
        .iter()
        .map(|(account, container)| {
            let state = snapshot
                .state_of(container)
                .unwrap_or(SessionState::Missing);
            (account.clone(), container.clone(), state)
        })
        .collect();
    for (account, container, state) in &observed {
        pass.warn
            .extend(tracker.observe(account, container, *state, now));
    }
    pass.observed = Some(observed);
    pass
}

/// Set while a watch pass runs, so a slow pass is never joined by another.
static IN_FLIGHT: AtomicBool = AtomicBool::new(false);

/// Clears [`IN_FLIGHT`] when the pass ends, however it ends.
struct InFlight;

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.store(false, Ordering::Release);
    }
}

/// One watch pass (blocking): snapshot, then fold it under the tracker lock,
/// WARN, and keep the observations.
fn watch_pass(root: &Path) {
    let seats = session_seats::seats(root);
    publish_seats(seats.clone());
    let accounts: Vec<(String, String)> = seats
        .iter()
        .map(|seat| (seat.account.clone(), seat.container.clone()))
        .collect();
    if accounts.is_empty() {
        TRACKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(Tracker::default)
            .retain(&[]);
        *LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        return;
    }
    // An unreadable registry is `None`: no drift verdict, never "all stale".
    let registered: Option<Vec<PathBuf>> =
        crate::workspace_registry::WorkspaceRegistry::load_default()
            .ok()
            .map(|registry| registry.roots());
    // Scoped override verbatim, else the shared lookup (#14885); an
    // unresolvable docker stays unlaunchable, so the snapshot reads
    // Unavailable and nothing is marked down (fail open).
    let docker = crate::docker_bin::scoped_or_fail_open(
        std::env::var("LOOM_CODEX_SESSION_DOCKER")
            .ok()
            .filter(|d| !d.is_empty())
            .as_deref(),
    );
    // The docker call runs without the tracker lock held.
    let started = Instant::now();
    let inputs = DriftInputs::new(registered.as_deref());
    let snapshot =
        Arc::new(session_state::snapshot(&docker, &inputs, session_state::SNAPSHOT_DEADLINE));
    session_state::publish(Arc::clone(&snapshot), started);
    let facts = seat_facts(&seats, &snapshot, &session_hold::held_across, &|profiles| {
        session_drift_removal::read(profiles).is_some()
    });
    let pass = {
        let mut guard = TRACKER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        fold(
            guard.get_or_insert_with(Tracker::default),
            &accounts,
            &snapshot,
            now_epoch_secs(),
        )
    };
    for message in &pass.warn {
        log::warn!("session: {message}");
    }
    // The signals (#15292) do not need docker, so they publish through an
    // outage; only the in-container probes do.
    let readings = probe_readings(
        &seats,
        &snapshot,
        &|container| codex_probe::probe_seat(&ProcessProbeTransport, container, Utc::now()),
        PROBE_BUDGET,
    );
    let signals = seat_signals(&seats, &readings, &Readers::real(), Utc::now());
    *LAST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some((
        started,
        Arc::new(Exported {
            observed: pass.observed.unwrap_or_default(),
            facts,
            signals,
        }),
    ));
}

/// Start the always-on watch for the account pools of every root registered
/// with `workspace_root`'s daemon. Runs whether or not telemetry is
/// configured; each pass runs off the async runtime, its docker calls are
/// killed at the snapshot deadline, and a tick is skipped while the previous
/// pass is still running. Registered with task liveness as
/// [`crate::task_liveness::CODEX_SESSION_WATCH`]; it beats once per completed
/// pass.
pub fn spawn_watch(workspace_root: PathBuf) -> tokio::task::JoinHandle<()> {
    use crate::task_liveness::{self, CODEX_SESSION_WATCH};
    task_liveness::register(
        CODEX_SESSION_WATCH,
        WATCH_INTERVAL,
        task_liveness::default_stale_after(WATCH_INTERVAL),
    );
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(WATCH_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut staleness = Staleness::default();
        loop {
            ticker.tick().await;
            let seats = published_seats().is_some_and(|seats| !seats.is_empty());
            let age = session_state::newest().map(|(age, _)| age);
            if let Some(message) = staleness.check(seats, age, now_epoch_secs()) {
                log::warn!("session: {message}");
            }
            if IN_FLIGHT.swap(true, Ordering::AcqRel) {
                log::info!(
                    "session: previous Codex session watch pass still running; tick skipped"
                );
                continue;
            }
            let root = workspace_root.clone();
            let pass = tokio::task::spawn_blocking(move || {
                let _in_flight = InFlight;
                watch_pass(&root);
                task_liveness::beat_if_registered(CODEX_SESSION_WATCH);
            });
            // This only stops *waiting*: a blocking pass cannot be cancelled.
            // `IN_FLIGHT` is what keeps an overrunning pass from being joined
            // by more.
            let bound = session_state::SNAPSHOT_DEADLINE
                + PROBE_BUDGET
                + codex_probe::PROBE_TIMEOUT
                + Duration::from_secs(10);
            if tokio::time::timeout(bound, pass).await.is_err() {
                log::info!("session: Codex session watch pass overran {bound:?}; not waiting");
            }
        }
    })
}

/// The points of the newest observations in `last`, none once they are older
/// than [`FRESH_FOR`]. Reads memory only: no filesystem, no docker.
fn fresh_points(last: &Mutex<Option<(Instant, Arc<Exported>)>>) -> Vec<MetricPoint> {
    let exported = last
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .as_ref()
        .filter(|(at, _)| at.elapsed() <= FRESH_FOR)
        .map(|(_, exported)| Arc::clone(exported));
    let Some(exported) = exported else {
        return Vec::new();
    };
    let mut all = points(&exported.observed);
    all.extend(fact_points(&exported.facts));
    all.extend(signal_points(&exported.signals));
    all
}

/// The collector's part: export the gauge from the watch's newest
/// observations, when the ops sink is registered. Never calls docker.
pub fn record() {
    let all = fresh_points(&LAST);
    if !all.is_empty() {
        super::emit_metrics(all);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const C: &str = "loom-codex-session-a";

    #[test]
    fn first_sight_running_is_silent() {
        let mut t = Tracker::default();
        assert!(t.observe("a", C, SessionState::Running, 0).is_none());
        assert!(t.observe("a", C, SessionState::Running, 10_000).is_none());
    }

    #[test]
    fn running_to_stopped_warns_naming_account_and_container() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        let m = t.observe("a", C, SessionState::Stopped, 300).unwrap();
        assert!(m.contains("account a") && m.contains(C) && m.contains("stopped"));
    }

    #[test]
    fn repeat_inside_the_window_is_quiet_and_after_it_warns_again() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        assert!(t.observe("a", C, SessionState::Missing, 100).is_some());
        assert!(t
            .observe("a", C, SessionState::Missing, 100 + REMINDER_SECS - 1)
            .is_none());
        assert!(t
            .observe("a", C, SessionState::Missing, 100 + REMINDER_SECS)
            .is_some());
        // the reminder re-arms from the reminder, not from the first WARN
        assert!(t
            .observe("a", C, SessionState::Missing, 100 + REMINDER_SECS + 5)
            .is_none());
    }

    #[test]
    fn recovery_is_reported() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        t.observe("a", C, SessionState::Stopped, 10);
        let m = t.observe("a", C, SessionState::Running, 20).unwrap();
        assert!(m.contains("recovered") && m.contains("stopped -> running"));
        assert!(t.observe("a", C, SessionState::Running, 30).is_none());
    }

    #[test]
    fn first_sight_down_warns_and_accounts_are_independent() {
        let mut t = Tracker::default();
        assert!(t.observe("a", C, SessionState::Stopped, 5).is_some());
        assert!(t
            .observe("b", "loom-codex-session-b", SessionState::Running, 5)
            .is_none());
        assert!(t
            .observe("a", C, SessionState::StaleMounts, 6)
            .unwrap()
            .contains("stale_mounts"));
    }

    #[test]
    fn gauge_emits_every_state_with_the_current_one_at_one() {
        let pts = points(&[("a".into(), C.into(), SessionState::Stopped)]);
        assert_eq!(pts.len(), SessionState::ALL.len());
        for p in &pts {
            let state = &p.labels["state"];
            let expect = i64::from(state == "stopped");
            assert_eq!(p.value, crate::telemetry::ops::MetricValue::Int(expect), "{state}");
            assert_eq!(p.labels["account"], "a");
            assert_eq!(p.labels["container"], C);
        }
    }

    #[test]
    fn the_warn_never_goes_through_a_debug_log() {
        let source = include_str!("codex_session.rs");
        let needle = ["log::", "debug!"].concat();
        assert!(!source
            .replace(&format!("\"{needle}\""), "")
            .contains(&format!("{needle}(")));
    }

    fn available(states: &[(&str, SessionState)]) -> Snapshot {
        Snapshot::Available(
            states
                .iter()
                .map(|(c, state)| {
                    (
                        (*c).to_string(),
                        session_state::Observed {
                            state: *state,
                            inspect: serde_json::Value::Null,
                            drift: None,
                        },
                    )
                })
                .collect(),
        )
    }

    #[test]
    fn restarting_warns_as_down() {
        let mut t = Tracker::default();
        t.observe("a", C, SessionState::Running, 0);
        let m = t.observe("a", C, SessionState::Restarting, 60).unwrap();
        assert!(m.contains("restarting") && m.contains(C));
    }

    #[test]
    fn an_unqueryable_docker_holds_state_and_exports_nothing() {
        let mut t = Tracker::default();
        let accounts = vec![("a".to_string(), C.to_string())];
        let up = fold(&mut t, &accounts, &available(&[(C, SessionState::Running)]), 0);
        assert_eq!(up.observed.unwrap()[0].2, SessionState::Running);
        let gone = Snapshot::Unavailable("Cannot connect to the Docker daemon".into());
        let first = fold(&mut t, &accounts, &gone, 60);
        assert_eq!(first.observed, None, "no gauge, never `missing`");
        assert_eq!(first.warn.len(), 1, "a host-wide outage WARNs on entry");
        assert!(first.warn[0].contains("cannot be observed"));
        let quiet = fold(&mut t, &accounts, &gone, 60 + REMINDER_SECS - 1);
        assert!(quiet.warn.is_empty(), "no storm inside the reminder window");
        let reminder = fold(&mut t, &accounts, &gone, 60 + REMINDER_SECS);
        assert_eq!(reminder.warn.len(), 1, "bounded reminder");
        assert!(reminder.warn[0].contains("still"));
        // Docker answers again and the container is still up: no container
        // state change, only the "observable again" line.
        let back = fold(&mut t, &accounts, &available(&[(C, SessionState::Running)]), 2_000);
        assert_eq!(back.warn.len(), 1);
        assert!(back.warn[0].contains("observable again"));
        let steady = fold(&mut t, &accounts, &available(&[(C, SessionState::Running)]), 2_060);
        assert!(steady.warn.is_empty());
    }

    #[test]
    fn an_unqueryable_docker_with_no_session_accounts_is_silent() {
        let mut t = Tracker::default();
        let gone = Snapshot::Unavailable("Cannot connect".into());
        assert!(fold(&mut t, &[], &gone, 0).warn.is_empty());
    }

    #[test]
    fn the_selection_ceiling_is_two_watch_intervals() {
        assert_eq!(session_state::LATEST_MAX_AGE, 2 * WATCH_INTERVAL);
    }

    #[test]
    fn an_account_absent_from_an_available_snapshot_is_missing() {
        let mut t = Tracker::default();
        let accounts = vec![("a".to_string(), C.to_string())];
        let pass = fold(&mut t, &accounts, &available(&[]), 0);
        assert_eq!(pass.observed.unwrap()[0].2, SessionState::Missing);
        assert_eq!(pass.warn.len(), 1);
    }

    #[test]
    fn a_stale_snapshot_warns_while_seats_exist_then_recovers_once() {
        let mut st = Staleness::default();
        let old = Some(session_state::LATEST_MAX_AGE + Duration::from_secs(1));
        let fresh = Some(Duration::from_secs(5));
        assert!(st.check(false, old, 0).is_none(), "no seats: nothing is blind");
        assert!(st.check(true, fresh, 0).is_none());
        assert!(st.check(true, None, 0).is_none(), "nothing published yet");
        let warn = st.check(true, old, 100).unwrap();
        assert!(warn.contains("121s old") && warn.contains("fail open"), "{warn}");
        assert!(st.check(true, old, 100 + REMINDER_SECS - 1).is_none());
        assert!(st.check(true, old, 100 + REMINDER_SECS).is_some(), "bounded reminder");
        assert!(st
            .check(true, fresh, 2_000)
            .unwrap()
            .contains("fresh again"));
        assert!(st.check(true, fresh, 2_060).is_none());
    }

    #[test]
    fn fact_points_carry_records_and_drift_counts() {
        let drift = EffectiveDrift {
            drift: session_state::MountDrift {
                missing: vec![PathBuf::from("/ws/b")],
                extra: vec![PathBuf::from("/ws/old"), PathBuf::from("/ws/secret")],
            },
            denied: vec![PathBuf::from("/ws/secret")],
        };
        let facts = [
            SeatFacts {
                account: "a".into(),
                container: C.into(),
                held: true,
                drift_removal: false,
                drift: Some(drift),
            },
            SeatFacts {
                account: "b".into(),
                container: "loom-codex-session-b".into(),
                held: false,
                drift_removal: true,
                drift: None,
            },
        ];
        let pts = fact_points(&facts);
        let value = |account: &str, name: MetricName, kind: &str| {
            pts.iter()
                .find(|p| {
                    p.name == name && p.labels["account"] == account && p.labels["kind"] == kind
                })
                .map(|p| p.value)
        };
        use crate::telemetry::ops::MetricValue::Int;
        assert_eq!(value("a", MetricName::CodexSessionRecord, "hold"), Some(Int(1)));
        assert_eq!(value("a", MetricName::CodexSessionRecord, "drift_removal"), Some(Int(0)));
        assert_eq!(value("a", MetricName::CodexSessionMountDrift, "missing"), Some(Int(1)));
        assert_eq!(value("a", MetricName::CodexSessionMountDrift, "extra"), Some(Int(1)));
        assert_eq!(value("a", MetricName::CodexSessionMountDrift, "denied"), Some(Int(1)));
        assert_eq!(value("b", MetricName::CodexSessionRecord, "drift_removal"), Some(Int(1)));
        assert_eq!(
            value("b", MetricName::CodexSessionMountDrift, "missing"),
            None,
            "no drift verdict, no drift points"
        );
        assert!(pts
            .iter()
            .all(|p| p.labels["container"].starts_with("loom-codex-session-")));
    }

    #[test]
    fn seat_facts_need_an_available_snapshot() {
        let seats = vec![Seat {
            account: "a".into(),
            container: C.into(),
            profiles: vec![PathBuf::from("/p/a")],
        }];
        let yes = |_: &[PathBuf]| true;
        let gone = Snapshot::Unavailable("down".into());
        assert!(seat_facts(&seats, &gone, &yes, &yes).is_empty());
        let facts = seat_facts(&seats, &available(&[]), &yes, &yes);
        assert_eq!(facts.len(), 1);
        assert!(facts[0].held && facts[0].drift_removal && facts[0].drift.is_none());
    }

    #[test]
    fn accounts_that_leave_the_inventory_are_forgotten() {
        let mut t = Tracker::default();
        let a = vec![("a".to_string(), C.to_string())];
        fold(&mut t, &a, &available(&[(C, SessionState::Stopped)]), 0);
        fold(&mut t, &[], &available(&[]), 10);
        // Re-added and still stopped: first sight again, so it warns at once.
        let pass = fold(&mut t, &a, &available(&[(C, SessionState::Stopped)]), 20);
        assert_eq!(pass.warn.len(), 1);
    }

    // ---- seat signals (#15292) ----------------------------------------------

    use crate::telemetry::ops::MetricValue;
    use crate::tokens_pool::codex_probe::parse_probe_replies;

    const NOW_SECS: i64 = 1_800_000_000;

    fn clock() -> DateTime<Utc> {
        DateTime::from_timestamp(NOW_SECS, 0).unwrap()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(secs, 0).unwrap()
    }

    fn window(used: f64, minutes: u64, resets: Option<i64>) -> RateLimitWindow {
        RateLimitWindow {
            used_fraction: used,
            window_minutes: Some(minutes),
            resets_at: resets.map(at),
        }
    }

    fn weekly_only(used: f64, resets: Option<i64>, observed: i64) -> UsageSnapshot {
        UsageSnapshot {
            observed_at: at(observed),
            primary: None,
            secondary: Some(window(used, 10_080, resets)),
        }
    }

    fn one_seat(profiles: &[&str]) -> Vec<Seat> {
        vec![Seat {
            account: "a".into(),
            container: C.into(),
            profiles: profiles.iter().map(PathBuf::from).collect(),
        }]
    }

    struct Fixture {
        held: bool,
        hold: Option<session_hold::Hold>,
        trust: HashMap<&'static str, Option<&'static str>>,
        local: Option<UsageSnapshot>,
    }

    impl Fixture {
        fn held_seat() -> Self {
            Fixture {
                held: true,
                hold: Some(session_hold::Hold {
                    schema_version: 1,
                    reason: session_hold::HOLD_REASON_OPERATOR_STOP.into(),
                    held_at_unix_ms: u64::try_from((NOW_SECS - 7_200) * 1000).unwrap(),
                }),
                trust: HashMap::from([("/p/a", Some("recorded"))]),
                local: Some(weekly_only(0.17, Some(NOW_SECS + 86_400), NOW_SECS - 600)),
            }
        }

        fn signals(
            &self,
            seats: &[Seat],
            readings: &HashMap<String, SeatReading>,
        ) -> Vec<SeatSignals> {
            let held = |_: &[PathBuf]| self.held;
            let hold = |_: &[PathBuf]| self.hold.clone();
            let trust = |p: &Path| self.trust.get(p.to_str().unwrap()).copied().flatten();
            let local = |_: &[PathBuf]| self.local.clone();
            seat_signals(
                seats,
                readings,
                &Readers {
                    held: &held,
                    hold: &hold,
                    trust: &trust,
                    local_quota: &local,
                },
                clock(),
            )
        }
    }

    fn find(
        pts: &[MetricPoint],
        name: MetricName,
        label: Option<(&str, &str)>,
    ) -> Option<MetricValue> {
        pts.iter()
            .find(|p| {
                p.name == name
                    && label.is_none_or(|(k, v)| p.labels.get(k).map(String::as_str) == Some(v))
            })
            .map(|p| p.value)
    }

    #[test]
    fn a_held_seat_with_weekly_quota_left_is_visible_without_docker_or_a_probe() {
        let signals = Fixture::held_seat().signals(&one_seat(&["/p/a"]), &HashMap::new());
        let pts = signal_points(&signals);
        assert_eq!(
            find(&pts, MetricName::CodexSessionHoldAge, Some(("reason", "operator_stop"))),
            Some(MetricValue::Int(7_200))
        );
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaUsed, Some(("kind", "7d"))),
            Some(MetricValue::Double(0.17))
        );
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaResetsAt, Some(("kind", "7d"))),
            Some(MetricValue::Int(NOW_SECS + 86_400))
        );
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaObservedAt, None),
            Some(MetricValue::Int(NOW_SECS - 600))
        );
        assert_eq!(find(&pts, MetricName::CodexSessionQuotaUsed, Some(("kind", "5h"))), None);
        // Nothing needing a container is invented.
        assert_eq!(find(&pts, MetricName::CodexSessionLogin, None), None);
        assert_eq!(find(&pts, MetricName::CodexSessionResetCredits, None), None);
        assert_eq!(
            find(&pts, MetricName::CodexSessionHookTrust, Some(("state", "recorded"))),
            Some(MetricValue::Int(1))
        );
        assert_eq!(
            find(&pts, MetricName::CodexSessionHookTrust, Some(("state", "missing"))),
            Some(MetricValue::Int(0))
        );
        for p in &pts {
            assert_eq!(p.labels["account"], "a");
            assert_eq!(p.labels["container"], C);
            assert!(
                p.labels
                    .keys()
                    .all(|k| ["account", "container", "reason", "state", "kind"]
                        .contains(&k.as_str()))
            );
        }
    }

    #[test]
    fn no_snapshot_and_no_probe_emit_nothing() {
        let fixture = Fixture {
            held: false,
            hold: None,
            trust: HashMap::from([("/p/a", None)]),
            local: None,
        };
        let signals = fixture.signals(&one_seat(&["/p/a"]), &HashMap::new());
        assert_eq!(signals.len(), 1);
        assert!(signal_points(&signals).is_empty(), "{signals:?}");
    }

    #[test]
    fn a_malformed_hold_still_holds_but_yields_no_age() {
        let fixture = Fixture {
            hold: None,
            ..Fixture::held_seat()
        };
        let pts = signal_points(&fixture.signals(&one_seat(&["/p/a"]), &HashMap::new()));
        assert_eq!(find(&pts, MetricName::CodexSessionHoldAge, None), None);
    }

    #[test]
    fn an_unrecognised_reason_is_bounded_and_a_future_hold_ages_zero() {
        let mut fixture = Fixture::held_seat();
        fixture.hold = Some(session_hold::Hold {
            schema_version: 1,
            reason: "/home/x/secret path: because".into(),
            held_at_unix_ms: u64::try_from((NOW_SECS + 60) * 1000).unwrap(),
        });
        let pts = signal_points(&fixture.signals(&one_seat(&["/p/a"]), &HashMap::new()));
        let age = pts
            .iter()
            .find(|p| p.name == MetricName::CodexSessionHoldAge)
            .unwrap();
        assert_eq!(age.labels["reason"], "other");
        assert_eq!(age.value, MetricValue::Int(0));
        assert!(pts
            .iter()
            .all(|p| p.labels.values().all(|v| !v.contains('/'))));
    }

    #[test]
    fn a_hold_record_without_a_hold_is_not_reported() {
        let fixture = Fixture {
            held: false,
            ..Fixture::held_seat()
        };
        let signals = fixture.signals(&one_seat(&["/p/a"]), &HashMap::new());
        assert_eq!(signals[0].hold, None);
    }

    fn reading(login: &str, rate_limits: &str) -> SeatReading {
        let text =
            format!("{{\"id\":2,\"result\":{login}}}\n{{\"id\":3,\"result\":{rate_limits}}}\n");
        parse_probe_replies(&text, clock())
    }

    #[test]
    fn a_live_reading_supplies_login_quota_and_reset_credits() {
        let live = reading(
            r#"{"account":{"type":"chatgpt","email":"x@example.com"}}"#,
            &format!(
                r#"{{"rateLimits":{{"primary":{{"usedPercent":0,"windowDurationMins":300,"resetsAt":{}}},"secondary":{{"usedPercent":42.5,"windowDurationMins":10080,"resetsAt":{}}}}},"rateLimitResetCredits":{{"availableCount":2,"credits":[{{"id":"c1"}}]}}}}"#,
                NOW_SECS + 1_000,
                NOW_SECS + 5_000
            ),
        );
        let fixture = Fixture {
            held: false,
            hold: None,
            ..Fixture::held_seat()
        };
        let readings = HashMap::from([("a".to_string(), live)]);
        let pts = signal_points(&fixture.signals(&one_seat(&["/p/a"]), &readings));
        assert_eq!(find(&pts, MetricName::CodexSessionLogin, None), Some(MetricValue::Int(1)));
        assert_eq!(
            find(&pts, MetricName::CodexSessionResetCredits, None),
            Some(MetricValue::Int(2))
        );
        // Live beats the local snapshot (0.17), and a measured zero is kept.
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaUsed, Some(("kind", "7d"))),
            Some(MetricValue::Double(0.425))
        );
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaUsed, Some(("kind", "5h"))),
            Some(MetricValue::Double(0.0))
        );
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaResetsAt, Some(("kind", "5h"))),
            Some(MetricValue::Int(NOW_SECS + 1_000))
        );
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaObservedAt, None),
            Some(MetricValue::Int(NOW_SECS))
        );
    }

    #[test]
    fn signed_out_is_zero_and_the_local_snapshot_still_supplies_quota() {
        let out = reading(r#"{"account":null}"#, r#"{}"#);
        assert_eq!(out.login, Some(false));
        let fixture = Fixture {
            held: false,
            hold: None,
            ..Fixture::held_seat()
        };
        let readings = HashMap::from([("a".to_string(), out)]);
        let pts = signal_points(&fixture.signals(&one_seat(&["/p/a"]), &readings));
        assert_eq!(find(&pts, MetricName::CodexSessionLogin, None), Some(MetricValue::Int(0)));
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaUsed, Some(("kind", "7d"))),
            Some(MetricValue::Double(0.17))
        );
    }

    #[test]
    fn the_account_and_quota_replies_are_read_independently() {
        // A reply for the limits only: login unknown, never a false "signed out".
        let limits_only = parse_probe_replies(
            r#"{"id":3,"result":{"rateLimits":{"primary":{"usedPercent":10,"windowDurationMins":10080,"resetsAt":1900000000}}}}"#,
            clock(),
        );
        assert_eq!(limits_only.login, None);
        assert!(matches!(limits_only.outcome, ProbeOutcome::Measured(_)));
        // An account reply with no limits reply.
        let account_only = parse_probe_replies(r#"{"id":2,"result":{"account":{}}}"#, clock());
        assert_eq!(account_only.login, Some(true));
        assert_eq!(account_only.reset_credits, None);
        // A reply with an unusable account value is unknown.
        let odd = parse_probe_replies(r#"{"id":2,"result":{"account":"x"}}"#, clock());
        assert_eq!(odd.login, None);
        assert_eq!(parse_probe_replies("", clock()).login, None);
    }

    #[test]
    fn reset_credit_counts_cover_zero_positive_missing_and_short_detail_lists() {
        let count = |credits: &str| {
            parse_probe_replies(
                &format!(r#"{{"id":3,"result":{{"rateLimits":{{}}{credits}}}}}"#),
                clock(),
            )
            .reset_credits
        };
        assert_eq!(count(r#","rateLimitResetCredits":{"availableCount":0}"#), Some(0));
        assert_eq!(count(r#","rateLimitResetCredits":{"availableCount":3}"#), Some(3));
        assert_eq!(count(""), None, "older CLIs omit it");
        assert_eq!(count(r#","rateLimitResetCredits":{"availableCount":-1}"#), None);
        assert_eq!(
            count(r#","rateLimitResetCredits":{"availableCount":4,"credits":[{"id":"1"}]}"#),
            Some(4),
            "fewer detail rows than the count"
        );
        assert_eq!(
            count(r#","rateLimitResetCredits":{"credits":[{"id":"1"}]}"#),
            None,
            "details alone are not a count"
        );
    }

    #[test]
    fn a_weekly_only_primary_slot_files_under_seven_days() {
        let r = reading(
            r#"{"account":{}}"#,
            &format!(
                r#"{{"rateLimits":{{"primary":{{"usedPercent":30,"windowDurationMins":10080,"resetsAt":{}}},"secondary":null}}}}"#,
                NOW_SECS + 9_000
            ),
        );
        let fixture = Fixture {
            held: false,
            hold: None,
            local: None,
            ..Fixture::held_seat()
        };
        let readings = HashMap::from([("a".to_string(), r)]);
        let pts = signal_points(&fixture.signals(&one_seat(&["/p/a"]), &readings));
        assert_eq!(
            find(&pts, MetricName::CodexSessionQuotaUsed, Some(("kind", "7d"))),
            Some(MetricValue::Double(0.3))
        );
        assert_eq!(find(&pts, MetricName::CodexSessionQuotaUsed, Some(("kind", "5h"))), None);
    }

    #[test]
    fn rolled_over_windows_are_not_current_quota() {
        let snap = UsageSnapshot {
            observed_at: at(NOW_SECS - 100_000),
            primary: Some(window(0.9, 300, Some(NOW_SECS - 82_000))),
            secondary: Some(window(0.5, 10_080, Some(NOW_SECS + 3_600))),
        };
        let quota = quota_from(None, Some(&snap), clock()).unwrap();
        assert_eq!(quota.five_hour, None, "its reset has passed");
        assert_eq!(quota.weekly.unwrap().used, 0.5);
        // No reported reset: a reading older than its own window is stale too.
        let aged = UsageSnapshot {
            observed_at: at(NOW_SECS - 400 * 60),
            primary: Some(window(0.9, 300, None)),
            secondary: Some(window(0.0, 10_080, None)),
        };
        let quota = quota_from(None, Some(&aged), clock()).unwrap();
        assert_eq!(quota.five_hour, None);
        assert_eq!(
            quota.weekly,
            Some(QuotaWindow {
                used: 0.0,
                resets_at: None
            }),
            "measured zero kept"
        );
        let dead = UsageSnapshot {
            primary: None,
            ..snap
        };
        let dead = UsageSnapshot {
            secondary: Some(window(0.5, 10_080, Some(NOW_SECS - 1))),
            ..dead
        };
        assert_eq!(quota_from(None, Some(&dead), clock()), None);
    }

    #[test]
    fn hook_trust_needs_every_profile_assessable_and_agreeing() {
        assert_eq!(combine_trust(&[]), None);
        assert_eq!(combine_trust(&[Some("sealed")]), Some("sealed"));
        assert_eq!(combine_trust(&[Some("missing"), Some("missing")]), Some("missing"));
        assert_eq!(combine_trust(&[Some("recorded"), Some("missing")]), None, "disagree");
        assert_eq!(combine_trust(&[Some("recorded"), None]), None, "one unreadable");
        assert_eq!(combine_trust(&[None]), None);
    }

    #[test]
    fn a_second_root_for_the_same_seat_gives_one_set_of_points() {
        let mut fixture = Fixture::held_seat();
        fixture.trust = HashMap::from([("/p/a", Some("recorded")), ("/q/a", Some("recorded"))]);
        let seats = one_seat(&["/p/a", "/q/a"]);
        let pts = signal_points(&fixture.signals(&seats, &HashMap::new()));
        let n = |name| pts.iter().filter(|p| p.name == name).count();
        assert_eq!(n(MetricName::CodexSessionHoldAge), 1);
        assert_eq!(n(MetricName::CodexSessionHookTrust), 3);
        // Contradictory profiles drop the verdict rather than contradict.
        fixture.trust.insert("/q/a", Some("missing"));
        let pts = signal_points(&fixture.signals(&seats, &HashMap::new()));
        assert!(find(&pts, MetricName::CodexSessionHookTrust, None).is_none());
    }

    #[test]
    fn only_running_seats_are_probed_and_a_failed_probe_leaves_no_reading() {
        let seats = vec![
            Seat {
                account: "up".into(),
                container: "c-up".into(),
                profiles: vec![],
            },
            Seat {
                account: "down".into(),
                container: "c-down".into(),
                profiles: vec![],
            },
            Seat {
                account: "flaky".into(),
                container: "c-flaky".into(),
                profiles: vec![],
            },
        ];
        let snap = available(&[
            ("c-up", SessionState::Running),
            ("c-down", SessionState::Stopped),
            ("c-flaky", SessionState::Running),
        ]);
        let calls = Mutex::new(Vec::new());
        let probe = |container: &str| {
            calls.lock().unwrap().push(container.to_string());
            (container == "c-up")
                .then(|| parse_probe_replies(r#"{"id":2,"result":{"account":{}}}"#, clock()))
        };
        let readings = probe_readings(&seats, &snap, &probe, Duration::from_secs(60));
        let mut called = calls.into_inner().unwrap();
        called.sort();
        assert_eq!(called, ["c-flaky", "c-up"]);
        assert_eq!(readings.keys().collect::<Vec<_>>(), ["up"]);
        let gone = Snapshot::Unavailable("down".into());
        let none = probe_readings(
            &seats,
            &gone,
            &|_: &str| panic!("docker is down"),
            Duration::from_secs(60),
        );
        assert!(none.is_empty(), "a docker outage probes nothing");
    }

    #[test]
    fn an_exhausted_budget_starts_no_further_probe_batch() {
        let seats: Vec<Seat> = (0..PROBE_PARALLEL + 1)
            .map(|i| Seat {
                account: format!("a{i}"),
                container: format!("c{i}"),
                profiles: vec![],
            })
            .collect();
        let states: Vec<(String, SessionState)> = seats
            .iter()
            .map(|s| (s.container.clone(), SessionState::Running))
            .collect();
        let refs: Vec<(&str, SessionState)> =
            states.iter().map(|(c, s)| (c.as_str(), *s)).collect();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let probe = |_: &str| {
            calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            None
        };
        probe_readings(&seats, &available(&refs), &probe, Duration::from_millis(10));
        assert_eq!(calls.load(Ordering::SeqCst), PROBE_PARALLEL, "the second batch never started");
    }

    #[test]
    fn the_collector_reads_memory_only_and_stale_or_cleared_samples_vanish() {
        let signals = Fixture::held_seat().signals(&one_seat(&["/p/a"]), &HashMap::new());
        let exported = |signals| Exported {
            signals,
            ..Exported::default()
        };
        let last = Mutex::new(Some((Instant::now(), Arc::new(exported(signals.clone())))));
        let fresh = fresh_points(&last);
        assert!(find(&fresh, MetricName::CodexSessionHoldAge, None).is_some());
        // Docker down: no container state, yet the local signals still export.
        assert!(find(&fresh, MetricName::CodexSessionState, None).is_none());
        // The next pass cannot read the snapshot: its field is gone, not stale.
        let cleared = SeatSignals {
            quota: None,
            ..signals[0].clone()
        };
        *last.lock().unwrap() = Some((Instant::now(), Arc::new(exported(vec![cleared]))));
        let after = fresh_points(&last);
        assert!(find(&after, MetricName::CodexSessionQuotaUsed, None).is_none());
        assert!(find(&after, MetricName::CodexSessionHoldAge, None).is_some());
        // Expired samples are not exported.
        if let Some(old) = Instant::now().checked_sub(FRESH_FOR + Duration::from_secs(1)) {
            *last.lock().unwrap() = Some((old, Arc::new(exported(signals))));
            assert!(fresh_points(&last).is_empty());
        }
        *last.lock().unwrap() = None;
        assert!(fresh_points(&last).is_empty());
    }
}
