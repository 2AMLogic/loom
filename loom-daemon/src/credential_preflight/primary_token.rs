//! Expiry and rotation tracking of the primary writer token (#15327).
//!
//! On 2026-11-25 four crew hosts' primary refresh tick stopped rotating the
//! writer token without a log line; the token expired an hour later and every
//! claim answered `401 Bad credentials` for five hours. The existing health
//! signal ([`super::record_forge_credential_failure`]) only fires when a tick
//! *runs and fails*, so a tick that never ran recorded nothing.
//!
//! This module watches the **published token itself**:
//!
//! - [`record_published`] remembers when the primary token was last written
//!   and when it expires.
//! - [`stale_hold_at`] makes [`super::forge_credential_stale`] true from the
//!   moment that token is (nearly) past `expires_at`, whether or not any tick
//!   is still running, for the same bounded grace the failure streaks use.
//! - [`spawn_monitor`] samples every [`SAMPLE_INTERVAL`] on its own ticker:
//!   it exports the token age / remaining life / last-rotation gauges and
//!   opens (or resolves) the `forge.credential-expired` loom-ui alert.
//! - [`reject_expired_mint`] turns a mint whose `expires_at` is already past
//!   into a failure instead of a cache hit.
//!
//! Like the failure streaks, the tracker is a plain value
//! ([`PrimaryTokenTracker`]) so tests drive a local instance with explicit
//! instants; the production singleton is one `static` built from it.

use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::GithubAppOutcome;
use crate::telemetry::ops::{MetricName, MetricPoint};

/// A token this close to its `expires_at` (or past it) counts as expired.
///
/// The shell helper re-mints under 600 s of remaining life and the tick runs
/// every 300 s, so a healthy token never has less than ~300 s left; 60 s only
/// ever fires for a token that stopped rotating.
pub const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

/// How often the monitor samples the tracker.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// The token published to the primary `GH_CONFIG_DIR` most recently.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PublishedToken {
    /// The installation owner, for the gauges' `owner` label.
    owner: String,
    /// When it was written: the last rotation.
    published_at: DateTime<Utc>,
    /// The minter's `expires_at`; `None` when it was empty or unparseable
    /// (the age gauges still work, the expiry watch is blind to it).
    expires_at: Option<DateTime<Utc>>,
}

/// What the tracker knows at one instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenStatus {
    pub owner: String,
    pub last_rotation: DateTime<Utc>,
    /// Seconds since [`Self::last_rotation`].
    pub age_secs: i64,
    /// Seconds until `expires_at`; negative once past. `None` when unknown.
    pub remaining_secs: Option<i64>,
}

/// The primary writer token's publication record, as a plain value.
#[derive(Debug, Default)]
pub struct PrimaryTokenTracker {
    published: Option<PublishedToken>,
}

/// Parse a minter's `expires_at` (RFC 3339).
fn parse_expiry(expires_at: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(expires_at.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

/// The instant from which a token expiring at `expires_at` counts as expired.
fn expiry_threshold(expires_at: DateTime<Utc>) -> DateTime<Utc> {
    expires_at
        - chrono::Duration::from_std(EXPIRY_MARGIN).unwrap_or_else(|_| chrono::Duration::zero())
}

impl PrimaryTokenTracker {
    /// Record that the primary token, minted for `owner_repo`, was published
    /// at `now` and expires at `expires_at`.
    pub fn record_published(&mut self, owner_repo: &str, expires_at: &str, now: DateTime<Utc>) {
        self.published = Some(PublishedToken {
            owner: super::owner_of_nwo(owner_repo).to_string(),
            published_at: now,
            expires_at: parse_expiry(expires_at),
        });
    }

    /// The tracker's reading as of `now`; `None` before the first publication.
    #[must_use]
    pub fn status_at(&self, now: DateTime<Utc>) -> Option<TokenStatus> {
        let p = self.published.as_ref()?;
        Some(TokenStatus {
            owner: p.owner.clone(),
            last_rotation: p.published_at,
            age_secs: (now - p.published_at).num_seconds().max(0),
            remaining_secs: p.expires_at.map(|e| (e - now).num_seconds()),
        })
    }

    /// Why the published token is unusable as of `now`, or `None` while it has
    /// life left (or its expiry is unknown).
    #[must_use]
    pub fn expiry_reason_at(&self, now: DateTime<Utc>) -> Option<String> {
        let p = self.published.as_ref()?;
        let expires_at = p.expires_at?;
        (now >= expiry_threshold(expires_at)).then(|| {
            format!(
                "the primary writer token published at {} expired at {} and was never replaced; \
                 every forge write will answer 401 Bad credentials",
                p.published_at.to_rfc3339(),
                expires_at.to_rfc3339()
            )
        })
    }

    /// Whether the expired token still falls inside the `grace` window, measured
    /// from the moment it became unusable, so the hold is bounded like a
    /// failure streak's is ([`super::credential_stale_hold`]).
    #[must_use]
    pub fn stale_hold_at(&self, now: DateTime<Utc>, grace: Duration) -> bool {
        let Some(expires_at) = self.published.as_ref().and_then(|p| p.expires_at) else {
            return false;
        };
        let since = now - expiry_threshold(expires_at);
        let grace = chrono::Duration::from_std(grace).unwrap_or(chrono::Duration::MAX);
        since >= chrono::Duration::zero() && since < grace
    }
}

static PRIMARY_TOKEN: OnceLock<Mutex<PrimaryTokenTracker>> = OnceLock::new();

fn tracker() -> std::sync::MutexGuard<'static, PrimaryTokenTracker> {
    PRIMARY_TOKEN
        .get_or_init(|| Mutex::new(PrimaryTokenTracker::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Empty the process-global tracker, **tests only** (same contract as
/// [`super::reset_forge_credential_streaks`], which calls it).
#[cfg(test)]
pub(crate) fn reset() {
    *tracker() = PrimaryTokenTracker::default();
}

/// Record that the primary writer token for `owner_repo` was just written to
/// disk. Call after every successful primary publication (startup and tick).
pub fn record_published(owner_repo: &str, expires_at: &str) {
    tracker().record_published(owner_repo, expires_at, Utc::now());
}

/// The global tracker's reading now.
#[must_use]
pub fn status() -> Option<TokenStatus> {
    tracker().status_at(Utc::now())
}

/// [`PrimaryTokenTracker::expiry_reason_at`] on the global tracker, now.
#[must_use]
pub fn expiry_reason() -> Option<String> {
    tracker().expiry_reason_at(Utc::now())
}

/// [`PrimaryTokenTracker::stale_hold_at`] on the global tracker, now.
#[must_use]
pub(super) fn stale_hold(grace: Duration) -> bool {
    tracker().stale_hold_at(Utc::now(), grace)
}

/// Turn a mint that is already expired into a failure.
///
/// The shell helper reports a cached token as `Minted` with its original
/// `expires_at`; a cache that returns a dead token must not read as a healthy
/// cache hit that merely needs no write. A missing or unparseable `expires_at`
/// passes through (the helper's own contract is that it re-mints under 600 s).
#[must_use]
pub fn reject_expired_mint(outcome: GithubAppOutcome, now: DateTime<Utc>) -> GithubAppOutcome {
    if let GithubAppOutcome::Minted {
        ref expires_at,
        ref app_id,
        ref installation_id,
        ..
    } = outcome
    {
        if let Some(expiry) = parse_expiry(expires_at) {
            if now >= expiry_threshold(expiry) {
                return GithubAppOutcome::Error(format!(
                    "the minter returned a token that is already expired (expires_at {expires_at}, \
                     app {app_id} installation {installation_id}); refusing to treat it as a \
                     cache hit"
                ));
            }
        }
    }
    outcome
}

/// The gauge points for `status`: age, remaining life and last-rotation time,
/// each labelled by the installation `owner`.
#[must_use]
pub fn points(status: &TokenStatus) -> Vec<MetricPoint> {
    let mut points = vec![
        MetricPoint::int(MetricName::DaemonForgeTokenAge, status.age_secs)
            .label("owner", status.owner.clone()),
        MetricPoint::int(
            MetricName::DaemonForgeTokenLastRotation,
            status.last_rotation.timestamp(),
        )
        .label("owner", status.owner.clone()),
    ];
    if let Some(remaining) = status.remaining_secs {
        points.push(
            MetricPoint::int(MetricName::DaemonForgeTokenRemaining, remaining)
                .label("owner", status.owner.clone()),
        );
    }
    points
}

/// One monitor pass: export the gauges and reconcile the loom-ui alert.
/// Returns the expiry reason, when the token is expired.
pub fn sample() -> Option<String> {
    if let Some(status) = status() {
        crate::observability::ops::emit_metrics(points(&status));
    }
    let reason = expiry_reason();
    super::loom_ui::primary_token_checked(reason.as_deref());
    reason
}

/// Sample every [`SAMPLE_INTERVAL`] for the daemon's lifetime, on its own task
/// so a wedged refresh tick cannot silence it. Pure async around a
/// non-blocking [`sample`].
pub fn spawn_monitor() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async {
        let mut ticker = tokio::time::interval(SAMPLE_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut warned = false;
        loop {
            ticker.tick().await;
            match sample() {
                Some(reason) if !warned => {
                    log::warn!("credential_preflight: {reason} — #15327");
                    warned = true;
                }
                None => warned = false,
                Some(_) => {}
            }
        }
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    fn minted(expires_at: &str) -> GithubAppOutcome {
        GithubAppOutcome::Minted {
            token: "ghs_secret".into(),
            installation_id: "9".into(),
            app_id: "4".into(),
            expires_at: expires_at.into(),
        }
    }

    #[test]
    fn a_fresh_token_is_healthy_and_a_stale_one_is_expired() {
        let mut t = PrimaryTokenTracker::default();
        assert!(t.expiry_reason_at(at("2026-11-25T12:00:00Z")).is_none());
        t.record_published("2AMLogic/2am", "2026-11-25T13:00:00Z", at("2026-11-25T12:00:00Z"));
        assert!(t.expiry_reason_at(at("2026-11-25T12:30:00Z")).is_none());
        // inside the margin counts as expired, so does well past
        assert!(t.expiry_reason_at(at("2026-11-25T12:59:30Z")).is_some());
        let reason = t.expiry_reason_at(at("2026-11-25T14:00:00Z")).unwrap();
        assert!(reason.contains("2026-11-25T13:00:00"), "{reason}");
        assert!(!reason.contains("ghs_"), "never a token: {reason}");
    }

    #[test]
    fn a_fresh_publish_clears_expiry() {
        let mut t = PrimaryTokenTracker::default();
        t.record_published("o/r", "2026-11-25T13:00:00Z", at("2026-11-25T12:00:00Z"));
        assert!(t.expiry_reason_at(at("2026-11-25T14:00:00Z")).is_some());
        t.record_published("o/r", "2026-11-25T15:00:00Z", at("2026-11-25T14:00:00Z"));
        assert!(t.expiry_reason_at(at("2026-11-25T14:01:00Z")).is_none());
        assert!(!t.stale_hold_at(at("2026-11-25T14:01:00Z"), Duration::from_secs(1800)));
    }

    #[test]
    fn the_stale_hold_is_bounded_by_the_grace_window() {
        let mut t = PrimaryTokenTracker::default();
        t.record_published("o/r", "2026-11-25T13:00:00Z", at("2026-11-25T12:00:00Z"));
        let grace = Duration::from_secs(1800);
        assert!(!t.stale_hold_at(at("2026-11-25T12:30:00Z"), grace), "still live");
        assert!(t.stale_hold_at(at("2026-11-25T12:59:30Z"), grace));
        assert!(t.stale_hold_at(at("2026-11-25T13:20:00Z"), grace));
        assert!(!t.stale_hold_at(at("2026-11-25T14:00:00Z"), grace), "reasserts after grace");
    }

    #[test]
    fn an_unknown_expiry_is_never_called_expired() {
        let mut t = PrimaryTokenTracker::default();
        t.record_published("o/r", "", at("2026-11-25T12:00:00Z"));
        assert!(t.expiry_reason_at(at("2030-01-01T00:00:00Z")).is_none());
        let s = t.status_at(at("2026-11-25T12:10:00Z")).unwrap();
        assert_eq!(s.age_secs, 600);
        assert_eq!(s.remaining_secs, None);
    }

    #[test]
    fn status_reports_age_and_remaining_life() {
        let mut t = PrimaryTokenTracker::default();
        assert!(t.status_at(at("2026-11-25T12:00:00Z")).is_none());
        t.record_published("2AMLogic/2am", "2026-11-25T13:00:00Z", at("2026-11-25T12:00:00Z"));
        let s = t.status_at(at("2026-11-25T12:20:00Z")).unwrap();
        assert_eq!(s.owner, "2AMLogic");
        assert_eq!(s.age_secs, 1200);
        assert_eq!(s.remaining_secs, Some(2400));
        assert_eq!(
            t.status_at(at("2026-11-25T13:10:00Z"))
                .unwrap()
                .remaining_secs,
            Some(-600)
        );
    }

    #[test]
    fn an_already_expired_mint_is_an_error_not_a_cache_hit() {
        let now = at("2026-11-25T13:00:00Z");
        match reject_expired_mint(minted("2026-11-25T12:00:00Z"), now) {
            GithubAppOutcome::Error(reason) => {
                assert!(reason.contains("already expired"), "{reason}");
                assert!(reason.contains("2026-11-25T12:00:00Z"), "{reason}");
                assert!(!reason.contains("ghs_secret"), "{reason}");
            }
            other => panic!("expected an error, got {other:?}"),
        }
        // within the margin too
        assert!(matches!(
            reject_expired_mint(minted("2026-11-25T13:00:30Z"), now),
            GithubAppOutcome::Error(_)
        ));
    }

    #[test]
    fn a_live_or_unparseable_mint_passes_through() {
        let now = at("2026-11-25T13:00:00Z");
        for expires_at in ["2026-11-25T13:55:00Z", "", "z"] {
            assert_eq!(reject_expired_mint(minted(expires_at), now), minted(expires_at));
        }
        assert_eq!(
            reject_expired_mint(GithubAppOutcome::NotConfigured, now),
            GithubAppOutcome::NotConfigured
        );
    }

    #[test]
    fn the_gauges_carry_age_remaining_and_last_rotation_per_owner() {
        let mut t = PrimaryTokenTracker::default();
        t.record_published("2AMLogic/2am", "2026-11-25T13:00:00Z", at("2026-11-25T12:00:00Z"));
        let first = points(&t.status_at(at("2026-11-25T12:10:00Z")).unwrap());
        let by_name =
            |pts: &[MetricPoint], n: MetricName| pts.iter().find(|p| p.name == n).unwrap().value;
        use crate::telemetry::ops::{MetricKind, MetricValue};
        assert_eq!(by_name(&first, MetricName::DaemonForgeTokenAge), MetricValue::Int(600));
        assert_eq!(by_name(&first, MetricName::DaemonForgeTokenRemaining), MetricValue::Int(3000));
        assert_eq!(MetricName::DaemonForgeTokenAge.kind(), MetricKind::Gauge);
        assert!(first.iter().all(|p| p.labels["owner"] == "2AMLogic"));

        // a rotation moves the gauges: age resets, last-rotation advances
        t.record_published("2AMLogic/2am", "2026-11-25T14:00:00Z", at("2026-11-25T12:30:00Z"));
        let second = points(&t.status_at(at("2026-11-25T12:31:00Z")).unwrap());
        assert_eq!(by_name(&second, MetricName::DaemonForgeTokenAge), MetricValue::Int(60));
        let (MetricValue::Int(before), MetricValue::Int(after)) = (
            by_name(&first, MetricName::DaemonForgeTokenLastRotation),
            by_name(&second, MetricName::DaemonForgeTokenLastRotation),
        ) else {
            panic!("last rotation is an integer gauge");
        };
        assert!(after > before);
    }

    /// The whole chain on the process-global state: a token past `expires_at`
    /// makes the staleness signal true and opens the loom-ui alert even though
    /// no tick ran; a fresh publish clears both.
    #[test]
    #[serial_test::serial]
    fn an_expired_published_token_is_stale_and_alerts_until_replaced() {
        crate::credential_preflight::reset_forge_credential_streaks();
        let ago = |m: i64| (Utc::now() - chrono::Duration::minutes(m)).to_rfc3339();
        let ahead = |m: i64| (Utc::now() + chrono::Duration::minutes(m)).to_rfc3339();

        record_published("2AMLogic/2am", &ahead(30));
        assert!(!crate::credential_preflight::forge_credential_stale());
        assert!(sample().is_none());
        assert!(!super::super::loom_ui::expired_alert_open());

        // the refresh stops; the token ages past its expiry (published an hour
        // ago, expired five minutes ago)
        record_published("2AMLogic/2am", &ago(5));
        assert!(crate::credential_preflight::forge_credential_stale());
        let summary = crate::credential_preflight::forge_credential_stale_summary().unwrap();
        assert!(summary.contains("never replaced"), "{summary}");
        assert!(sample().is_some());
        assert!(super::super::loom_ui::expired_alert_open());

        // a fresh publish clears the staleness; the next sample resolves the alert
        record_published("2AMLogic/2am", &ahead(55));
        assert!(!crate::credential_preflight::forge_credential_stale());
        assert!(sample().is_none());
        assert!(!super::super::loom_ui::expired_alert_open());

        crate::credential_preflight::reset_forge_credential_streaks();
    }
}
