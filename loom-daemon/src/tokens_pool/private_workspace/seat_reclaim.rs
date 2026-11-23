//! Reclaim inside private-workspace seat volumes (#14678, Asks 1 and 3 — the
//! periodic idle pass; the shared object store is #14680, the per-seat size
//! telemetry is #14693).
//!
//! # What this reclaims, and where it runs
//!
//! A seat volume (`/workspace` inside `loom-codex-workspace-<seat>`) grows as
//! seats × registered repos × (clone + worktrees + scratch) and the host-side
//! reaper never sees inside it — its `reaper.pass` spans report
//! `preserved.private_workspace` for everything there. The disk-pressure
//! incident behind this issue (two workers at 100%, 224 GB across 7 seats)
//! was mostly per-step scratch and finished-work checkouts no host pass could
//! reach. This module adds the missing in-volume pass:
//!
//! - **Scratch**: `cache/<role>-<n>.*` entries and top-level step dirs
//!   (`<role>-<n>.*`, `*-tmp`) older than the existing
//!   `autonomous.worktreeReaper.scratchReclaim.maxAgeHours`.
//! - **Worktrees**: linked checkouts under `repo` and `work/<slug>` that are
//!   clean, fully published, and whose branch has a confirmed **merged** PR
//!   at that exact tip past the existing reaper grace
//!   (`autonomous.worktreeReaper.gracePeriodSecs`).
//!
//! The pass runs in two halves. The container half ([`seat_reclaim`]) is a
//! `private-workspace seat-reclaim` verb driven like `prepare`/`recover`; it
//! never mutates `.identity` records, never quarantines and never takes a
//! backup — a worktree it cannot prove finished is preserved with a reason.
//! The host half ([`schedule_from_tick`], called from the eager-reclaim tier
//! 1 the way [`crate::scratch_reclaim`] is) enumerates the seats, takes each
//! account's exclusive lease, requires the container running and idle — the
//! same admission [`super::lifecycle::run_job`] applies — and execs the verb.
//! It never calls `docker::prepare_target`: preparation runs
//! `clean_worktrees`, which refuses a dirty worktree, and a dirty seat must
//! still get its scratch reclaimed and its size reported.
//!
//! # Off the dispatch tick (#14777)
//!
//! Each seat's exec is bounded by a 600s timeout, so a synchronous whole-host
//! walk can hold the dispatch loop's tick for `seats × 600s` — exactly while
//! free space is below the floor. Since #14777 the eager tier-1 trigger
//! therefore **schedules** the sequential walk on a blocking worker
//! ([`schedule_from_tick`]) and returns immediately: at most one walk is in
//! flight host-wide (a trigger during a walk returns pending and queues
//! nothing), the existing enable switch and host-wide cooldown still gate it,
//! and the walk's verdict — reclaimed something, or empty — is folded into
//! the originating root's eager-reclaim ledger exactly once on completion.
//!
//! # Ownership vs per-job preparation (#14483)
//!
//! Job preparation (merged #14483) owns the **target clone at admission
//! time**: it forgives untracked caches, force-reaps linked worktrees whose
//! tip has a *finished* PR — merged **or closed-unmerged**, dirty ones
//! included — and offers the explicit `accounts session recover` listing.
//! This pass owns **all seats and all clones, idle**: it is merged-only,
//! clean-only, and publishes what it kept through `reaper.pass`. The dirty
//! sibling in a seat with a published tip at a merged PR is preparation's to
//! remove and this pass's to preserve; see
//! `defaults/docs/private-session-dispatch.md`.
//!
//! # Safety
//!
//! Liveness is structural, not probed: the verb only ever runs while the
//! seat's account lease is held and the container is proven idle, so no step
//! session can be live in the volume; the scratch age floor then bounds the
//! blast radius to entries no recent step wrote. Future-dated mtimes are
//! never reclaimable ([`crate::scratch_reclaim::is_reclaimable_age`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::repository;
use crate::worktree_ops::clean::check_grace_period;
use crate::worktree_ops::clean_owner::PrAtHead;

// ============================================================================
// The verb (container side)
// ============================================================================

#[derive(clap::Args)]
pub struct SeatReclaimArgs {
    #[arg(long)]
    pub account: String,
    /// Scratch age floor in seconds; the host resolves it from
    /// `autonomous.worktreeReaper.scratchReclaim.maxAgeHours`.
    #[arg(long)]
    pub max_age_secs: i64,
    /// Post-merge grace in seconds; the host resolves it from
    /// `autonomous.worktreeReaper.gracePeriodSecs`.
    #[arg(long)]
    pub grace_secs: i64,
}

/// One item the pass removed (#14889): what the host logs per item and
/// exports as one `reaper.removal` span. `path` is relative to the seat
/// volume (`cache/judge-42`, `repo/.loom/worktrees/issue-7`), so an operator
/// can tell a cached scratch entry from a top-level one without ever seeing
/// a host path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedItem {
    /// `worktree` or `scratch`.
    pub kind: String,
    /// The item's path relative to the seat volume.
    pub path: String,
    /// The removed worktree's branch (worktree removals only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The removed worktree's full pre-removal tip SHA (worktree removals
    /// only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tip: Option<String>,
    /// The bytes the removal freed — the count scratch reclaim already
    /// computed, or the held size a worktree removal already adds up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

impl RemovedItem {
    /// The item's one-line audit description for the host log (#14889).
    /// Every free-form field is Debug-escaped, so a crafted name cannot
    /// forge or split the line; a long path rides here in full even when the
    /// span's bounded attributes drop it.
    #[must_use]
    pub fn describe(&self) -> String {
        let mut line = format!("{} {:?}", self.kind, self.path);
        if let Some(branch) = &self.branch {
            line.push_str(&format!(", branch {branch:?}"));
        }
        if let Some(tip) = &self.tip {
            line.push_str(&format!(", tip {tip}"));
        }
        if let Some(bytes) = self.bytes {
            line.push_str(&format!(", {}", crate::fs_util::human_size(bytes)));
        }
        line
    }

    /// The item as the reaper's removal span carries it (#14889).
    #[must_use]
    pub fn removal(&self) -> crate::observability::ops::reaper::Removal<'_> {
        crate::observability::ops::reaper::Removal {
            kind: &self.kind,
            path: &self.path,
            branch: self.branch.as_deref(),
            tip: self.tip.as_deref(),
            bytes: self.bytes,
        }
    }
}

/// One pass's outcome, reported to the host as the protocol `outcome`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatReclaimOutcome {
    /// Linked worktrees evaluated (the main checkout is never a candidate).
    pub worktrees_scanned: usize,
    pub worktrees_removed: usize,
    pub worktrees_failed: usize,
    /// The preserve wording of every worktree left in place.
    pub preserved: Vec<String>,
    /// Bytes freed by worktree removals plus scratch removals.
    pub bytes: u64,
    /// Step-scratch entries removed (cache and top-level).
    pub scratch_removed: usize,
    /// Step-scratch entries evaluated (shape-matched candidates).
    pub scratch_scanned: usize,
    /// Bytes freed by scratch removals alone.
    pub scratch_bytes: u64,
    /// One record per successful removal, worktrees and scratch alike
    /// (#14889). Absent from a counts-only seat image (serde default), which
    /// still yields the summaries and counts above.
    #[serde(default)]
    pub removed: Vec<RemovedItem>,
    /// A Git error that stopped the worktree half's inspection (#14889).
    /// Inspection stops there exactly as before, but the removals already
    /// recorded are complete and stand: the host reports both the failure
    /// and every earlier removal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inspection_error: Option<String>,
}

/// The PR-state lookup the worktree half consults, injected so tests never
/// contact a forge: `(clone, repository, branch, tip) -> state`.
pub type PrLookup<'a> = dyn Fn(&Path, &str, &str, &str) -> PrAtHead + 'a;

/// One in-volume pass. `root` is the seat volume root (`/workspace`).
pub(super) fn seat_reclaim(
    root: &Path,
    args: &SeatReclaimArgs,
    pr_at_head: &PrLookup<'_>,
    now: DateTime<Utc>,
) -> anyhow::Result<SeatReclaimOutcome> {
    if args.account.is_empty() {
        anyhow::bail!("seat reclaim requires the account identity");
    }
    let root = root.canonicalize()?;
    let mut outcome = SeatReclaimOutcome::default();
    reclaim_scratch(&root, args.max_age_secs, now, &mut outcome);
    reap_merged_worktrees(&root, args, pr_at_head, now, &mut outcome);
    Ok(outcome)
}

// ============================================================================
// Scratch selection (pure gates + the sweep)
// ============================================================================

/// Whether `name` matches the per-step scratch shape `<role>-<n>` with an
/// optional `.`/`-` remainder: `sta-6421.tar.gz`, `judge-9661`,
/// `builder-6940.x`, `squad-6940-dir`, `loom-7456-check.log`. Reserved roots
/// (`repo`, `work`, `cache`, `identity.json`, dot-entries) match no prefix of
/// this shape. Pure.
#[must_use]
pub fn is_step_scratch_name(name: &str) -> bool {
    let Some((role, rest)) = name.split_once('-') else {
        return false;
    };
    if role.is_empty() || !role.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let digits = rest.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    let remainder = &rest[digits..];
    remainder.is_empty() || remainder.starts_with('.') || remainder.starts_with('-')
}

/// Whether `name` is a top-level step dir ending in `-tmp`:
/// `doctor-6422-tmp`, `curator-repro-tmp`. Pure.
#[must_use]
pub fn is_tmp_step_name(name: &str) -> bool {
    name.len() > 4 && name.ends_with("-tmp")
}

/// The item's path relative to the volume root (#14889): the record names
/// the item without a host path, and `cache/judge-42` stays distinct from a
/// top-level `judge-42`.
fn relative(root: &Path, path: &Path) -> String {
    let path = path.strip_prefix(root).unwrap_or(path);
    // JSON paths are strings. Preserve the byte escapes of a non-UTF-8 name
    // so distinct files don't collapse to the same replacement character.
    path.to_str()
        .map_or_else(|| format!("{path:?}"), str::to_owned)
}

fn reclaim_scratch(
    root: &Path,
    max_age_secs: i64,
    now: DateTime<Utc>,
    outcome: &mut SeatReclaimOutcome,
) {
    let max_age_secs = max_age_secs.max(0);
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root.join("cache")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_step_scratch_name(&name) {
                candidates.push(entry.path());
            }
        }
    }
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if is_step_scratch_name(&name) || is_tmp_step_name(&name) {
                candidates.push(entry.path());
            }
        }
    }
    for path in candidates {
        outcome.scratch_scanned += 1;
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        let modified: DateTime<Utc> = modified.into();
        let age_secs = (now - modified).num_seconds();
        if !crate::scratch_reclaim::is_reclaimable_age(age_secs, max_age_secs) {
            continue;
        }
        let bytes = if path.is_dir() {
            tree_bytes(&path)
        } else {
            metadata.len()
        };
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if removed.is_ok() {
            outcome.scratch_removed += 1;
            outcome.scratch_bytes += bytes;
            outcome.bytes += bytes;
            outcome.removed.push(RemovedItem {
                kind: "scratch".into(),
                path: relative(root, &path),
                branch: None,
                tip: None,
                bytes: Some(bytes),
            });
        } else {
            log::debug!("seat_reclaim: could not remove {}: {:?}", path.display(), removed);
        }
    }
}

fn tree_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    total += tree_bytes(&entry.path());
                } else if let Ok(meta) = entry.metadata() {
                    total += meta.len();
                }
            }
        }
    }
    total
}

// ============================================================================
// Worktree reap (merged-only, clean-only, in-volume)
// ============================================================================

/// Every clone of the volume with its recorded repository: the legacy primary
/// (`repo`, identity at the volume root) and each `work/<slug>` carrying a
/// `work/.identity/<slug>.json` record. A staging dir (`.cloning-*`) and an
/// unrecorded directory are not ours to touch and are not enumerated.
fn volume_clones(root: &Path) -> Vec<(PathBuf, String)> {
    let mut clones = Vec::new();
    let primary_record = root.join("identity.json");
    let primary = root.join("repo");
    if primary.is_dir() {
        if let Ok(Some((repository, _))) = repository::recorded_identity(&primary_record) {
            clones.push((primary, repository));
        }
    }
    let work = root.join("work");
    let identities = work.join(".identity");
    if let Ok(entries) = std::fs::read_dir(&work) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if !entry.path().is_dir() || !super::safe_segment(name) || name.contains("..") {
                continue;
            }
            let record = identities.join(format!("{name}.json"));
            if let Ok(Some((repository, _))) = repository::recorded_identity(&record) {
                clones.push((entry.path(), repository));
            }
        }
    }
    clones
}

/// The merged-only reap. A Git error no longer aborts the verb past the
/// accumulated outcome (#14889): inspection stops at the first error exactly
/// as before — never guessing at unreadable state — but the error is
/// recorded in [`SeatReclaimOutcome::inspection_error`] so the host reports
/// both it and every removal already completed (scratch included, which the
/// old `?` propagated away).
fn reap_merged_worktrees(
    root: &Path,
    args: &SeatReclaimArgs,
    pr_at_head: &PrLookup<'_>,
    now: DateTime<Utc>,
    outcome: &mut SeatReclaimOutcome,
) {
    'pass: for (repo, repository) in volume_clones(root) {
        let clone = CloneUnderInspection {
            root,
            path: &repo,
            repository: &repository,
        };
        let entries = match repository::worktree_entries(clone.path) {
            Ok(entries) => entries,
            Err(error) => {
                outcome.inspection_error = Some(error.to_string());
                break 'pass;
            }
        };
        for entry in entries {
            if let Err(error) = inspect_entry(&clone, &entry, args, pr_at_head, now, outcome) {
                outcome.inspection_error = Some(error.to_string());
                break 'pass;
            }
        }
    }
}

/// One enumerated clone under inspection: the volume root it belongs to
/// (for the containment gate and relative record paths), the clone's path
/// and its recorded repository.
struct CloneUnderInspection<'a> {
    root: &'a Path,
    path: &'a Path,
    repository: &'a str,
}

/// One worktree's gates and, when every gate passes, its removal. Preserve
/// decisions land in `outcome.preserved`; an `Err` is a Git inspection
/// failure, which stops the whole pass.
fn inspect_entry(
    clone: &CloneUnderInspection<'_>,
    entry: &repository::WorktreeEntry,
    args: &SeatReclaimArgs,
    pr_at_head: &PrLookup<'_>,
    now: DateTime<Utc>,
    outcome: &mut SeatReclaimOutcome,
) -> anyhow::Result<()> {
    let (root, repo, path) = (clone.root, clone.path, &entry.path);
    outcome.worktrees_scanned += 1;
    let Some(canonical) = path.canonicalize().ok() else {
        outcome.preserved.push("worktree state unreadable".into());
        return Ok(());
    };
    if canonical == repo {
        // The main checkout is what the seat IS; never a candidate.
        outcome.worktrees_scanned -= 1;
        return Ok(());
    }
    if entry.locked {
        outcome.preserved.push("worktree locked (in use)".into());
        return Ok(());
    }
    if !canonical.starts_with(root) {
        outcome
            .preserved
            .push("worktree outside the private volume".into());
        return Ok(());
    }
    let Some(branch) = repository::current_branch(path)? else {
        outcome
            .preserved
            .push("detached HEAD (unnamed work)".into());
        return Ok(());
    };
    if repository::ahead_of_origin(path, "HEAD")? > 0 {
        outcome.preserved.push("unpushed commits on HEAD".into());
        return Ok(());
    }
    if !repository::worktree_status(path)?.is_empty() {
        outcome
            .preserved
            .push("uncommitted changes in worktree".into());
        return Ok(());
    }
    let tip = repository::git(path, &["rev-parse", "HEAD"])?
        .trim()
        .to_owned();
    match pr_at_head(repo, clone.repository, &branch, &tip) {
        PrAtHead::Open => outcome.preserved.push("PR still open".into()),
        PrAtHead::ClosedNoMerge => outcome
            .preserved
            .push("PR closed without merging (not merged)".into()),
        PrAtHead::NoPr => outcome
            .preserved
            .push("no merged PR for the branch at this tip".into()),
        PrAtHead::Unknown => outcome.preserved.push("PR status unknown".into()),
        PrAtHead::Merged(merged_at) => {
            let Ok(merged) = DateTime::parse_from_rfc3339(&merged_at) else {
                outcome
                    .preserved
                    .push("PR status unknown (unparseable merge time)".into());
                return Ok(());
            };
            let (past, _) = check_grace_period(merged.with_timezone(&Utc), args.grace_secs, now);
            if !past {
                outcome
                    .preserved
                    .push("merged PR grace period not passed".into());
                return Ok(());
            }
            let held = tree_bytes(&canonical);
            if repository::git(repo, &["worktree", "remove", &path.display().to_string()]).is_ok() {
                outcome.worktrees_removed += 1;
                outcome.bytes += held;
                outcome.removed.push(RemovedItem {
                    kind: "worktree".into(),
                    path: relative(root, &canonical),
                    branch: Some(branch),
                    tip: Some(tip),
                    bytes: Some(held),
                });
            } else {
                outcome.worktrees_failed += 1;
            }
        }
    }
    Ok(())
}

/// The production lookup the verb binds: resolves the GitHub host/slug of
/// the clone's own repository URL, then asks
/// [`crate::worktree_ops::clean_owner::pr_at_head`] with the clone as the
/// probe's working directory. A repository on any other forge answers
/// [`PrAtHead::Unknown`] — preserved, never removed on a guess.
pub(super) fn github_pr_at_head(
    repo: &Path,
    repository: &str,
    branch: &str,
    tip: &str,
) -> PrAtHead {
    let host = std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into());
    let Some(slug) = super::forge_auth::github_slug(repository, &host) else {
        return PrAtHead::Unknown;
    };
    crate::worktree_ops::clean_owner::pr_at_head(repo, &host, &slug, branch, tip)
}

// ============================================================================
// The host-side pass (leased, idle-only, no preparation)
// ============================================================================

/// One seat's contribution to a host walk (#14678): the account, the verb's
/// outcome (or the reason this seat was skipped), and how long the attempt
/// took — the per-seat elapsed figure the #14777 baseline records, so a live
/// pass distinguishes how long a skip cost from how long a reclaim ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatWalkResult {
    pub account: String,
    pub result: Result<SeatReclaimOutcome, String>,
    pub elapsed: Duration,
}

/// One host-wide pass's outcome over every private-workspace seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeatReclaimReport {
    /// `Some(reason)` when no pass ran at all (disabled, or the cooldown had
    /// not elapsed). `None` on a pass that ran.
    pub skipped: Option<String>,
    /// Per-seat results: the account name and either the verb's outcome or
    /// the reason this seat was skipped/refused (busy, stopped, not idle, a
    /// containment-validation refusal, an endpoint error), with the attempt's
    /// elapsed time.
    pub seats: Vec<SeatWalkResult>,
}

impl SeatReclaimReport {
    /// Worktrees removed across every seat that ran.
    #[must_use]
    pub fn worktrees_removed(&self) -> usize {
        self.seats
            .iter()
            .filter_map(|walk| walk.result.as_ref().ok())
            .map(|outcome| outcome.worktrees_removed)
            .sum()
    }

    /// Scratch entries removed across every seat that ran.
    #[must_use]
    pub fn scratch_removed(&self) -> usize {
        self.seats
            .iter()
            .filter_map(|walk| walk.result.as_ref().ok())
            .map(|outcome| outcome.scratch_removed)
            .sum()
    }

    /// Bytes freed across every seat that ran.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        self.seats
            .iter()
            .filter_map(|walk| walk.result.as_ref().ok())
            .map(|outcome| outcome.bytes)
            .sum()
    }

    /// One human-readable summary for the eager-reclaim log line.
    #[must_use]
    pub fn summary(&self) -> String {
        if let Some(reason) = &self.skipped {
            return format!("skipped: {reason}");
        }
        format!(
            "{} seat(s): {} worktree(s), {} scratch entries ({})",
            self.seats.len(),
            self.worktrees_removed(),
            self.scratch_removed(),
            crate::fs_util::human_size(self.bytes())
        )
    }
}

/// Publish one seat's completed outcome (#14889): the two per-seat
/// `reaper.pass` summaries (phases, counts and byte totals exactly as
/// before), one `reaper.removal` span and one info line per removed item,
/// and a warning when a Git error stopped the worktree half mid-pass — the
/// removals recorded before it stand and are each reported once.
pub(super) fn report_seat(
    account: &str,
    outcome: &SeatReclaimOutcome,
    elapsed: std::time::Duration,
) {
    use crate::observability::ops::reaper;

    // The span's repo label is the seat's account name: one path-shaped
    // carrier, never a host path (`reaper::pass_span` takes the final
    // component only).
    let seat = Path::new("/private-workspace-seats").join(account);
    let worktree_bytes = outcome.bytes.saturating_sub(outcome.scratch_bytes);
    let preserved: Vec<&str> = outcome.preserved.iter().map(String::as_str).collect();
    reaper::publish(
        &seat,
        &reaper::Pass {
            class: "seat",
            phase: reaper::PHASE_REAP,
            scanned: outcome.worktrees_scanned,
            removed: outcome.worktrees_removed,
            failed: outcome.worktrees_failed,
            preserved: preserved.clone(),
            bytes: worktree_bytes,
            backed_up: 0,
            backup_refs: Vec::new(),
        },
    );
    reaper::publish(
        &seat,
        &reaper::Pass {
            class: "seat",
            phase: reaper::PHASE_RECLAIM,
            scanned: outcome.scratch_scanned,
            removed: outcome.scratch_removed,
            failed: 0,
            preserved: Vec::new(),
            bytes: outcome.scratch_bytes,
            backed_up: 0,
            backup_refs: Vec::new(),
        },
    );
    for item in &outcome.removed {
        let mut line = item.describe();
        if let Some(trace_id) = reaper::publish_removal(&seat, &item.removal()) {
            line.push_str(", trace_id=");
            line.push_str(trace_id.as_str());
        }
        log::info!("private_workspace seat_reclaim: seat {account} removed {line}");
    }
    if let Some(error) = &outcome.inspection_error {
        log::warn!(
            "private_workspace seat_reclaim: seat {account} stopped at a Git inspection error \
             after {} completed removal(s); the removals above stand: {error}",
            outcome.removed.len()
        );
    }
    if outcome.worktrees_removed > 0 || outcome.scratch_removed > 0 {
        log::info!(
            "private_workspace seat_reclaim: seat {account} removed {} worktree(s) and {} \
             scratch entries ({}) — preserved {preserved:?}",
            outcome.worktrees_removed,
            outcome.scratch_removed,
            crate::fs_util::human_size(outcome.bytes),
        );
    } else {
        log::debug!(
            "private_workspace seat_reclaim: seat {account} (took {:?}s) nothing to reclaim — \
             preserved {preserved:?}",
            elapsed.as_secs(),
        );
    }
}

/// Host-wide walk state: one marker key, not per-repo — the pass walks
/// every seat from whatever root the eager trigger names, so a per-root key
/// would re-run it once per registered root per tick. Since #14777 the
/// cooldown eligibility check, its recording and the single-flight marker
/// are one lock acquisition, and `active` additionally caps the whole host
/// at one in-flight walk (#14777).
struct WalkState {
    /// When a walk last started (recorded at schedule time, so a walk that
    /// outlasts the cooldown is still single-flighted, not re-triggered).
    last_run_at: Option<DateTime<Utc>>,
    /// The one walk currently in flight, if any.
    active: Option<ActiveWalk>,
    /// Monotonic walk id, so a late-clearing guard can only ever clear its
    /// own walk.
    next_id: u64,
}

/// The in-flight walk marker. Only the id is read: a late-clearing guard
/// (a worker that panicked long after a test reset, say) may only ever
/// clear its own walk.
struct ActiveWalk {
    id: u64,
}

static WALK_STATE: OnceLock<Mutex<WalkState>> = OnceLock::new();

fn walk_state_slot() -> &'static Mutex<WalkState> {
    WALK_STATE.get_or_init(|| {
        Mutex::new(WalkState {
            last_run_at: None,
            active: None,
            next_id: 0,
        })
    })
}

/// Drop walk state. Test-only seam (the process-global would otherwise leak
/// between tests in the same binary).
#[cfg(test)]
#[doc(hidden)]
pub fn reset_state_for_test() {
    *walk_state_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = WalkState {
        last_run_at: None,
        active: None,
        next_id: 0,
    };
}

/// Whether a walk is in flight right now. Test-only observation seam for the
/// single-flight tests.
#[cfg(test)]
#[doc(hidden)]
pub fn walk_in_flight_for_test() -> bool {
    walk_state_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .active
        .is_some()
}

/// Clears this walk's in-flight marker on drop — normal completion **and**
/// a panicked worker alike, so a failed walk cannot block the host's walks
/// forever (#14777: "worker failure permits a later eligible pass").
struct ActiveGuard {
    id: u64,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut state = walk_state_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.active.as_ref().is_some_and(|walk| walk.id == self.id) {
            state.active = None;
        }
    }
}

/// What the eligibility gates decided for one trigger. Pure.
#[derive(Debug, Clone, PartialEq, Eq)]
enum WalkDecision {
    Start,
    /// A walk is already in flight — the trigger returns pending and queues
    /// nothing, even past the cooldown.
    AlreadyRunning,
    Skip(String),
}

/// The trigger gates, pure (#14777): the existing enable switch, then the
/// single-flight marker (a walk in flight is a pending verdict regardless of
/// the cooldown, since a walk can outlast its own cooldown window), then the
/// host-wide cooldown.
fn walk_decision(
    enabled: bool,
    active: bool,
    last_run_at: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: u64,
) -> WalkDecision {
    if !enabled {
        return WalkDecision::Skip("disabled (scratchReclaim.enabled=false)".into());
    }
    if active {
        return WalkDecision::AlreadyRunning;
    }
    if let Some(reason) = cooldown_skip(last_run_at, now, min_interval_secs) {
        return WalkDecision::Skip(reason);
    }
    WalkDecision::Start
}

/// The seat sub-pass's disposition from one eager trigger (#14777): the walk
/// runs off the dispatch tick, so a trigger either finds a walk in flight or
/// schedules one, and never awaits the result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeatScheduling {
    /// A walk is in flight (scheduled by this trigger, or already running
    /// from any repository root). The pass's empty-pass verdict is deferred
    /// to the walk's completion.
    Pending,
    /// No seat work: the existing enable gate or host-wide cooldown said no.
    /// Carries the reason into the eager report.
    Skipped(String),
}

/// The walk body the coordinator runs on a blocking worker: given the
/// originating root, walk every seat and report. Injectable so the
/// single-flight tests can gate it on a channel.
type WalkBody = Arc<dyn Fn(PathBuf) -> SeatReclaimReport + Send + Sync>;

/// The production walk body: the sequential seat walk with its existing
/// lease, running/idle and reporting behavior, on this blocking worker.
fn run_walk(workspace: &Path) -> SeatReclaimReport {
    let config = crate::scratch_reclaim::read_scratch_reclaim_config(workspace);
    let max_age_secs =
        i64::try_from(crate::scratch_reclaim::resolve_max_age_hours(&config).saturating_mul(3600))
            .unwrap_or(i64::MAX);
    let grace_secs = crate::worktree_reaper::resolve_grace_period(
        &crate::worktree_reaper::read_worktree_reaper_config(workspace),
    );
    let seats = super::lifecycle::seat_reclaim(workspace, max_age_secs, grace_secs);

    for walk in &seats {
        match &walk.result {
            Ok(outcome) => report_seat(&walk.account, outcome, walk.elapsed),
            Err(_) => log::debug!(
                "private_workspace seat_reclaim: {} skipped after {:?}s: {:?}",
                walk.account,
                walk.elapsed.as_secs(),
                walk.result
            ),
        }
    }

    let report = SeatReclaimReport {
        skipped: None,
        seats,
    };
    if report.worktrees_removed() > 0 || report.scratch_removed() > 0 {
        log::info!(
            "private_workspace seat_reclaim: {} reclaimed across {} seat(s)",
            report.summary(),
            report.seats.len()
        );
    }
    report
}

fn secs_fmt(elapsed: Duration) -> String {
    format!("{:.1}s", elapsed.as_secs_f64())
}

/// Spawn the walk on a blocking worker and return immediately. The worker
/// clears the in-flight marker on exit (via [`ActiveGuard`], even on panic),
/// folds the walk's verdict into the originating root's eager-reclaim ledger
/// exactly once, and logs the completion line the #14777 baseline reads:
/// per-seat elapsed, distinguishing seats that ran from seats that were
/// skipped.
fn spawn_walk(handle: &tokio::runtime::Handle, id: u64, origin: PathBuf, body: WalkBody) {
    let guard = ActiveGuard { id };
    handle.spawn_blocking(move || {
        let _guard = guard;
        let started = std::time::Instant::now();
        let report = body(origin.clone());
        let elapsed = started.elapsed();
        crate::eager_reclaim::record_seat_verdict(&origin, &report);
        let per_seat: Vec<String> = report
            .seats
            .iter()
            .map(|walk| match &walk.result {
                Ok(_) => format!("{}={}:ran", walk.account, secs_fmt(walk.elapsed)),
                Err(_) => format!("{}={}:skip", walk.account, secs_fmt(walk.elapsed)),
            })
            .collect();
        if report.skipped.is_none() {
            log::info!(
                "private_workspace seat_reclaim: off-tick walk from {} finished in {} — {} [{}] \
                 (#14777)",
                origin.display(),
                secs_fmt(elapsed),
                report.summary(),
                per_seat.join(", "),
            );
        }
    });
}

/// Schedule one walk if the existing gates allow it, against `now`. The
/// eligibility check, the cooldown recording and the single-flight marker are
/// one lock acquisition; the spawn happens after the lock is released. The
/// `body` seam exists so the single-flight tests can hold the walk on a
/// channel.
fn schedule_with(workspace: &Path, now: DateTime<Utc>, body: WalkBody) -> SeatScheduling {
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return SeatScheduling::Skipped(
            "no async runtime in scope for the blocking walk worker (#14777)".into(),
        );
    };
    let config = crate::scratch_reclaim::read_scratch_reclaim_config(workspace);
    let enabled = crate::scratch_reclaim::resolve_enabled(&config);
    let min_interval_secs = crate::scratch_reclaim::resolve_min_interval_secs(&config);
    let id = {
        let mut state = walk_state_slot()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match walk_decision(
            enabled,
            state.active.is_some(),
            state.last_run_at,
            now,
            min_interval_secs,
        ) {
            WalkDecision::Skip(reason) => {
                log::debug!("private_workspace seat_reclaim: trigger declined: {reason}");
                return SeatScheduling::Skipped(reason);
            }
            WalkDecision::AlreadyRunning => return SeatScheduling::Pending,
            WalkDecision::Start => {
                state.last_run_at = Some(now);
                state.next_id += 1;
                let id = state.next_id;
                state.active = Some(ActiveWalk { id });
                id
            }
        }
    };
    spawn_walk(&handle, id, workspace.to_path_buf(), body);
    SeatScheduling::Pending
}

/// The eager-reclaim tier-1 binding (#14678; off the dispatch tick since
/// #14777): honor the existing `autonomous.worktreeReaper.scratchReclaim.*`
/// surface for the enable gate, scratch age floor and cooldown — no new
/// flags — and schedule the sequential seat walk on a blocking worker,
/// returning without awaiting it. The worktree half's grace comes from the
/// existing `autonomous.worktreeReaper.gracePeriodSecs`.
pub fn schedule_from_tick(workspace: &Path) -> SeatScheduling {
    schedule_with(workspace, Utc::now(), Arc::new(|root| run_walk(&root)))
}

/// Test-only: [`schedule_with`] with an injected clock and walk body, so
/// tests outside this module (the tick-line's dispatch-advance coverage)
/// can gate a fake walk on a channel without touching production wiring.
#[cfg(test)]
#[doc(hidden)]
pub(crate) fn schedule_with_for_test(
    workspace: &Path,
    now: DateTime<Utc>,
    body: WalkBody,
) -> SeatScheduling {
    schedule_with(workspace, now, body)
}

/// Whether the host-wide cooldown suppresses a pass at `now`. Pure: the
/// decision the report's `skipped` reason is built from.
#[must_use]
pub fn cooldown_skip(
    last: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    min_interval_secs: u64,
) -> Option<String> {
    let last = last?;
    let since = (now - last).num_seconds();
    (since >= 0 && since < i64::try_from(min_interval_secs).unwrap_or(i64::MAX))
        .then(|| format!("a pass ran {since}s ago (cooldown {min_interval_secs}s)"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_git_helpers::{configure, run as raw_git};
    use crate::worktree_ops::clean_owner::pr_at_head_in_rows;
    use chrono::Duration;

    /// The volume fixture: a remote, a prepared primary clone (`repo` with
    /// its identity record), and nothing else — the same shape
    /// [`super::super::tests::RepoFixture`] builds for the prepare tests.
    struct Volume {
        _temp: tempfile::TempDir,
        remote: PathBuf,
        root: PathBuf,
    }

    impl Volume {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let remote = temp.path().join("remote");
            let root = temp.path().join("volume");
            std::fs::create_dir(&remote).unwrap();
            std::fs::create_dir(&root).unwrap();
            raw_git(&remote, &["init", "-b", "main"]);
            configure(&remote);
            std::fs::write(remote.join("fixture"), "base").unwrap();
            raw_git(&remote, &["add", "."]);
            raw_git(&remote, &["commit", "-m", "fixture"]);
            let args = repository::PrepareArgs {
                account: "fixture".into(),
                repository: remote.to_string_lossy().into_owned(),
                base: "main".into(),
                container_id: "test-container".into(),
                branch: None,
                slug: None,
            };
            repository::prepare(&root, &args).unwrap();
            Self {
                _temp: temp,
                remote,
                root,
            }
        }

        fn repo(&self) -> PathBuf {
            self.root.join("repo")
        }

        /// A linked worktree under the clone's `.loom/worktrees`, on a new
        /// branch with one commit; `push` publishes the branch and fetches it
        /// back, so the clone's remote-tracking refs name the tip (the
        /// unpublished-branch gate reads those, not the remote).
        fn worktree(&self, name: &str, push: bool) -> PathBuf {
            let branch = format!("feature/{name}");
            let path = self.repo().join(".loom/worktrees").join(name);
            raw_git(
                &self.repo(),
                &[
                    "worktree",
                    "add",
                    "-b",
                    &branch,
                    path.to_string_lossy().as_ref(),
                ],
            );
            std::fs::write(path.join(name), "work").unwrap();
            raw_git(&path, &["add", "."]);
            raw_git(&path, &["commit", "-m", name]);
            if push {
                raw_git(&path, &["push", self.remote.to_string_lossy().as_ref(), &branch]);
                // Update the clone's remote-tracking refs: `rev-list --not
                // --remotes=origin` reads those, not the remote itself.
                raw_git(&path, &["fetch", "origin"]);
            }
            path
        }
    }

    fn args(max_age_secs: i64, grace_secs: i64) -> SeatReclaimArgs {
        SeatReclaimArgs {
            account: "fixture".into(),
            max_age_secs,
            grace_secs,
        }
    }

    fn merged_days_ago(days: i64) -> String {
        (Utc::now() - Duration::days(days)).to_rfc3339()
    }

    // ===================================================================
    // Pure gates
    // ===================================================================

    #[test]
    fn step_scratch_shapes_match_only_step_names() {
        for name in [
            "sta-6421",
            "sta-6421.tar.gz",
            "judge-9661.json",
            "builder-6940.x",
            "squad-6940-dir",
            "loom-7456-check.log",
            "role_1-2",
        ] {
            assert!(is_step_scratch_name(name), "{name}");
        }
        for reserved in [
            "repo",
            "work",
            "cache",
            "identity.json",
            ".identity",
            ".cloning-x",
            "main",
            "not-step",
            "steps-late",
            "builder-x",
            "-6422",
        ] {
            assert!(!is_step_scratch_name(reserved), "{reserved}");
        }
        assert!(is_tmp_step_name("doctor-6422-tmp"));
        assert!(is_tmp_step_name("curator-repro-tmp"));
        assert!(!is_tmp_step_name("tmp"));
        assert!(!is_tmp_step_name("work"));
    }

    // ===================================================================
    // The periodic-pass fixture: eligible beside dirty / unpublished /
    // open-PR / closed-unmerged siblings. Only the eligible checkout
    // disappears (#14678).
    // ===================================================================

    #[test]
    fn only_the_eligible_checkout_is_removed() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        // Everything a removal needs — so for the dirty sibling, published
        // tip at a merged PR past grace, cleanliness alone preserves it.
        let eligible = volume.worktree("issue-10", true);
        let dirty = volume.worktree("issue-11", true);
        std::fs::write(dirty.join("untracked"), "dirty").unwrap();
        let unpublished = volume.worktree("issue-12", false);
        let open = volume.worktree("issue-13", true);
        let closed_unmerged = volume.worktree("issue-14", true);
        let within_grace = volume.worktree("issue-15", true);

        let lookup = |_: &Path, _: &str, branch: &str, _: &str| -> PrAtHead {
            match branch {
                "feature/issue-10" | "feature/issue-11" => PrAtHead::Merged(merged_days_ago(30)),
                "feature/issue-13" => PrAtHead::Open,
                "feature/issue-14" => PrAtHead::ClosedNoMerge,
                "feature/issue-15" => PrAtHead::Merged(merged_days_ago(0)),
                _ => PrAtHead::NoPr,
            }
        };
        let outcome = seat_reclaim(&volume.root, &args(0, 3600), &lookup, Utc::now()).unwrap();

        assert!(!eligible.exists(), "the eligible checkout must disappear");
        assert!(dirty.exists(), "dirty is preserved by cleanliness alone");
        assert!(unpublished.exists(), "unpublished is preserved");
        assert!(open.exists(), "an open PR is preserved");
        assert!(closed_unmerged.exists(), "closed-unmerged is preserved");
        assert!(within_grace.exists(), "merged inside the grace window is preserved");

        assert_eq!(outcome.worktrees_removed, 1);
        assert_eq!(outcome.worktrees_failed, 0);
        assert_eq!(outcome.worktrees_scanned, 6);
        let reasons = outcome.preserved.join("\n");
        assert!(reasons.contains("uncommitted changes in worktree"), "{reasons}");
        assert!(reasons.contains("unpushed commits on HEAD"), "{reasons}");
        assert!(reasons.contains("PR still open"), "{reasons}");
        assert!(reasons.contains("PR closed without merging"), "{reasons}");
        assert!(reasons.contains("merged PR grace period not passed"), "{reasons}");
        // The branch ref and its commits survive the checkout's removal.
        assert!(
            raw_git(&volume.repo(), &["rev-parse", "--verify", "refs/heads/feature/issue-10"])
                .len()
                == 40
        );
    }

    /// PR-fetch residue shadowing a merged branch: the pass must hand the
    /// branch's true name to the PR lookup (so the checkout is reclaimed),
    /// skip a detached sibling, and leave every ref it does not own — the
    /// branch ref and the residue — exactly where it was (#15027).
    #[test]
    fn residue_shadowed_worktrees_pass_their_true_branch_to_the_pr_lookup() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        let shadowed = volume.worktree("issue-344", true);
        let detached = volume.repo().join(".loom/worktrees").join("detached");
        raw_git(
            &volume.repo(),
            &[
                "worktree",
                "add",
                "--detach",
                detached.to_string_lossy().as_ref(),
            ],
        );
        let repo = volume.repo();
        let tip = raw_git(&repo, &["rev-parse", "refs/heads/feature/issue-344"]);
        raw_git(&repo, &["update-ref", "refs/feature/issue-344", &tip]);
        raw_git(&repo, &["update-ref", "refs/remotes/feature/issue-344", &tip]);
        let lookup = |_: &Path, _: &str, branch: &str, _: &str| -> PrAtHead {
            if branch == "feature/issue-344" {
                PrAtHead::Merged(merged_days_ago(30))
            } else {
                PrAtHead::NoPr
            }
        };
        let outcome = seat_reclaim(&volume.root, &args(0, 0), &lookup, Utc::now()).unwrap();
        assert!(!shadowed.exists(), "the shadowed checkout is reclaimed by its true name");
        assert!(detached.exists(), "a detached worktree is never a candidate");
        assert_eq!(outcome.worktrees_removed, 1);
        let reasons = outcome.preserved.join("\n");
        assert!(reasons.contains("detached HEAD (unnamed work)"), "{reasons}");
        assert!(
            raw_git(&repo, &["rev-parse", "--verify", "refs/heads/feature/issue-344"]).len() == 40,
            "the branch ref survives the checkout's removal"
        );
        assert_eq!(raw_git(&repo, &["rev-parse", "refs/feature/issue-344"]), tip);
        assert_eq!(raw_git(&repo, &["rev-parse", "refs/remotes/feature/issue-344"]), tip);
    }

    #[test]
    fn main_checkout_and_foreign_worktrees_are_never_candidates() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        // A worktree the clone knows about but that lives outside the volume
        // (a host path): preserved, never followed.
        let outside = volume._temp.path().join("outside-wt");
        raw_git(
            &volume.repo(),
            &[
                "worktree",
                "add",
                "-b",
                "feature/issue-20",
                outside.to_string_lossy().as_ref(),
            ],
        );
        raw_git(
            &outside,
            &[
                "push",
                volume.remote.to_string_lossy().as_ref(),
                "feature/issue-20",
            ],
        );
        raw_git(&outside, &["fetch", "origin"]);
        let lookup = |_: &Path, _: &str, branch: &str, _: &str| -> PrAtHead {
            if branch == "feature/issue-20" {
                PrAtHead::Merged(merged_days_ago(30))
            } else {
                PrAtHead::NoPr
            }
        };
        let outcome = seat_reclaim(&volume.root, &args(0, 0), &lookup, Utc::now()).unwrap();
        assert_eq!(outcome.worktrees_removed, 0);
        assert!(outside.exists());
        assert!(outcome
            .preserved
            .iter()
            .any(|r| r.contains("outside the private volume")));
    }

    // ===================================================================
    // Scratch: ended vs live step, future-dated mtime, reserved roots
    // ===================================================================

    #[test]
    fn scratch_reclaim_removes_ended_steps_and_keeps_live_and_future_dated_ones() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        let cache = volume.root.join("cache");
        std::fs::create_dir_all(cache.join("sta-6421.tar")).unwrap();
        std::fs::write(cache.join("sta-6421.tar").join("blob"), "4 GB").unwrap();
        std::fs::write(cache.join("judge-9661.json"), "log").unwrap();
        std::fs::write(cache.join("tool-cache"), "not a step").unwrap();
        std::fs::create_dir_all(volume.root.join("doctor-6422-tmp")).unwrap();
        std::fs::create_dir_all(volume.root.join("builder-6940.x")).unwrap();
        // A live step's scratch and a clock-skewed one: never reclaimable.
        std::fs::write(cache.join("live-7777.tar"), "fresh").unwrap();
        std::fs::write(cache.join("skewed-8888.tar"), "future").unwrap();

        let now = Utc::now();
        // Ended steps: mtime 48h in the past.
        for path in [
            cache.join("sta-6421.tar"),
            cache.join("judge-9661.json"),
            volume.root.join("doctor-6422-tmp"),
            volume.root.join("builder-6940.x"),
            cache.join("tool-cache"),
        ] {
            set_mtime(&path, now - Duration::hours(48));
        }
        // Clock skew: an mtime in the future.
        set_mtime(&cache.join("skewed-8888.tar"), now + Duration::hours(2));

        let outcome =
            seat_reclaim(&volume.root, &args(24 * 3600, 3600), &|_, _, _, _| PrAtHead::NoPr, now)
                .unwrap();
        assert_eq!(outcome.scratch_removed, 4, "{:?}", outcome);
        assert_eq!(outcome.scratch_scanned, 6);
        assert!(!cache.join("sta-6421.tar").exists());
        assert!(!cache.join("judge-9661.json").exists());
        assert!(!volume.root.join("doctor-6422-tmp").exists());
        assert!(!volume.root.join("builder-6940.x").exists());
        assert!(cache.join("tool-cache").exists(), "non-step cache entries stay");
        assert!(cache.join("live-7777.tar").exists(), "a live step's scratch stays");
        assert!(cache.join("skewed-8888.tar").exists(), "a future-dated entry stays");
        assert!(outcome.scratch_bytes > 0);
        // The reserved roots are never candidates at any age.
        assert!(volume.root.join("repo").exists());
        assert!(volume.root.join("cache").exists());
        assert!(volume.root.join("identity.json").exists());
    }

    /// Stamp an mtime (files and directories) the way the age gates read it.
    fn set_mtime(path: &Path, when: DateTime<Utc>) {
        std::fs::File::open(path)
            .unwrap()
            .set_modified(when.into())
            .unwrap();
    }

    #[test]
    fn a_dirty_seat_still_reclaims_its_scratch() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        std::fs::write(volume.repo().join("stray"), "unfinished work").unwrap();
        let cache = volume.root.join("cache");
        std::fs::write(cache.join("sta-6421.tar"), "4 GB").unwrap();

        let outcome = seat_reclaim(
            &volume.root,
            &args(0, 3600),
            &|_, _, _, _| PrAtHead::Merged(merged_days_ago(30)),
            Utc::now(),
        )
        .unwrap();
        assert!(!cache.join("sta-6421.tar").exists(), "scratch is reclaimed");
        assert_eq!(outcome.scratch_removed, 1);
        assert_eq!(outcome.worktrees_removed, 0);
        assert!(volume.repo().join("stray").exists(), "the dirty main checkout is untouched");
    }

    // ===================================================================
    // Removal records (#14889): every successful removal names what went,
    // with the volume-relative path keeping `cache/judge-42` distinct from
    // `judge-42`; preserved items produce none.
    // ===================================================================

    #[test]
    fn removal_records_name_each_removed_item_and_nothing_preserved() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        let eligible = volume.worktree("issue-30", true);
        let expected_tip = raw_git(&eligible, &["rev-parse", "HEAD"]).trim().to_owned();
        let preserved = volume.worktree("issue-31", true);

        // Same-named scratch at the volume root and under cache/, plus a
        // live (preserved) sibling: the records must tell them apart.
        let cache = volume.root.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("sta-6421"), "1234").unwrap();
        std::fs::write(volume.root.join("sta-6421"), "12").unwrap();
        std::fs::write(cache.join("live-7777"), "fresh").unwrap();
        let now = Utc::now();
        for path in [cache.join("sta-6421"), volume.root.join("sta-6421")] {
            set_mtime(&path, now - Duration::hours(48));
        }

        let lookup = |_: &Path, _: &str, branch: &str, _: &str| -> PrAtHead {
            match branch {
                "feature/issue-30" => PrAtHead::Merged(merged_days_ago(30)),
                "feature/issue-31" => PrAtHead::Open,
                _ => PrAtHead::NoPr,
            }
        };
        let outcome = seat_reclaim(&volume.root, &args(24 * 3600, 3600), &lookup, now).unwrap();

        assert_eq!(outcome.removed.len(), 3, "{:?}", outcome.removed);
        // The worktree record: relative path, branch and the FULL
        // pre-removal tip captured before deletion.
        let worktree = outcome
            .removed
            .iter()
            .find(|item| item.kind == "worktree")
            .expect("a worktree record");
        assert_eq!(worktree.path, "repo/.loom/worktrees/issue-30");
        assert_eq!(worktree.branch.as_deref(), Some("feature/issue-30"));
        assert_eq!(worktree.tip.as_deref(), Some(expected_tip.as_str()));
        assert_eq!(worktree.tip.as_deref().map(str::len), Some(40));
        assert_eq!(worktree.bytes, Some(outcome.bytes - outcome.scratch_bytes));

        // The scratch records: relative names, the byte counts reclaim
        // already computed, and the same name distinguishable by location.
        let scratch: Vec<_> = outcome
            .removed
            .iter()
            .filter(|item| item.kind == "scratch")
            .collect();
        assert_eq!(scratch.len(), 2);
        let by_path = |p: &str| {
            scratch
                .iter()
                .find(|item| item.path == p)
                .unwrap_or_else(|| panic!("no record for {p}"))
        };
        assert_eq!(by_path("cache/sta-6421").bytes, Some(4));
        assert_eq!(by_path("sta-6421").bytes, Some(2));
        assert_eq!(scratch.iter().filter(|item| item.bytes.is_some()).count(), 2);
        // The records' byte totals agree with the outcome's counters.
        assert_eq!(
            scratch
                .iter()
                .map(|item| item.bytes.unwrap_or(0))
                .sum::<u64>(),
            outcome.scratch_bytes
        );
        // Preserved items — the open-PR worktree and the live scratch
        // entry — produce zero records.
        assert!(
            outcome
                .removed
                .iter()
                .all(|item| !item.path.contains("issue-31") && !item.path.contains("live-7777")),
            "{:?}",
            outcome.removed
        );
        assert!(preserved.exists());
        assert!(cache.join("live-7777").exists());
        // The describe() line keeps the record auditable: path, branch, tip.
        let line = worktree.describe();
        assert!(
            line.contains("repo/.loom/worktrees/issue-30")
                && line.contains("feature/issue-30")
                && line.contains(&expected_tip),
            "{line}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_removal_names_remain_distinct_in_the_audit_record() {
        use std::{ffi::OsStr, os::unix::ffi::OsStrExt};

        let root = Path::new("/volume");
        let one = root.join(OsStr::from_bytes(b"cache/judge-42-\xfe"));
        let two = root.join(OsStr::from_bytes(b"cache/judge-42-\xff"));
        assert_ne!(relative(root, &one), relative(root, &two));
        assert!(!relative(root, &one).contains('\u{fffd}'));
        let valid = "cache/judge-42-\u{fffd}";
        assert_eq!(relative(root, &root.join(valid)), valid);
    }

    #[test]
    fn bounded_removal_spans_identify_each_escaped_scratch_path_in_the_log() {
        let volume = tempfile::tempdir().unwrap();
        let cache = volume.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        let long = format!("judge-42-{}", "\u{1}".repeat(70));
        let names = ["judge-42-a\nb", "judge-42-c\nd", &long, "judge-42-plain"];
        for name in names {
            std::fs::write(cache.join(name), b"same").unwrap();
        }
        let mut outcome = SeatReclaimOutcome::default();
        reclaim_scratch(volume.path(), 86_400, Utc::now() + Duration::days(2), &mut outcome);
        assert_eq!(outcome.scratch_removed, names.len());
        let (logs, captured) = crate::observability::ops::capture::capture(|| {
            crate::test_log_capture::capture_logs(|| {
                report_seat("fixture-seat", &outcome, std::time::Duration::from_secs(1))
            })
        });
        let spans: Vec<_> = captured
            .spans
            .into_iter()
            .filter(|span| span.name.as_str() == "reaper.removal")
            .map(|span| span.bounded())
            .collect();
        assert_eq!(spans.len(), names.len());
        for name in names {
            assert!(!cache.join(name).exists());
            let path = format!("cache/{name}");
            let matches: Vec<_> = logs
                .iter()
                .filter(|(_, line)| line.contains(&format!("{path:?}")))
                .collect();
            assert_eq!(matches.len(), 1, "one full escaped log line for {path:?}");
            let (_, line) = matches[0];
            assert!(!line.contains('\n'));
            let matching: Vec<_> = spans
                .iter()
                .filter(|span| {
                    line.contains(&format!("trace_id={}", span.context.trace_id.as_str()))
                })
                .collect();
            assert_eq!(matching.len(), 1, "one exported record matches {line}");
            let attrs = &matching[0].attributes;
            assert_eq!(attrs["loom.reaper.removal.bytes"], "4");
            assert_eq!(
                attrs.get("loom.reaper.removal.path"),
                (!name.chars().any(char::is_control)).then_some(&path)
            );
            let escaped = name.escape_debug().to_string();
            assert_eq!(
                attrs.get("loom.reaper.removal.name"),
                (escaped.len() <= 256).then_some(&escaped)
            );
        }
    }

    /// #14889: a Git inspection error no longer throws away the outcome —
    /// inspection stops exactly as before, but the scratch removals already
    /// completed and the failure indication reach the host together.
    #[test]
    fn a_git_inspection_failure_still_reports_the_completed_removals() {
        let _fork = super::super::tests::fork_guard();
        let volume = Volume::new();
        // An eligible scratch entry: reclaimed before the worktree half runs.
        let cache = volume.root.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(cache.join("sta-6421"), "4 GB").unwrap();
        set_mtime(&cache.join("sta-6421"), Utc::now() - Duration::hours(48));
        // A recorded clone that is not a git repository: the worktree half's
        // Git inspection fails on it.
        let work = volume.root.join("work");
        std::fs::create_dir_all(work.join(".identity")).unwrap();
        std::fs::create_dir_all(work.join("broken-clone")).unwrap();
        std::fs::write(
            work.join(".identity").join("broken-clone.json"),
            serde_json::json!({
                "protocol": super::super::PROTOCOL,
                "account": "fixture",
                "repository": volume.remote.to_string_lossy(),
                "container_id": "test-container",
                "revision": "0".repeat(40),
            })
            .to_string(),
        )
        .unwrap();

        let outcome = seat_reclaim(
            &volume.root,
            &args(24 * 3600, 3600),
            &|_, _, _, _| PrAtHead::NoPr,
            Utc::now(),
        )
        .unwrap();
        assert_eq!(outcome.scratch_removed, 1, "{:?}", outcome);
        assert!(
            outcome
                .inspection_error
                .as_deref()
                .is_some_and(|e| !e.is_empty()),
            "the failure is carried alongside the removals: {:?}",
            outcome.inspection_error
        );
        assert_eq!(outcome.removed.len(), 1);

        // Drive the partial outcome through the host reporting path: the
        // failure is reported AND each earlier removal published once.
        let logs = crate::test_log_capture::capture_logs(|| {
            let ((), captured) = crate::observability::ops::capture::capture(|| {
                report_seat("agent-1", &outcome, std::time::Duration::from_secs(1));
            });
            let removals: Vec<_> = captured
                .spans
                .iter()
                .filter(|span| span.name.as_str() == "reaper.removal")
                .collect();
            assert_eq!(removals.len(), 1, "each earlier removal exactly once");
            let summaries: Vec<_> = captured
                .spans
                .iter()
                .filter(|span| span.name.as_str() == "reaper.pass")
                .map(|span| span.attributes["loom.reaper.phase"].as_str())
                .collect();
            assert_eq!(summaries, ["reap", "reclaim"], "both summaries stand");
            assert_eq!(removals[0].attributes["loom.reaper.removal.path"], "cache/sta-6421");
        });
        assert!(
            logs.iter().any(|(level, line)| *level == log::Level::Warn
                && line.contains("agent-1")
                && line.contains("Git inspection error")),
            "the host reports the failure: {logs:?}"
        );
        assert!(
            logs.iter()
                .any(|(_, line)| line.contains("agent-1") && line.contains("cache/sta-6421")),
            "the host still logs the removed item: {logs:?}"
        );
    }

    // ===================================================================
    // The merged-only row classification the verb's lookup builds on
    // (#14678: closed-unmerged is preserved here, finished-at-head is not
    // the same question).
    // ===================================================================

    fn row(
        state: &str,
        merged_at: Option<&str>,
        closed_at: Option<&str>,
        sha: &str,
    ) -> crate::worktree_ops::clean_owner::PrRowRest {
        serde_json::from_value(serde_json::json!({
            "state": state,
            "merged_at": merged_at,
            "closed_at": closed_at,
            "head": {"ref": "feature/x", "sha": sha, "repo": {"full_name": "Owner/Repo"}},
            "base": {"repo": {"full_name": "Owner/Repo"}},
        }))
        .unwrap()
    }

    /// The one shape every eligible sibling is built from: closed and merged
    /// at `sha`.
    fn merged(sha: &str) -> crate::worktree_ops::clean_owner::PrRowRest {
        row("closed", Some("2020-01-01T00:00:00Z"), Some("2020-01-01T00:00:00Z"), sha)
    }

    #[test]
    fn pr_at_head_rows_are_merged_only_and_open_wins() {
        assert_eq!(
            pr_at_head_in_rows(&[merged("tip")], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::Merged("2020-01-01T00:00:00Z".into())
        );
        // A newer open PR on the same branch takes precedence over the merge.
        let open = row("open", None, None, "other");
        assert_eq!(
            pr_at_head_in_rows(&[merged("tip"), open], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::Open
        );
        // Closed without merging at the tip: preserved by the seat pass.
        let closed = row("closed", None, Some("2020-01-01T00:00:00Z"), "tip");
        assert_eq!(
            pr_at_head_in_rows(&[closed], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::ClosedNoMerge
        );
        // A merge at a different tip leaves no merge at this tip.
        assert_eq!(
            pr_at_head_in_rows(&[merged("older")], "Owner/Repo", "feature/x", "tip"),
            PrAtHead::NoPr
        );
        // A truncated or foreign record is never evidence.
        assert_eq!(
            pr_at_head_in_rows(&[merged("tip")], "Other/Repo", "feature/x", "tip"),
            PrAtHead::Unknown
        );
        let many: Vec<_> = (0..100).map(|i| merged(&format!("tip{i}"))).collect();
        assert_eq!(pr_at_head_in_rows(&many, "Owner/Repo", "feature/x", "tip"), PrAtHead::Unknown);
    }

    // ===================================================================
    // The host report folds seat outcomes
    // ===================================================================

    #[test]
    fn the_host_wide_cooldown_suppresses_only_a_recent_pass() {
        let now = Utc::now();
        assert!(cooldown_skip(None, now, 1800).is_none(), "a first pass runs");
        assert!(
            cooldown_skip(Some(now - chrono::Duration::seconds(60)), now, 1800).is_some(),
            "a pass 60s ago is inside a 30m cooldown"
        );
        let held = cooldown_skip(Some(now - chrono::Duration::seconds(1800)), now, 1800);
        assert!(held.is_none(), "a pass exactly one cooldown old re-runs: {held:?}");
        // Clock skew backwards never wedges the cooldown shut.
        assert!(cooldown_skip(Some(now + chrono::Duration::hours(1)), now, 1800).is_none());
    }

    #[test]
    fn report_sums_across_seats() {
        let one = SeatReclaimOutcome {
            worktrees_scanned: 3,
            worktrees_removed: 1,
            worktrees_failed: 0,
            preserved: vec!["PR still open".into()],
            bytes: 100,
            scratch_removed: 2,
            scratch_scanned: 2,
            scratch_bytes: 40,
            removed: Vec::new(),
            inspection_error: None,
        };
        let report = SeatReclaimReport {
            skipped: None,
            seats: vec![
                SeatWalkResult {
                    account: "agent-1".into(),
                    result: Ok(one),
                    elapsed: std::time::Duration::from_secs(3),
                },
                SeatWalkResult {
                    account: "agent-2".into(),
                    result: Err("seat busy".into()),
                    elapsed: std::time::Duration::from_millis(12),
                },
            ],
        };
        assert_eq!(report.worktrees_removed(), 1);
        assert_eq!(report.scratch_removed(), 2);
        assert_eq!(report.bytes(), 100);
        assert!(report.summary().contains("worktree"));
        let skipped = SeatReclaimReport {
            skipped: Some("disabled".into()),
            seats: Vec::new(),
        };
        assert!(skipped.summary().contains("disabled"));
    }

    // ===================================================================
    // Off the dispatch tick (#14777): scheduling, single-flight, recovery
    // ===================================================================

    /// Bounded wait for `cond` — the failure guard the #14777 test plan
    /// names: these tests gate a walk on a channel, so an unbounded wait
    /// would hang the binary instead of failing it.
    fn soon(cond: impl Fn() -> bool) -> bool {
        for _ in 0..2_000 {
            if cond() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    /// A walk body that signals each invocation and then holds the walk open
    /// until the test drops the release sender — the gate the single-flight
    /// tests open from the test side. The body is one shared [`Arc`], so
    /// every trigger handed a clone schedules the *same* counting walk.
    struct HeldWalk {
        release: Option<std::sync::mpsc::Sender<()>>,
        started: std::sync::mpsc::Receiver<PathBuf>,
        invocations: Arc<std::sync::atomic::AtomicUsize>,
        body: WalkBody,
    }

    impl HeldWalk {
        fn new() -> Self {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let (started_tx, started_rx) = std::sync::mpsc::channel::<PathBuf>();
            let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let inv = Arc::clone(&invocations);
            let release = std::sync::Mutex::new(release_rx);
            let body: WalkBody = Arc::new(move |root| {
                inv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                started_tx.send(root).unwrap();
                // Hold the walk open: the trigger that scheduled it has long
                // since returned.
                let _ = release.lock().unwrap().recv();
                SeatReclaimReport {
                    skipped: None,
                    seats: Vec::new(),
                }
            });
            Self {
                release: Some(release_tx),
                started: started_rx,
                invocations,
                body,
            }
        }

        fn body(&self) -> WalkBody {
            Arc::clone(&self.body)
        }

        /// Open the gate: the held walk's `recv` fails and the worker exits.
        fn open_the_gate(&mut self) {
            self.release = None;
        }
    }

    /// AC: one walk host-wide. The first eligible trigger schedules the
    /// sequential walk on a blocking worker and returns without awaiting it;
    /// a trigger from a second root — even past the cooldown — finds the
    /// walk in flight and queues nothing, including after the worker is
    /// released. A later eligible trigger (cooldown elapsed) can start a
    /// new walk.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_walk_is_single_flight_across_roots_and_never_queues_a_duplicate() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let root_a = tmp.path().join("a");
        let root_b = tmp.path().join("b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let t0 = Utc::now();
        let mut walk = HeldWalk::new();

        // The first trigger returns immediately — this is the dispatch tick
        // advancing past a walk that is still blocked.
        let started = std::time::Instant::now();
        assert_eq!(schedule_with(&root_a, t0, walk.body()), SeatScheduling::Pending);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "scheduling must not await the walk"
        );
        assert!(walk_in_flight_for_test());
        let walked_root = walk
            .started
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the scheduled walk must start");
        assert_eq!(walked_root, root_a, "the originating root reaches the body");

        // A second trigger from another root, past the cooldown, while the
        // first walk is still held: pending, not a second walk.
        assert_eq!(
            schedule_with(&root_b, t0 + chrono::Duration::seconds(3600), walk.body()),
            SeatScheduling::Pending
        );
        assert_eq!(
            walk.invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a walk in flight absorbs a past-cooldown trigger from another root"
        );

        // Release the worker; the watch for a queued duplicate keeps polling
        // until the walk has fully exited, so nothing can slip in behind it.
        walk.open_the_gate();
        assert!(soon(|| !walk_in_flight_for_test()), "the walk must finish after release");
        assert_eq!(
            walk.invocations.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no duplicate walk was queued behind the in-flight one"
        );

        // The cooldown was recorded at schedule time, so a retrigger inside
        // that window is declined without seat work…
        assert!(matches!(
            schedule_with(&root_a, t0 + chrono::Duration::seconds(60), walk.body()),
            SeatScheduling::Skipped(reason)
                if reason.contains("cooldown"),
        ));

        // …while a later eligible trigger starts a new walk.
        assert_eq!(
            schedule_with(&root_b, t0 + chrono::Duration::seconds(7200), walk.body()),
            SeatScheduling::Pending
        );
        assert!(soon(|| { walk.invocations.load(std::sync::atomic::Ordering::SeqCst) == 2 }));
        // The released channel makes the second walk exit immediately.
        assert!(soon(|| !walk_in_flight_for_test()));
    }

    /// AC: a disabled pass performs no seat work — the trigger declines
    /// before anything is scheduled.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_disabled_pass_performs_no_seat_work() {
        reset_state_for_test();
        std::env::set_var(crate::scratch_reclaim::SCRATCH_RECLAIM_ENABLE_ENV, "0");
        let tmp = tempfile::tempdir().unwrap();
        let scheduling = schedule_with(
            tmp.path(),
            Utc::now(),
            Arc::new(|_| panic!("a disabled pass must not run the walk body")),
        );
        std::env::remove_var(crate::scratch_reclaim::SCRATCH_RECLAIM_ENABLE_ENV);
        assert!(
            matches!(&scheduling, SeatScheduling::Skipped(reason) if reason.contains("disabled")),
            "got {scheduling:?}"
        );
        assert!(!walk_in_flight_for_test());
    }

    /// AC: completion or **worker failure** permits a later eligible pass —
    /// the in-flight marker is cleared by the drop guard even when the
    /// blocking worker panics, so one failed walk cannot wedge the host.
    #[tokio::test]
    #[serial_test::serial]
    async fn a_failed_worker_releases_the_flight_marker() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let t0 = Utc::now();
        let invocations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let inv = Arc::clone(&invocations);
        let body: WalkBody = Arc::new(move |_: PathBuf| {
            inv.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            panic!("the injected worker failure");
        });

        assert_eq!(schedule_with(tmp.path(), t0, Arc::clone(&body)), SeatScheduling::Pending);
        assert!(
            soon(|| !walk_in_flight_for_test()),
            "a panicked walk must clear the in-flight marker"
        );
        assert_eq!(invocations.load(std::sync::atomic::Ordering::SeqCst), 1);

        // A later eligible trigger (cooldown elapsed) starts a new walk.
        assert_eq!(
            schedule_with(tmp.path(), t0 + chrono::Duration::seconds(3600), Arc::clone(&body)),
            SeatScheduling::Pending
        );
        assert!(soon(|| invocations.load(std::sync::atomic::Ordering::SeqCst) == 2));
        assert!(soon(|| !walk_in_flight_for_test()));
    }
}
