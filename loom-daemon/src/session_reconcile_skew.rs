//! Daemon-version coupling for the session reconcile pass (issue #11727
//! Slice 2).
//!
//! A session container keeps the `loom-daemon` of the image it was started
//! from. When the host daemon rolls and a host-side hook starts calling a
//! subcommand the container lacks, every Codex role run there fails. This
//! module heals that: a running container whose daemon is **older** than the
//! host's is recreated once idle, from the session image pinned to the host
//! version (`<repo>:<host version>`, never `:latest`), and no start or
//! recreate may use an image whose tag is older than the running container's
//! daemon or the host's (no downgrade).
//!
//! # Safety rules (the mount-drift rules of [`super::drift`], reused)
//!
//! 1. **Act only on a positive finding.** The host version must have been
//!    read (`PassInputs::host_version`), the container's version must parse
//!    from `docker exec <c> loom-daemon --version`, and the container's must
//!    be strictly lower. Anything unreadable, equal, or newer: no action.
//! 2. **Idle only.** A fresh inspect must show the same running container, no
//!    `docker exec` may be in flight, and the dispatch lock must be taken
//!    exclusively (held through the recreate). Busy: deferred, re-checked
//!    next pass. Never killed.
//! 3. **One attempt per (container version, host version).** A recreate that
//!    leaves the container still skewed is not retried until a version
//!    changes, so a broken recreate cannot churn an idle seat every pass.
//!    A pull that fails **before** the teardown has churned nothing: the seat
//!    is intact and the pinned tag is usually a release workflow that has not
//!    finished (the session image follows the tag by ~25 minutes), so the
//!    pull is retried with a doubling pass backoff
//!    ([`PULL_RETRY_MAX_SKIP`] passes at most) until it succeeds or a version
//!    changes (#13493).
//! 4. **Private-clone sessions** are never version-probed and never recreated
//!    host-mounted, and an account whose mode cannot be ruled out is never
//!    touched; the host-mounted path also needs a readable workspace
//!    registry. A running private-clone seat converges by **image identity**
//!    instead (`reconcile_private_image`, #14112): a seat on the stock
//!    image whose image is not the daemon's release image — a `:latest` seat
//!    a roll left behind — is rolled onto it once idle, under the seat's
//!    exclusive account lease (a job using it defers the roll; nothing is
//!    ever interrupted), with the same no-downgrade, one-attempt-per-pair
//!    and pull-backoff rules. An override seat, or one on an image an
//!    operator named explicitly, is never replaced.
//!
//! Versions are compared numerically per component (`0.19.624 < 0.19.1151`),
//! never as strings.

use std::cmp::Ordering;
use std::time::Duration;

use serde_json::Value;

use super::drift::DeferReason;
use super::{recreate_container, AccountMemory, Outcome, PassInputs};
use crate::tokens_pool::account_registry::AccountDescriptor;
use crate::tokens_pool::private_workspace::{AccountBusy, ImageUnavailable, Rolled};
use crate::tokens_pool::session_dispatch_lock::{self, Exclusive};
use crate::tokens_pool::session_hold::OperatorHeld;
use crate::tokens_pool::session_lifecycle::{
    container_name, image_reference, release_session_image, ContainerRunner, SessionLifecycle,
    UndoLock, STOP_GRACE,
};
use crate::tokens_pool::session_state::{container_running, is_private_clone, workspace_label};

/// How long the `loom-daemon --version` probe may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// A dotted numeric daemon version (`0.19.1151`).
#[derive(Debug, Clone)]
pub struct DaemonVersion(Vec<u64>);

impl DaemonVersion {
    /// Parse `text` as exactly a version (`0.19.1151`, optionally followed by
    /// a `-`/`+` suffix). At least two components, so a commit hash is never
    /// mistaken for one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let core = text.split(['-', '+']).next()?;
        let parts: Vec<u64> = core
            .split('.')
            .map(|p| if p.is_empty() { None } else { p.parse().ok() })
            .collect::<Option<_>>()?;
        (parts.len() >= 2).then_some(Self(parts))
    }

    /// The first version-looking token of `--version` output such as
    /// `loom-daemon 0.19.1152 (commit 2d5fe17, built …)`.
    #[must_use]
    pub fn from_version_output(output: &str) -> Option<Self> {
        output
            .split_whitespace()
            .filter(|t| t.starts_with(|c: char| c.is_ascii_digit()))
            .find_map(Self::parse)
    }

    /// The version a session image tag names (`repo:0.19.1151`). `None` for
    /// `:latest`, an untagged name, or a digest pin: they say nothing.
    #[must_use]
    pub fn from_image(image: &str) -> Option<Self> {
        let (_, tag) = image_repo_and_tag(image);
        Self::parse(tag?)
    }
}

impl PartialEq for DaemonVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for DaemonVersion {}

impl Ord for DaemonVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let n = self.0.len().max(other.0.len());
        (0..n)
            .map(|i| {
                let a = self.0.get(i).copied().unwrap_or(0);
                let b = other.0.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for DaemonVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for DaemonVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u64::to_string).collect();
        f.write_str(&parts.join("."))
    }
}

/// `(repository, tag)` of an image reference; a digest pin has no tag, and a
/// registry port (`host:5000/img`) is not a tag.
///
/// A reference may carry both a tag and a digest (`repo:0.19.1209@sha256:…`,
/// which is what `docker inspect` and the last-start sidecar record for a
/// pulled-by-tag image). The repository is still everything before the tag:
/// returning `repo:0.19.1209` as the repository made [`pinned_image`] build
/// `repo:0.19.1209:<host>`, an invalid reference `docker pull` refuses, so a
/// skewed seat was never recreated (#13479). The tag is still reported as
/// `None` for a digest pin: the digest, not the tag, names the image, so it
/// says nothing [`refuse_downgrade`] may act on.
fn image_repo_and_tag(image: &str) -> (&str, Option<&str>) {
    let name = image.split('@').next().unwrap_or(image);
    let digest_pinned = name.len() != image.len();
    match name.rsplit_once(':') {
        Some((repo, tag)) if !tag.contains('/') => {
            (repo, if digest_pinned { None } else { Some(tag) })
        }
        _ => (name, None),
    }
}

/// `image` re-pinned to `host`: same repository, tag = the host version.
#[must_use]
pub fn pinned_image(image: &str, host: &DaemonVersion) -> String {
    format!("{}:{host}", image_repo_and_tag(image).0)
}

/// No downgrade: `image` is refused when its tag names a version older than
/// any of `floors` (the running container's daemon, the host's). An image
/// whose tag names no version (`:latest`, a digest) cannot be judged and is
/// allowed.
///
/// # Errors
/// The image is older than a floor.
pub fn refuse_downgrade(image: &str, floors: &[&DaemonVersion]) -> anyhow::Result<()> {
    let Some(tag) = DaemonVersion::from_image(image) else {
        return Ok(());
    };
    match floors.iter().find(|floor| tag < ***floor) {
        Some(floor) => anyhow::bail!(
            "image {image} carries loom-daemon {tag}, older than {floor}; refusing the downgrade"
        ),
        None => Ok(()),
    }
}

/// [`refuse_downgrade`] against this pass's host version (a no-op when the
/// host version was not read).
///
/// # Errors
/// The image is older than the host's daemon.
pub fn guard_image(inputs: &PassInputs<'_>, image: Option<&str>) -> anyhow::Result<()> {
    match (inputs.host_version.as_ref(), image) {
        (Some(host), Some(image)) => refuse_downgrade(image, &[host]),
        _ => Ok(()),
    }
}

/// Longest run of passes skipped between two pulls of a pinned image that is
/// not available yet: 30 passes is half an hour at the default interval.
pub const PULL_RETRY_MAX_SKIP: u32 = 30;

/// What the pass remembers about one account's version skew.
#[derive(Debug, Default, Clone)]
pub(super) struct SkewMemory {
    /// The container id and daemon version already probed (a running
    /// container's daemon cannot change).
    probed: Option<(String, DaemonVersion)>,
    /// The `(container, host)` versions a recreate was already attempted for.
    attempted: Option<(DaemonVersion, DaemonVersion)>,
    deferred: Option<DeferReason>,
    /// A pinned image the pass could not pull for `(container, host)`: how
    /// many pulls failed and how many passes remain to skip before the next.
    pull: Option<PullRetry>,
    /// A private-clone seat's image roll (#14112).
    private: PrivateImageMemory,
}

/// What the pass remembers about one private-clone seat's image (#14112).
#[derive(Debug, Default, Clone)]
pub(super) struct PrivateImageMemory {
    /// The `(running, expected)` images a roll was already attempted — or
    /// declined — for: not again until one of them changes (rule 3).
    attempted: Option<(String, String)>,
    /// The expected image the roll could not obtain, and its pull backoff.
    pull: Option<PullRetry>,
}

/// Backoff state for an image that is not available yet (rule 3), for
/// whichever `(from, to)` pair — daemon versions or image references —
/// asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PullRetry {
    key: (String, String),
    failures: u32,
    skip: u32,
}

impl PullRetry {
    /// The run of passes to skip after `failures` failed pulls: 1, 2, 4, …
    /// capped at [`PULL_RETRY_MAX_SKIP`].
    pub(super) fn skip_after(failures: u32) -> u32 {
        1u32.checked_shl(failures.saturating_sub(1))
            .unwrap_or(PULL_RETRY_MAX_SKIP)
            .min(PULL_RETRY_MAX_SKIP)
    }
}

/// Whether this pass should pull the image `key` asks for or sit out the
/// backoff. Counts the pass down when it sits out.
fn pull_due(retry: &mut Option<PullRetry>, key: &(String, String)) -> bool {
    match retry {
        Some(retry) if &retry.key == key && retry.skip > 0 => {
            retry.skip -= 1;
            false
        }
        _ => true,
    }
}

/// A pull of the image `key` asks for failed: schedule the next one.
fn note_pull_failure(retry: &mut Option<PullRetry>, key: (String, String)) -> u32 {
    let failures = match retry {
        Some(retry) if retry.key == key => retry.failures + 1,
        _ => 1,
    };
    let skip = PullRetry::skip_after(failures);
    *retry = Some(PullRetry {
        key,
        failures,
        skip,
    });
    skip
}

fn probe<R: ContainerRunner>(
    lifecycle: &SessionLifecycle<R>,
    container: &str,
    id: &str,
    mem: &mut SkewMemory,
) -> Option<DaemonVersion> {
    if let Some((cached_id, version)) = &mem.probed {
        if cached_id == id {
            return Some(version.clone());
        }
    }
    let out = match lifecycle.runner().exec_capture(
        container,
        &["loom-daemon", "--version"],
        PROBE_TIMEOUT,
    ) {
        Ok(out) if out.success && !out.unavailable && !out.timed_out => out,
        Ok(out) => {
            log::debug!(
                "session_reconcile: {container}: `loom-daemon --version` did not answer \
                 (exit {:?}); no version-skew decision",
                out.exit_code
            );
            return None;
        }
        Err(error) => {
            log::debug!("session_reconcile: {container}: version probe failed ({error:#})");
            return None;
        }
    };
    let version = DaemonVersion::from_version_output(&out.output)?;
    mem.probed = Some((id.to_string(), version.clone()));
    Some(version)
}

/// Report a step that found something to do but may not act this pass: INFO
/// the first time a given reason defers it, DEBUG while the same reason
/// stands. `found` names the finding; `outcome` is the variant to report.
fn defer(
    container: &str,
    reason: DeferReason,
    found: &str,
    detail: &str,
    mem: &mut SkewMemory,
    outcome: fn(DeferReason) -> Outcome,
) -> Outcome {
    if mem.deferred == Some(reason) {
        log::debug!("session_reconcile: {container}: {found}; still deferred ({reason:?})");
    } else {
        log::info!("session_reconcile: {container}: {found}; {detail}");
    }
    mem.deferred = Some(reason);
    outcome(reason)
}

const HOST_SKEW: &str = "daemon older than the host's";
const PRIVATE_IMAGE: &str = "image is not this daemon's release image";

fn skew_deferred(reason: DeferReason) -> Outcome {
    Outcome::SkewDeferred { reason }
}

fn image_deferred(reason: DeferReason) -> Outcome {
    Outcome::PrivateImageDeferred { reason }
}

/// The version-skew step for a healthy running container. `None`: nothing to
/// do (no positive finding, or nothing may be decided this pass).
#[allow(clippy::too_many_lines)]
pub(super) fn reconcile_skew<R: ContainerRunner>(
    lifecycle: &mut SessionLifecycle<R>,
    account: &AccountDescriptor,
    inspect: &Value,
    inputs: &PassInputs<'_>,
    account_mem: &mut AccountMemory,
) -> anyhow::Result<Option<Outcome>> {
    let name = account.id.name.as_str();
    let container = container_name(name);
    let Some(host) = inputs.host_version.as_ref() else {
        return Ok(None);
    };
    if !container_running(inspect) {
        return Ok(None);
    }
    // A private-clone seat converges by image identity, not by version probe.
    if is_private_clone(inspect) {
        return reconcile_private_image(account, inspect, inputs, account_mem, host);
    }
    let (Some(label), Some(_)) = (workspace_label(inspect), inputs.registered) else {
        return Ok(None);
    };
    let id = inspect["Id"].as_str().unwrap_or_default();
    let image = inspect["Config"]["Image"].as_str().map(str::to_string);
    let mem = &mut account_mem.skew;
    let Some(have) = probe(lifecycle, &container, id, mem) else {
        return Ok(None);
    };
    if have >= *host {
        *mem = SkewMemory {
            probed: mem.probed.take(),
            ..SkewMemory::default()
        };
        return Ok(None);
    }
    if mem.attempted.as_ref() == Some(&(have.clone(), host.clone())) {
        log::debug!("session_reconcile: {container}: skew unchanged since the last recreate");
        return Ok(None);
    }
    // An override seat is never recreated for skew (issue #14019): its
    // digest-pinned image carries the daemon it was built on, so recreating
    // from it would reproduce the same skew, and re-pinning its repository
    // to the host version would fabricate a tag the operator's repository
    // does not have. The seat stays on its running container; the fix is
    // rebuilding the derived image on the new base and updating the digest
    // pin, then a stop and start. Never a silent fall back to the stock tag.
    if inputs
        .session_config
        .is_some_and(crate::tokens_pool::session_image::SessionImageConfig::has_image_override)
    {
        log::warn!(
            "session_reconcile: {container}: its loom-daemon {have} is older than the host's \
             {host}, but the seat is configured for an override image (accounts.session.image); \
             not recreating it. Rebuild the derived image FROM the base session image for \
             {host}, update the digest pin, then stop and start the seat."
        );
        mem.attempted = Some((have, host.clone()));
        return Ok(None);
    }
    if !pull_due(&mut mem.pull, &(have.to_string(), host.to_string())) {
        log::debug!(
            "session_reconcile: {container}: pinned image still backing off after a failed pull"
        );
        return Ok(None);
    }
    if !matches!((inputs.is_private_clone)(account), Ok(false)) {
        return Ok(None);
    }
    let (workspace, recorded_image) = match inputs.index.last_start(name) {
        Some(start) => (start.workspace, Some(start.image)),
        None => (label.to_path_buf(), image.clone()),
    };
    let Some(base) = recorded_image.or(image) else {
        return Ok(None);
    };
    let pinned = pinned_image(&base, host);
    if let Err(error) = refuse_downgrade(&pinned, &[&have, host]) {
        log::warn!("session_reconcile: {container}: not recreating for skew: {error:#}");
        return Ok(None);
    }
    if workspace.parent().is_none() || (inputs.would_create_accept)(&workspace).is_err() {
        log::warn!(
            "session_reconcile: {container}: daemon {have} is older than the host's {host}, but \
             a recreate against {} would be refused; left running",
            workspace.display()
        );
        mem.attempted = Some((have, host.clone()));
        return Ok(None);
    }
    match lifecycle.runner().inspect(&container)? {
        Some(now) if now.running && now.id == id => {}
        _ => {
            return Ok(Some(defer(
                &container,
                DeferReason::Changed,
                HOST_SKEW,
                "it changed since the read",
                mem,
                skew_deferred,
            )))
        }
    }
    if lifecycle.runner().has_active_exec(&container)? {
        let detail = "an in-flight `docker exec`; deferring and re-checking next pass";
        return Ok(Some(defer(
            &container,
            DeferReason::Busy,
            HOST_SKEW,
            detail,
            mem,
            skew_deferred,
        )));
    }
    let is_held = || inputs.index.is_held(name);
    if is_held() {
        return Err(OperatorHeld.into());
    }
    let _teardown = match session_dispatch_lock::try_exclusive(inputs.dispatch_locks, &container) {
        Exclusive::Acquired(lock) => lock,
        Exclusive::Busy => {
            let detail = "a dispatch is starting or running; deferring and re-checking next pass";
            return Ok(Some(defer(
                &container,
                DeferReason::Busy,
                HOST_SKEW,
                detail,
                mem,
                skew_deferred,
            )));
        }
        Exclusive::Unknown(why) => {
            let detail = format!("the dispatch lock is unusable ({why}); not stopping it");
            return Ok(Some(defer(
                &container,
                DeferReason::LockUnknown,
                HOST_SKEW,
                &detail,
                mem,
                skew_deferred,
            )));
        }
    };
    mem.deferred = None;
    // The pinned tag must be on hand before the teardown: `docker run` would
    // pull it implicitly, but only after the seat is already gone. A failed
    // pull leaves the container running on its old daemon, and the seat has
    // not been touched, so it is retried after a backoff (rule 3, #13493):
    // the tag is usually a release workflow that has not published the
    // session image yet.
    if let Err(error) = lifecycle.runner().ensure_image(&pinned) {
        let skip = note_pull_failure(&mut mem.pull, (have.to_string(), host.to_string()));
        let failures = mem.pull.as_ref().map_or(1, |r| r.failures);
        if failures == 1 {
            log::warn!(
                "session_reconcile: {container}: not recreating for skew: {pinned} is not \
                 available ({error:#}); left running, pull retried after {skip} pass(es)"
            );
        } else {
            log::debug!(
                "session_reconcile: {container}: {pinned} still not available (pull {failures} \
                 failed: {error:#}); next pull after {skip} pass(es)"
            );
        }
        return Ok(None);
    }
    // Recorded before the teardown: a failure past this point is not retried
    // until a version changes (rule 3).
    mem.attempted = Some((have.clone(), host.clone()));
    mem.pull = None;
    lifecycle.runner().stop_and_remove(&container, STOP_GRACE)?;
    let undo = UndoLock::HeldByCaller;
    recreate_container(lifecycle, name, &workspace, Some(pinned.clone()), &is_held, undo)?;
    account_mem.awaiting_confirm = true;
    account_mem.skew.probed = None;
    log::warn!(
        "session_reconcile: {container}: its loom-daemon {have} is older than the host's {host}; \
         recreated it from {pinned} (--mount-workspace {})",
        workspace.display()
    );
    Ok(Some(Outcome::SkewRecreated {
        workspace,
        from: have,
        to: host.clone(),
    }))
}

/// The image-convergence step for a running private-clone seat (#14112). Its
/// container is never version-probed: what decides is the image it was
/// created from, since that is the daemon and control bundle it carries.
/// `None`: nothing to do this pass.
///
/// Only a session positively known to be configured private is acted on. A
/// seat on the stock image is expected on the daemon's release image; the
/// roll itself ([`crate::tokens_pool::private_workspace::roll`]) keeps an
/// override seat, or one on an image an operator named explicitly, as it is.
/// The host path's rules apply: idle only (the roll holds the seat's exclusive
/// account lease, so a job using it defers it and nothing is interrupted), no
/// downgrade, one attempt per `(running, expected)` pair, and a pull that
/// fails before any teardown retried on a doubling backoff.
#[allow(clippy::too_many_lines)]
fn reconcile_private_image(
    account: &AccountDescriptor,
    inspect: &Value,
    inputs: &PassInputs<'_>,
    account_mem: &mut AccountMemory,
    host: &DaemonVersion,
) -> anyhow::Result<Option<Outcome>> {
    let name = account.id.name.as_str();
    let container = container_name(name);
    if !matches!((inputs.is_private_clone)(account), Ok(true)) {
        return Ok(None);
    }
    let Some(running) = inspect["Config"]["Image"].as_str().map(image_reference) else {
        return Ok(None);
    };
    let release = release_session_image(&host.to_string());
    let mem = &mut account_mem.skew;
    if running == release {
        mem.private = PrivateImageMemory::default();
        return Ok(None);
    }
    let pair = (running.to_owned(), release.clone());
    if mem.private.attempted.as_ref() == Some(&pair) {
        log::debug!(
            "session_reconcile: {container}: image roll already attempted for {running} -> {release}"
        );
        return Ok(None);
    }
    // No downgrade: a seat on a newer release than this daemon's stays there.
    if DaemonVersion::from_image(running).is_some_and(|running| running > *host) {
        log::warn!(
            "session_reconcile: {container}: runs {running}, newer than this daemon's release \
             {host}; not downgrading it"
        );
        mem.private.attempted = Some(pair);
        return Ok(None);
    }
    if !pull_due(&mut mem.private.pull, &pair) {
        log::debug!(
            "session_reconcile: {container}: release image still backing off after a failed pull"
        );
        return Ok(None);
    }
    let is_held = || inputs.index.is_held(name);
    let rolled = match (inputs.roll_private)(account, &release, &is_held) {
        Ok(rolled) => rolled,
        Err(error) if error.downcast_ref::<AccountBusy>().is_some() => {
            let detail =
                "a job or worker is using it; deferring its image roll and re-checking next pass";
            return Ok(Some(defer(
                &container,
                DeferReason::Busy,
                PRIVATE_IMAGE,
                detail,
                mem,
                image_deferred,
            )));
        }
        Err(error) if error.downcast_ref::<OperatorHeld>().is_some() => return Err(error),
        Err(error) if error.downcast_ref::<ImageUnavailable>().is_some() => {
            let skip = note_pull_failure(&mut mem.private.pull, pair);
            if mem
                .private
                .pull
                .as_ref()
                .is_some_and(|retry| retry.failures == 1)
            {
                log::warn!(
                    "session_reconcile: {container}: not rolling it onto {release}: {error:#}; \
                     left running, pull retried after {skip} pass(es)"
                );
            } else {
                log::debug!(
                    "session_reconcile: {container}: {release} still not available ({error:#}); \
                     next pull after {skip} pass(es)"
                );
            }
            return Ok(None);
        }
        Err(error) => {
            // Recorded before reporting: a recreate that fails past the
            // roll's own checks is not attempted again until an image changes
            // (rule 3), and must not have churned the seat every pass.
            mem.private.attempted = Some(pair);
            mem.private.pull = None;
            // If the teardown had begun, whatever brings the seat back next
            // must not be the image it was leaving: the next sighting of the
            // old container overwrites this, a missing one recovers on it.
            account_mem.image = Some(release);
            return Err(error);
        }
    };
    mem.private.pull = None;
    match rolled {
        Rolled::Recreated => {
            mem.private.attempted = Some(pair);
            mem.deferred = None;
            account_mem.awaiting_confirm = true;
            log::warn!(
                "session_reconcile: {container}: ran {running}, not this daemon's release image; \
                 recreated it from {release} against its existing volume and profile"
            );
            Ok(Some(Outcome::PrivateImageRecreated {
                from: running.to_owned(),
                to: release,
            }))
        }
        Rolled::Kept => {
            mem.private.attempted = Some(pair);
            Ok(None)
        }
        Rolled::Unchanged => Ok(None),
    }
}
