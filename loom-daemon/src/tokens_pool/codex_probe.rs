//! Live Codex rate limits via `codex app-server` (issue #9233).
//!
//! [`super::codex_check`] reads each profile's recorded rollout snapshots,
//! which are only as current as the account's last Codex turn: an account
//! nobody has used for a few days reports a stale window or none at all. This
//! module asks Codex itself — `account/read` + `account/rateLimits/read` over
//! the app-server's JSON-RPC stdio — and produces the same [`UsageSnapshot`]
//! the rest of `accounts check` already consumes, so ranking, health feedback,
//! and output are unchanged; only the source is live (`accounts check --live`).
//!
//! # Ownership (ADR-0017 Decision 1)
//!
//! A **session-managed** profile is probed **only inside its session
//! container** (`docker exec`), the same seam `accounts status` uses for its
//! in-container login probe. If no session container is running the result is
//! [`ProbeOutcome::SessionUnavailable`] — never a host-direct fallback, because
//! the container is that account's single `auth.json` owner and Codex has no
//! cross-process lock of its own (a proactive refresh from a second process can
//! race the owner's and kill the refresh chain). A host-managed profile is
//! probed host-directly with `CODEX_HOME=<profile>`.
//!
//! # Wire (codex-cli 0.156, verified in a session container 2026-09-27)
//!
//! - Newline-delimited JSON; replies **omit** `jsonrpc`; notifications carry no
//!   `id` and are skipped.
//! - The requests are written up front and stdin is held open for
//!   [`STDIN_HOLD_SECS`] (the app-server answers pipelined requests in order,
//!   then exits on EOF), so one non-interactive `sh -c` works identically on the
//!   host and through `docker exec`, whose stdin the existing seam closes.
//! - `rateLimits.primary` / `.secondary` each carry `usedPercent`,
//!   `windowDurationMins`, `resetsAt` (epoch seconds). On 0.156 the **weekly**
//!   window arrives as `primary` with `secondary: null`, so windows are filed by
//!   duration ([`super::codex_check::slot_by_duration`]), never by slot.
//! - An unused window reports `resetsAt ≈ now + window` — a window that has not
//!   started and would move on every probe. It is kept as measured headroom but
//!   with **no** reset instant ([`window_not_started`]).
//! - `-32600` / `-32601` on `account/rateLimits/read` mean the capability is
//!   absent (an older CLI) — unless `account/read` answered `account: null`,
//!   which an unauthenticated home also produces, so that is checked first.
//!
//! Nothing secret is read or emitted: no token, no email, no raw reply body.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};

use super::codex_check::{epoch_to_datetime, slot_by_duration, RateLimitWindow, UsageSnapshot};
use super::session_lifecycle::{self, ContainerRunner, ExecOutput, ProcessContainerRunner};
use crate::proc_exec::{Completion, ExecError};

/// Seconds stdin stays open after the requests are written. The app-server
/// answers in about a second; this is the ceiling before EOF ends it.
pub const STDIN_HOLD_SECS: u64 = 4;

/// Wall-clock bound for one account's probe, container or host.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// Output ceiling: the replies are a few KB; anything past this is not a probe.
const MAX_OUTPUT_BYTES: u64 = 256 * 1024;

/// A window reporting `resetsAt` within this many seconds of
/// `now + windowDurationMins` with nothing used has not started.
const NOT_STARTED_TOLERANCE_SECS: i64 = 180;

/// The whole probe as one POSIX `sh -c` program. Runs with `CODEX_HOME`
/// already set by the caller (host) or by the session container's
/// environment. stderr is discarded: it carries only diagnostics.
#[must_use]
pub fn probe_script() -> String {
    script("{}")
}

/// [`probe_script`] for the daemon's background sampling (#15292): the
/// rate-limit read asks Codex to leave out the per-credit detail rows, which
/// are never kept, so only the available count crosses the wire.
#[must_use]
pub fn background_probe_script() -> String {
    script("{\"excludeResetCreditDetails\":true}")
}

fn script(rate_limit_params: &str) -> String {
    format!(
        "(printf '%s\\n' \
         '{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{{\"clientInfo\":{{\"name\":\"loom-daemon-probe\",\"version\":\"{version}\"}}}}}}' \
         '{{\"jsonrpc\":\"2.0\",\"method\":\"initialized\",\"params\":{{}}}}' \
         '{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"account/read\",\"params\":{{}}}}' \
         '{{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"account/rateLimits/read\",\"params\":{rate}}}'; \
         sleep {hold}) | codex -s read-only -a never app-server 2>/dev/null",
        version = env!("CARGO_PKG_VERSION"),
        rate = rate_limit_params,
        hold = STDIN_HOLD_SECS,
    )
}

/// What one live probe established. Secret-free by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum ProbeOutcome {
    /// A live reading.
    Measured(UsageSnapshot),
    /// `account/read` answered `account: null`: the home is not logged in.
    NotLoggedIn,
    /// The CLI does not implement `account/rateLimits/read`.
    CapabilityAbsent,
    /// The session container is missing or stopped.
    SessionUnavailable,
    /// The runtime executable could not start.
    RuntimeUnavailable(std::io::ErrorKind),
    /// The executable started, but its inspection or execution failed.
    SessionProbeError(String),
    /// The probe ran but produced no usable answer.
    Failed(&'static str),
}

impl ProbeOutcome {
    /// The row detail `accounts check` reports for a probe that measured
    /// nothing; `None` for a measurement.
    #[must_use]
    pub fn detail(&self) -> Option<&'static str> {
        match self {
            Self::Measured(_) => None,
            Self::NotLoggedIn => Some("not_logged_in"),
            Self::CapabilityAbsent => Some("rate_limits_unsupported_by_codex_cli"),
            Self::SessionUnavailable => Some("session_unavailable"),
            Self::RuntimeUnavailable(_) => Some("container_runtime_unavailable"),
            Self::SessionProbeError(_) => Some("session_probe_error"),
            Self::Failed(why) => Some(why),
        }
    }

    pub(super) fn diagnostic(&self) -> Option<String> {
        match self {
            Self::RuntimeUnavailable(kind) => {
                let search = crate::docker_bin::search_description();
                Some(format!(
                    "container_runtime_unavailable: docker could not start ({kind:?}), {search}"
                ))
            }
            Self::SessionProbeError(category) => {
                Some(format!("session_probe_error: docker {category}"))
            }
            _ => None,
        }
    }
}

/// Keep the underlying spawn error distinct from a command that ran. Only
/// the bounded category is reported: Docker stderr may contain private data.
pub(super) fn session_error(err: &anyhow::Error, operation: &str) -> ProbeOutcome {
    use crate::proc_exec::ExecError;
    match err.downcast_ref::<ExecError>() {
        Some(ExecError::Spawn(io)) => return ProbeOutcome::RuntimeUnavailable(io.kind()),
        Some(ExecError::Collect(io)) => {
            return ProbeOutcome::SessionProbeError(format!(
                "{operation} output collection: {:?}",
                io.kind()
            ));
        }
        None => {}
    }
    if let Some(io) = err.downcast_ref::<std::io::Error>() {
        return match io.kind() {
            std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied => {
                ProbeOutcome::RuntimeUnavailable(io.kind())
            }
            kind => ProbeOutcome::SessionProbeError(format!("{operation} I/O: {kind:?}")),
        };
    }
    let kind = if err.is::<super::docker_cli::DockerTimedOut>() {
        "timeout"
    } else {
        "command failed"
    };
    ProbeOutcome::SessionProbeError(format!("{operation}: {kind}"))
}

/// How a probe program is run. Split out so tests can prove the ownership
/// rule (a session-managed profile never reaches [`ProbeTransport::host`]).
pub trait ProbeTransport {
    /// Check executable availability once before inspecting session profiles.
    fn check_runtime(&self) -> Result<()> {
        Ok(())
    }

    /// Run the probe host-directly with `CODEX_HOME=profile`.
    fn host(&self, profile: &Path) -> Result<ExecOutput>;
    /// Run the probe inside `container`. `Ok(None)` = no probe was possible
    /// (the container is missing or stopped). Spawn errors remain typed.
    fn container(&self, container: &str) -> Result<Option<ExecOutput>>;
    /// [`Self::container`] for the daemon's background sampling (#15292),
    /// which asks for reset-credit counts without their detail rows.
    fn container_background(&self, container: &str) -> Result<Option<ExecOutput>> {
        self.container(container)
    }
}

/// The real transport: `sh -c` on the host, `docker exec` via the session
/// lifecycle's own [`ProcessContainerRunner`] in a container.
pub struct ProcessProbeTransport;

impl ProbeTransport for ProcessProbeTransport {
    fn check_runtime(&self) -> Result<()> {
        // Only the spawn matters here. Inspect each container separately when
        // the executable runs, even if its version command exits nonzero.
        ProcessContainerRunner::run_capture(&["--version"]).map(|_| ())
    }

    fn host(&self, profile: &Path) -> Result<ExecOutput> {
        let mut command = Command::new("sh");
        command
            .args(["-c", &probe_script()])
            .env("CODEX_HOME", profile);
        run_bounded(command)
    }

    fn container(&self, container: &str) -> Result<Option<ExecOutput>> {
        exec_in_container(container, &probe_script())
    }

    fn container_background(&self, container: &str) -> Result<Option<ExecOutput>> {
        exec_in_container(container, &background_probe_script())
    }
}

fn exec_in_container(container: &str, script: &str) -> Result<Option<ExecOutput>> {
    let runner = ProcessContainerRunner;
    match runner.inspect(container)? {
        Some(state) if state.running => {}
        _ => return Ok(None),
    }
    let out = runner.exec_capture(container, &["sh", "-c", script], PROBE_TIMEOUT)?;
    if out.unavailable {
        // exec_capture sets this bit when spawning Docker returned ENOENT,
        // including if the executable disappeared after the initial check.
        return Err(std::io::Error::from(std::io::ErrorKind::NotFound).into());
    }
    Ok(Some(out))
}

/// Probe one account, honoring ADR-0017's ownership rule.
pub fn probe_account<T: ProbeTransport + ?Sized>(
    transport: &T,
    name: &str,
    profile: &Path,
    now: DateTime<Utc>,
) -> ProbeOutcome {
    let run = if session_lifecycle::is_session_managed(profile) {
        match transport.container(&session_lifecycle::container_name(name)) {
            Ok(Some(out)) => out,
            Ok(None) => return ProbeOutcome::SessionUnavailable,
            Err(err) => return session_error(&err, "inspect/exec"),
        }
    } else {
        match transport.host(profile) {
            Ok(out) => out,
            Err(_) => return ProbeOutcome::Failed("host_probe_error"),
        }
    };
    if run.unavailable {
        return ProbeOutcome::Failed("codex_cli_unavailable");
    }
    if run.timed_out {
        return ProbeOutcome::Failed("probe_timed_out");
    }
    parse_probe_output(&run.output, now)
}

/// Interpret the app-server's replies. Pure, for the tests.
#[must_use]
pub fn parse_probe_output(text: &str, now: DateTime<Utc>) -> ProbeOutcome {
    parse_probe_replies(text, now).outcome
}

/// What the replies establish about one seat, each part independently of the
/// others (#15292). Only verdicts and numbers: no account payload, reset-credit
/// detail row or raw reply text is kept.
#[derive(Debug, Clone, PartialEq)]
pub struct SeatReading {
    /// `account/read`'s verdict: `Some(true)` a valid account object,
    /// `Some(false)` an explicit `account: null`, `None` no usable reply.
    pub login: Option<bool>,
    /// `rateLimitResetCredits.availableCount` when the reply carries it,
    /// whatever the windows say. Older CLIs omit it.
    pub reset_credits: Option<u64>,
    /// The quota outcome, as `accounts check --live` reads it.
    pub outcome: ProbeOutcome,
}

/// [`parse_probe_output`] with the login verdict and reset-credit count kept.
/// Pure, for the tests.
#[must_use]
pub fn parse_probe_replies(text: &str, now: DateTime<Utc>) -> SeatReading {
    let mut login: Option<bool> = None;
    let mut reset_credits: Option<u64> = None;
    let mut limits: Option<serde_json::Value> = None;
    let mut capability_absent = false;
    for line in text.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        match message.get("id").and_then(serde_json::Value::as_i64) {
            Some(2) => {
                if let Some(account) = message.get("result").and_then(|r| r.get("account")) {
                    if account.is_null() {
                        login = Some(false);
                    } else if account.is_object() {
                        login = Some(true);
                    }
                }
            }
            Some(3) => {
                if let Some(code) = message
                    .get("error")
                    .and_then(|e| e.get("code"))
                    .and_then(serde_json::Value::as_i64)
                {
                    capability_absent = code == -32600 || code == -32601;
                } else if let Some(result) = message.get("result") {
                    reset_credits = result
                        .get("rateLimitResetCredits")
                        .and_then(|c| c.get("availableCount"))
                        .and_then(serde_json::Value::as_u64);
                    if let Some(rl) = result.get("rateLimits") {
                        limits = Some(rl.clone());
                    }
                }
            }
            _ => {}
        }
    }
    let outcome = quota_outcome(login == Some(false), capability_absent, limits.as_ref(), now);
    SeatReading {
        login,
        reset_credits,
        outcome,
    }
}

fn quota_outcome(
    account_null: bool,
    capability_absent: bool,
    limits: Option<&serde_json::Value>,
    now: DateTime<Utc>,
) -> ProbeOutcome {
    // An unauthenticated home answers `account: null` *and* -32600 on the
    // rate-limit call; the null account is the real finding.
    if account_null {
        return ProbeOutcome::NotLoggedIn;
    }
    if capability_absent {
        return ProbeOutcome::CapabilityAbsent;
    }
    let Some(limits) = limits else {
        return ProbeOutcome::Failed("no_rate_limit_reply");
    };
    let window = |key: &str| limits.get(key).and_then(|w| parse_live_window(w, now));
    let (primary, secondary) = slot_by_duration(window("primary"), window("secondary"));
    if primary.is_none() && secondary.is_none() {
        return ProbeOutcome::Failed("no_rate_limit_windows");
    }
    ProbeOutcome::Measured(UsageSnapshot {
        observed_at: now,
        primary,
        secondary,
    })
}

/// One bounded in-container round trip for a session seat's background
/// sampling (#15292): login, live limits and the reset-credit count from one
/// `codex app-server`. Never host-direct: the container owns the seat's
/// `auth.json` (ADR-0017). `None` when no reading was possible (container
/// missing or stopped, docker or the CLI unavailable, the probe timed out).
pub fn probe_seat<T: ProbeTransport + ?Sized>(
    transport: &T,
    container: &str,
    now: DateTime<Utc>,
) -> Option<SeatReading> {
    let run = transport.container_background(container).ok()??;
    if run.unavailable || run.timed_out {
        return None;
    }
    Some(parse_probe_replies(&run.output, now))
}

fn parse_live_window(value: &serde_json::Value, now: DateTime<Utc>) -> Option<RateLimitWindow> {
    let used_percent = value
        .get("usedPercent")
        .and_then(serde_json::Value::as_f64)?;
    if !used_percent.is_finite() || used_percent < 0.0 {
        return None;
    }
    let window_minutes = value
        .get("windowDurationMins")
        .and_then(serde_json::Value::as_u64);
    let mut resets_at = value
        .get("resetsAt")
        .and_then(serde_json::Value::as_i64)
        .and_then(epoch_to_datetime);
    if window_not_started(used_percent, window_minutes, resets_at, now) {
        resets_at = None;
    }
    Some(RateLimitWindow {
        used_fraction: (used_percent / 100.0).min(1.0),
        window_minutes,
        resets_at,
    })
}

/// An unused window whose reset is `now + its own length`: it starts at first
/// use, so its "reset" is not an instant anything will happen at.
#[must_use]
pub fn window_not_started(
    used_percent: f64,
    window_minutes: Option<u64>,
    resets_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    let (Some(minutes), Some(reset)) = (window_minutes, resets_at) else {
        return false;
    };
    let Ok(minutes) = i64::try_from(minutes) else {
        return false;
    };
    let expected = now + chrono::Duration::minutes(minutes);
    used_percent == 0.0 && (reset - expected).num_seconds().abs() <= NOT_STARTED_TOLERANCE_SECS
}

/// Run a host command to completion under [`PROBE_TIMEOUT`].
fn run_bounded(command: Command) -> Result<ExecOutput> {
    run_bounded_within(command, PROBE_TIMEOUT)
}

fn run_bounded_within(mut command: Command, timeout: Duration) -> Result<ExecOutput> {
    command.stdin(Stdio::null());
    let output = match crate::proc_exec::run_bounded(command, timeout) {
        Ok(Completion::Exited(output)) => ExecOutput {
            success: output.status.success(),
            unavailable: false,
            timed_out: false,
            exit_code: output.status.code(),
            output: probe_text(&output.stdout),
        },
        Ok(Completion::TimedOut { stdout, .. }) => ExecOutput {
            success: false,
            unavailable: false,
            timed_out: true,
            exit_code: None,
            output: probe_text(&stdout),
        },
        Err(ExecError::Spawn(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            ExecOutput {
                success: false,
                unavailable: true,
                timed_out: false,
                exit_code: None,
                output: String::new(),
            }
        }
        Err(ExecError::Spawn(error) | ExecError::Collect(error)) => return Err(error.into()),
    };
    Ok(output)
}

fn probe_text(bytes: &[u8]) -> String {
    let cut = bytes.len().min(MAX_OUTPUT_BYTES as usize);
    String::from_utf8_lossy(&bytes[..cut]).into_owned()
}

#[cfg(test)]
mod tests;
