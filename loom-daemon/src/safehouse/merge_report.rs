//! The merged-PR line and its delivery rule (#13946).
//!
//! One slim line per merged PR goes to the fleet room. Delivery is **best
//! effort** (operator ruling 2026-11-25, on #13946): the line is a cosmetic
//! heartbeat, the dashboard and telemetry are the record of merges, and neither
//! a missing line nor an occasional duplicate under a race or a Matrix outage
//! matters. Safehouse's transport is unchanged, so there is no caller-supplied
//! transaction key to make a retry idempotent. The rule instead is:
//!
//! 1. **Post with the existing `send` op.**
//! 2. If the reply is **lost or times out**, the outcome is unknown. **Read the
//!    recent fleet-room messages with the existing `read` op and re-send only
//!    when that PR's line is absent.** A failed read-back means "still
//!    unknown": nothing is re-sent.
//! 3. A refusal (`ok:false`) or a failed connect means the line was **not**
//!    sent; the caller decides whether to try again later.
//!
//! The identity of a report is `owner/repo#PR`, matched in the message text:
//! no new claim kind, event type or transaction key. `owner/repo` comes from
//! the PR's own URL, so the same PR number in two repositories is two reports.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::merge_source::MergedPr;
use super::{
    set_state, Envelope, RecentMessages, SafehouseClient, SafehouseConfig, SafehouseState,
    SendError, SharedSafehouseState,
};

/// How many recent room messages the read-back inspects (safehoused caps
/// `read` at 100). Older lines can fall outside the returned history.
pub(super) const READ_BACK_LIMIT: u32 = 100;

/// Bound on one safehoused round trip (connect + `hello`, `send`, or `read`).
/// A reply that has not arrived by then is treated as lost.
pub(super) const DEFAULT_RPC_TIMEOUT: Duration = Duration::from_secs(15);

/// Where and how to reach the fleet room. Cheap to clone; every operation opens
/// its own connection (a merge is rare, so there is no long-lived socket to
/// keep healthy), and the outcome of each connect is reflected in the shared
/// state cell `loom-daemon status` reads.
#[derive(Clone)]
pub(super) struct ReportTarget {
    pub(super) socket: PathBuf,
    pub(super) persona: String,
    /// The fleet room; `None` is valid only when safehoused joined exactly one.
    pub(super) room: Option<String>,
    pub(super) rpc_timeout: Duration,
    pub(super) state: SharedSafehouseState,
}

impl ReportTarget {
    pub(super) fn new(
        config: &SafehouseConfig,
        socket: PathBuf,
        state: SharedSafehouseState,
    ) -> Self {
        Self {
            socket,
            persona: config.persona.clone(),
            room: config.signal_room().map(ToOwned::to_owned),
            rpc_timeout: DEFAULT_RPC_TIMEOUT,
            state,
        }
    }

    pub(super) async fn connect(&self) -> Result<SafehouseClient> {
        let attempt = tokio::time::timeout(
            self.rpc_timeout,
            SafehouseClient::connect(&self.socket, &self.persona),
        )
        .await
        .unwrap_or_else(|_| Err(anyhow::anyhow!("timed out connecting to safehoused")));
        // A handshake (or a failed reconnect) does not establish that the room
        // will accept a send. Keep the last rejection until an acknowledgement.
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !matches!(*state, SafehouseState::SendRejected { .. }) {
            *state = if attempt.is_ok() {
                self.connected()
            } else {
                SafehouseState::Unreachable {
                    socket: self.socket.clone(),
                }
            };
        }
        attempt
    }

    fn connected(&self) -> SafehouseState {
        SafehouseState::Connected {
            socket: self.socket.clone(),
            room: self.room.clone(),
        }
    }
}

/// One merged PR, ready to be said in the room.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PrReport {
    /// Forge `owner/repo`.
    slug: String,
    pub(super) merged_at: DateTime<Utc>,
    number: u32,
    url: String,
    title: Option<String>,
}

/// The longest PR title the line carries, in characters.
const MAX_TITLE_CHARS: usize = 100;

impl PrReport {
    /// The report for `pr`; `None` when its repository cannot be named (the
    /// identity is `owner/repo#N`).
    pub(super) fn from_merged(pr: &MergedPr) -> Option<Self> {
        Some(Self {
            slug: pr.repo_slug()?,
            merged_at: pr.merged_at,
            number: pr.number,
            url: pr.url.clone(),
            title: pr.title.as_deref().map(one_line).filter(|t| !t.is_empty()),
        })
    }

    /// `owner/repo#N`, the report's identity.
    pub(super) fn key(&self) -> String {
        format!("{}#{}", self.slug, self.number)
    }

    /// The text of the room line: `merged ✓ owner/repo#N · <title> · <url>`.
    pub(super) fn body(&self) -> String {
        match &self.title {
            Some(title) => format!("{} · {title} · {}", self.prefix(), self.url),
            None => format!("{} · {}", self.prefix(), self.url),
        }
    }

    fn envelope(&self) -> Envelope {
        Envelope {
            to: "*".to_owned(),
            kind: "chat".to_owned(),
            task_id: None,
            body: self.body(),
        }
    }

    /// What every line for this PR starts with.
    fn prefix(&self) -> String {
        format!("merged ✓ {}", self.key())
    }

    /// Whether any of `bodies` is a report of this PR. Only a line that *starts*
    /// with this report's prefix counts — a person mentioning the PR is not a
    /// report — and `#12` must not match `#123`.
    pub(super) fn is_reported_in(&self, bodies: &[String]) -> bool {
        let prefix = self.prefix();
        bodies.iter().any(|body| {
            body.trim_start()
                .strip_prefix(prefix.as_str())
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_digit()))
        })
    }
}

/// `text` collapsed onto one line and capped at [`MAX_TITLE_CHARS`].
fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= MAX_TITLE_CHARS {
        return flat;
    }
    let head: String = flat.chars().take(MAX_TITLE_CHARS).collect();
    format!("{head}…")
}

/// How one report ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Outcome {
    /// The line was sent and acknowledged (or, after a lost reply, was absent
    /// from the room and was re-sent once successfully).
    Posted,
    /// The room already holds this PR's line (found by the read-back), so
    /// nothing was sent.
    AlreadyReported,
    /// This attempt definitely did not deliver a line.
    Failed(String),
    /// A send may have landed. Any later attempt must read before sending.
    Uncertain(String),
}

async fn attempt_send(target: &ReportTarget, envelope: &Envelope) -> Outcome {
    let mut client = match target.connect().await {
        Ok(client) => client,
        Err(err) => return Outcome::Failed(format!("{err:#}")),
    };
    let sent =
        tokio::time::timeout(target.rpc_timeout, client.send_to(envelope, target.room.as_deref()))
            .await;
    match sent {
        Ok(Ok(_event_id)) => {
            set_state(&target.state, target.connected());
            Outcome::Posted
        }
        Ok(Err(SendError::Rejected { reason })) => {
            set_state(
                &target.state,
                SafehouseState::SendRejected {
                    socket: target.socket.clone(),
                    reason: reason.clone(),
                },
            );
            Outcome::Failed(format!("safehoused rejected the send: {reason}"))
        }
        Ok(Err(SendError::Transport(err))) => Outcome::Uncertain(format!("{err:#}")),
        Err(_elapsed) => Outcome::Uncertain("no reply before the timeout".to_owned()),
    }
}

/// The recent messages of the fleet room, oldest first (the read-back). `Err`
/// carries a reason when the room could not be read.
pub(super) async fn read_room(
    target: &ReportTarget,
) -> std::result::Result<RecentMessages, String> {
    let mut client = target.connect().await.map_err(|err| format!("{err:#}"))?;
    let read = tokio::time::timeout(
        target.rpc_timeout,
        client.read_recent(target.room.as_deref(), READ_BACK_LIMIT),
    )
    .await;
    match read {
        Ok(Ok(bodies)) => Ok(bodies),
        Ok(Err(err)) => Err(err.to_string()),
        Err(_elapsed) => Err("no reply before the timeout".to_owned()),
    }
}

/// Post `report` to the fleet room, reading the room back before any retry.
/// See the module docs for the rule. At most two `send`s are made.
pub(super) async fn post(target: &ReportTarget, report: &PrReport) -> Outcome {
    let envelope = report.envelope();
    let started = Utc::now();
    match attempt_send(target, &envelope).await {
        Outcome::Uncertain(_) => post_after_read(target, report, started).await,
        outcome => outcome,
    }
}

/// Resolve a previous ambiguous send. One fresh, complete negative read
/// authorizes exactly one send, even if this attempt also loses its reply.
pub(super) async fn post_after_read(
    target: &ReportTarget,
    report: &PrReport,
    started: DateTime<Utc>,
) -> Outcome {
    match read_room(target).await {
        Ok(recent) if report.is_reported_in(&recent.bodies) => Outcome::AlreadyReported,
        // An empty page supplies no historical coverage, but permits one
        // best-effort retry of this specific send after a readable response.
        Ok(recent) if recent.covers(started) || (recent.complete && recent.bodies.is_empty()) => {
            attempt_send(target, &report.envelope()).await
        }
        Ok(_) => Outcome::Uncertain("read-back incomplete".to_owned()),
        Err(why) => Outcome::Uncertain(format!("read-back failed: {why}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn merged(number: u32, url: &str, title: Option<&str>) -> MergedPr {
        MergedPr {
            number,
            url: url.to_owned(),
            title: title.map(ToOwned::to_owned),
            merged_at: Utc.with_ymd_and_hms(2026, 11, 25, 10, 0, 0).unwrap(),
        }
    }

    fn report(number: u32, slug: &str) -> PrReport {
        PrReport::from_merged(&merged(
            number,
            &format!("https://github.com/{slug}/pull/{number}"),
            Some("Retire narration"),
        ))
        .unwrap()
    }

    #[test]
    fn the_line_names_the_pr_and_links_it() {
        let r = report(13946, "2AMLogic/loom");
        assert_eq!(r.key(), "2AMLogic/loom#13946");
        assert_eq!(
            r.body(),
            "merged ✓ 2AMLogic/loom#13946 · Retire narration · https://github.com/2AMLogic/loom/pull/13946"
        );
    }

    #[test]
    fn a_pr_without_a_title_still_reports() {
        let r = PrReport::from_merged(&merged(3, "https://github.com/a/b/pull/3", None)).unwrap();
        assert_eq!(r.body(), "merged ✓ a/b#3 · https://github.com/a/b/pull/3");
    }

    #[test]
    fn a_long_multiline_title_is_flattened_and_capped() {
        let long = format!("first line\n  second   line {}", "x".repeat(200));
        let r = PrReport::from_merged(&merged(3, "https://github.com/a/b/pull/3", Some(&long)))
            .unwrap();
        let body = r.body();
        assert!(!body.contains('\n'));
        assert!(body.contains("first line second line"));
        assert!(body.contains('…'));
    }

    #[test]
    fn a_pr_whose_repo_cannot_be_named_is_not_reported() {
        assert!(PrReport::from_merged(&merged(3, "not a url", Some("t"))).is_none());
    }

    #[test]
    fn the_report_matches_its_own_line_and_no_other() {
        let r = report(12, "a/b");
        let line = r.body();
        assert!(r.is_reported_in(std::slice::from_ref(&line)));
        assert!(r.is_reported_in(&["chatter".to_owned(), format!("  {line}")]));
        // #12 is not #123, and another repo's #12 is not this one.
        assert!(!r.is_reported_in(&[report(123, "a/b").body()]));
        assert!(!r.is_reported_in(&[report(12, "a/c").body()]));
        assert!(!r.is_reported_in(&[report(12, "xa/b").body()]));
    }

    #[test]
    fn a_person_mentioning_the_pr_is_not_a_report() {
        let r = report(12, "a/b");
        assert!(!r.is_reported_in(&["did a/b#12 land?".to_owned()]));
        assert!(!r.is_reported_in(&[]));
    }
}
