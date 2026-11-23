//! Reaper for agent-created per-ticket scratch roots (#14952): the
//! `loom-<ticket>-<role>.<random>` directories agent sessions leave at the
//! top of the worktree volume, `$HOME`, `~/.cache` and `$TMPDIR`. Nothing
//! else owns them — `scratch_reclaim` only handles
//! `<repo>/.loom/claude-config/<agent>/tmp`, `agent_residue_reaper` reaps
//! processes and scopes (not directories), and `worktree_reaper` only walks
//! `<repo>/.loom/worktrees/issue-<N>` — so on a shared volume they
//! accumulate (~300/day, 1.5 TB in three days on the reporting host) until
//! the volume fills and the fleet stalls on disk-full.
//!
//! # Discovery, and why it is deliberately narrow
//!
//! Candidates are **direct children only** of an explicit root list (the
//! mount-volume top holding `<repo>/.loom/worktrees`, `$HOME`,
//! `$HOME/.cache`, `$TMPDIR`) whose name matches
//! [`ROOT_NAME_RE`]. The pass never recurses to *find* candidates and never
//! follows symlinks; a candidate that is itself a symlink or sits on a
//! different device than its parent (a volume mounted at the candidate path)
//! is preserved — deleting through either would cross a boundary the name
//! pattern did not describe.
//!
//! # Ticket → repo, and fail-closed everywhere
//!
//! The directory name carries no repository, so the repo is resolved from the
//! `origin` remote of a git worktree found *inside* the root (they sit as
//! deep as `<root>/worktrees/loom/issue-<N>/.git`, so the search is a bounded
//! recursive walk, not a shallow one). No worktree, or no GitHub-shaped
//! `origin`, means the root is preserved — an unattributable directory is
//! never removed on a guess.
//!
//! Every gate must **prove** eligibility; any unreadable answer preserves:
//!
//! 1. the linked ticket is `CLOSED`/`MERGED` on the resolved repo (a probe
//!    failure is `UNKNOWN` → preserve);
//! 2. a `watch-pid` file at the root's top level names a dead process (absent
//!    passes; unparseable is treated as alive);
//! 3. no process has its cwd or an open fd under the root
//!    ([`crate::worktree_ops::safety::find_processes_using_directory`]);
//! 4. nothing under the root changed within the idle window (24 h), verified
//!    by a full walk — an unreadable subtree is `Unknown` → preserve;
//! 5. every git worktree inside is clean **and** its `HEAD` is reachable from
//!    a remote ref (`git rev-list --count HEAD --not --remotes` == 0, the
//!    canonical [`crate::worktree_ops::clean::branch_reachable_from_remotes`]
//!    probe — never `--branches`, which counts every local branch of the
//!    shared repo, not just this worktree's commits);
//!    a **failed** `git status` (e.g. a worktree whose parent repo is gone)
//!    counts as "not safe", never as clean;
//! 6. the root itself is at least `scratchReclaim.maxAgeHours` old.
//!
//! # Removal mechanics (the hand-cleanup gotchas, implemented as stated)
//!
//! Read-only trees (Go module caches, cert stores) need `chmod -R u+w`
//! before removal; deleting a worktree directory leaves a stale entry in its
//! parent repo, so the pass runs `git worktree prune` on every parent repo
//! that still exists afterwards.
//!
//! # Reporting
//!
//! Each removal (and, under `dryRun`, each *planned* removal) is logged and
//! published on the event bus as `daemon.scratch_root.reaped` with
//! `{path, bytes, ticket, repo, dry_run}` — the record shape #14889 asks the
//! seat pass for, which had not merged when this was written. The pass
//! summary logs removed count/bytes and every preserved root with its reason.
//!
//! # Config surface
//!
//! Shares `autonomous.worktreeReaper.scratchReclaim` with
//! [`crate::scratch_reclaim`] (`enabled` / `maxAgeHours` / `minIntervalSecs`
//! plus this pass's `dryRun`), so one block governs both scratch reapers and
//! the existing `LOOM_SCRATCH_RECLAIM*` env overrides keep working. Wired on
//! the scheduled `worktree_reaper` tick and the eager tier-1 pass, gated by
//! its own 30-minute per-repo cooldown.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use regex::Regex;

use crate::fs_util::human_size;

/// The topic published for every (planned) root removal.
pub const EVENT_TOPIC: &str = "daemon.scratch_root.reaped";

/// The idle window every gate-4 walk uses: nothing under a root may have
/// changed for 24 h before the root is eligible. Fixed by the issue's
/// acceptance ("no file under it changed in the last 24 h"), independent of
/// the `maxAgeHours` floor.
pub const IDLE_WINDOW_SECS: i64 = 24 * 3600;

/// How deep below a scratch root the worktree search looks for `.git`.
/// `worktrees/loom/issue-<N>/.git` sits three levels down, `source/.git` one;
/// six leaves room for one more nesting without walking entire 100k-file
/// trees to their bottom.
const WORKTREE_SEARCH_MAX_DEPTH: usize = 6;

/// Bound on entries one idle walk visits. Examining this many entries
/// without finding a recent write is positive evidence of idleness, not a
/// failure to answer — the same rationale as
/// `worktree_activity::MAX_ENTRIES_SCANNED`, at a larger bound because a
/// scratch root holds whole worktrees, not one source tree.
const IDLE_WALK_ENTRY_CAP: usize = 50_000;

// ============================================================================
// Name matching
// ============================================================================

/// `loom-<ticket>-<role>.<random>` — the shape agent sessions name their
/// per-ticket scratch roots after. The random suffix is 4+ alphanumeric
/// characters so a plain `loom-42-builder` (no suffix) is never a candidate.
static ROOT_NAME_RE: OnceLock<Regex> = OnceLock::new();

fn root_name_re() -> &'static Regex {
    ROOT_NAME_RE.get_or_init(|| {
        Regex::new(r"^loom-(\d+)-([a-z][a-z0-9-]*)\.([A-Za-z0-9]{4,})$")
            .expect("static scratch-root name regex compiles")
    })
}

/// The ticket and role parsed from a candidate root's directory name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchRootName {
    pub ticket: u32,
    pub role: String,
}

/// Parse `loom-<ticket>-<role>.<random>` into its ticket and role. `None`
/// for anything else — including a ticket number too large for `u32`.
#[must_use]
pub fn parse_root_name(name: &str) -> Option<ScratchRootName> {
    let caps = root_name_re().captures(name)?;
    Some(ScratchRootName {
        ticket: caps.get(1)?.as_str().parse().ok()?,
        role: caps.get(2)?.as_str().to_string(),
    })
}

// ============================================================================
// Candidate roots and discovery
// ============================================================================

/// The topmost directory of the mount volume holding `path`: walk up while
/// the device id is unchanged. On the reporting host this turns
/// `/Volumes/Stripe/agents/loom/.loom/worktrees` into `/Volumes/Stripe`,
/// which is where the leaked roots sit. `None` when the path does not exist
/// or its device cannot be read (non-Unix: no volume root is derived).
fn volume_root_of(path: &Path) -> Option<PathBuf> {
    let mut cur = path.canonicalize().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let mut dev = std::fs::metadata(&cur).ok()?.dev();
        while let Some(parent) = cur.parent() {
            let Ok(pmeta) = std::fs::metadata(parent) else {
                break;
            };
            if pmeta.dev() != dev {
                break;
            }
            dev = pmeta.dev();
            cur = parent.to_path_buf();
        }
    }
    #[cfg(not(unix))]
    {
        return None;
    }
    Some(cur)
}

/// The explicit root list candidates are discovered in: the mount-volume top
/// holding the repo's worktree root, `$HOME`, `$HOME/.cache` and `$TMPDIR`
/// (or `/tmp`). Deduplicated and canonicalized; non-existent entries drop
/// out silently.
#[must_use]
pub fn candidate_roots(repo_root: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(top) = volume_root_of(&repo_root.join(".loom").join("worktrees")) {
        roots.push(top);
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            roots.push(PathBuf::from(&home));
            roots.push(Path::new(&home).join(".cache"));
        }
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    if !tmp.is_empty() {
        roots.push(PathBuf::from(tmp));
    }
    let mut seen = BTreeSet::new();
    roots
        .into_iter()
        .filter_map(|r| r.canonicalize().ok())
        .filter(|r| r.is_dir())
        .filter(|r| seen.insert(r.clone()))
        .collect()
}

/// Every direct child directory (or symlink) of `roots` whose name matches
/// [`ROOT_NAME_RE`].
/// Symlinked entries are included here so the gate layer can *report* them
/// as preserved (never followed, never removed); the pass never recurses to
/// find candidates.
#[must_use]
pub fn discover_candidates(roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().into_string().ok() else {
                continue;
            };
            // Plain files (`loom-<n>-<slug>.patch`) match the name pattern
            // but are not scratch roots; symlinks stay so they are reported.
            let is_root_shaped = entry
                .file_type()
                .is_ok_and(|t| t.is_dir() || t.is_symlink());
            if is_root_shaped && parse_root_name(&name).is_some() {
                out.push(entry.path());
            }
        }
    }
    out
}

// ============================================================================
// Gates
// ============================================================================

/// Whether anything under `root` changed within `window_secs`, from a full
/// recursive walk. Symlinks are never followed (a link's own mtime is still
/// examined); a future-dated mtime reads as age zero (maximally recent), the
/// same clock-skew convention as `worktree_activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleProbe {
    /// Something under the root was written within the window (payload:
    /// how long ago, in seconds; 0 for a future mtime).
    Recent(u64),
    /// The walk completed and found nothing within the window — including
    /// the capped-walk case, where examining [`IDLE_WALK_ENTRY_CAP`] entries
    /// without a recent write is positive evidence of idleness.
    Idle,
    /// The walk could not answer (unreadable root/subtree) — preserved,
    /// never treated as idle.
    Unknown,
}

/// One entry's mtime as epoch seconds, or `None` if unreadable.
fn mtime_epoch(path: &Path) -> Option<i64> {
    let mtime: SystemTime = std::fs::symlink_metadata(path).ok()?.modified().ok()?;
    mtime
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
}

/// The recursive idle walk behind gate 4. Fails closed: an unreadable
/// subtree answers [`IdleProbe::Unknown`], never a completed-looking walk.
fn walk_idle(path: &Path, cutoff_epoch: i64, now_epoch: i64, visited: &mut usize) -> IdleProbe {
    *visited += 1;
    if *visited > IDLE_WALK_ENTRY_CAP {
        return IdleProbe::Idle;
    }
    let Some(mtime) = mtime_epoch(path) else {
        return IdleProbe::Unknown;
    };
    if mtime >= cutoff_epoch || mtime > now_epoch {
        return IdleProbe::Recent(0);
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        return IdleProbe::Unknown;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return IdleProbe::Unknown;
        };
        let child = entry.path();
        let is_symlink = entry.file_type().is_ok_and(|t| t.is_symlink());
        if is_symlink {
            // Never follow a symlink; its own mtime still counts.
            let Some(m) = mtime_epoch(&child) else {
                return IdleProbe::Unknown;
            };
            if m >= cutoff_epoch || m > now_epoch {
                return IdleProbe::Recent(0);
            }
            continue;
        }
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            let probe = walk_idle(&child, cutoff_epoch, now_epoch, visited);
            if probe != IdleProbe::Idle {
                return probe;
            }
        } else {
            let Some(m) = mtime_epoch(&child) else {
                return IdleProbe::Unknown;
            };
            if m >= cutoff_epoch || m > now_epoch {
                return IdleProbe::Recent(0);
            }
        }
    }
    IdleProbe::Idle
}

/// Gate 4's entry point: nothing under `root` changed in the last
/// `window_secs` (relative to `now`).
#[must_use]
pub fn probe_idle(root: &Path, window_secs: i64, now: DateTime<Utc>) -> IdleProbe {
    if window_secs <= 0 {
        return IdleProbe::Idle;
    }
    let now_epoch = now.timestamp();
    let cutoff_epoch = now_epoch.saturating_sub(window_secs);
    let mut visited = 0usize;
    walk_idle(root, cutoff_epoch, now_epoch, &mut visited)
}

/// Every directory under `root` (bounded depth, no symlink follow, never
/// descending into a `.git` directory's contents) that carries a `.git`
/// entry — a linked worktree's `.git` FILE or a main repo's `.git` DIR.
/// `None` when the walk hit an unreadable subtree: the "every worktree is
/// safe" claim is then unprovable, and the caller must preserve the root.
#[must_use]
pub fn find_git_worktrees(root: &Path) -> Option<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return None;
        };
        let mut has_git = false;
        for entry in entries {
            let Ok(entry) = entry else {
                return None;
            };
            let name = entry.file_name();
            let child = entry.path();
            // Never follow symlinks, and never descend into a .git dir.
            if entry.file_type().is_ok_and(|t| t.is_symlink()) {
                continue;
            }
            if name == ".git" {
                has_git = true;
                continue;
            }
            if depth < WORKTREE_SEARCH_MAX_DEPTH && entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push((child, depth + 1));
            }
        }
        if has_git {
            out.push(dir);
        }
    }
    Some(out)
}

/// Gate 5's per-worktree verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeSafety {
    Safe,
    NotSafe(&'static str),
}

/// One worktree inside a scratch root is safe to lose iff its `git status`
/// **succeeds** and reports no tracked change (a failed status — e.g. a
/// worktree whose parent repo is gone — is "not safe", never clean), and its
/// `HEAD` is reachable from a remote ref.
///
/// Dirtiness is answered by one `git status --porcelain` call rather than
/// [`crate::worktree_ops::safety::check_uncommitted_changes`]: that probe
/// deliberately fails toward "no changes" (its worktree-reaper callers layer
/// other gates on top), while this pass has no other gate between a dirty
/// worktree and `remove_dir_all`, so the failure direction must invert here.
/// The published half reuses the canonical
/// [`crate::worktree_ops::clean::branch_reachable_from_remotes`] probe —
/// `git rev-list --count HEAD --not --remotes`, never `--branches` (which
/// counts every local branch of the shared repo, not this worktree's).
///
/// `--no-optional-locks` is load-bearing, not hygiene: a plain `git status`
/// opportunistically refreshes and **writes** the index, which would stamp
/// a fresh mtime under the root on every pass — the 24h idle gate would then
/// never let a root the pass itself looked at become eligible.
#[must_use]
pub fn worktree_safety(worktree: &Path) -> WorktreeSafety {
    let out = Command::new("git")
        .args(["--no-optional-locks", "-C"])
        .arg(worktree)
        .args(["status", "--porcelain"])
        .output();
    let Ok(out) = out else {
        return WorktreeSafety::NotSafe("git status failed to run");
    };
    if !out.status.success() {
        return WorktreeSafety::NotSafe("git status failed");
    }
    let tracked_change = String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| !line.starts_with("??"));
    if tracked_change {
        return WorktreeSafety::NotSafe("worktree has tracked changes");
    }
    if !crate::worktree_ops::clean::branch_reachable_from_remotes(worktree, "HEAD") {
        return WorktreeSafety::NotSafe("worktree HEAD has commits no remote holds");
    }
    // A self-contained clone (`.git` is a DIRECTORY) owns its whole ref
    // namespace, so `HEAD` alone does not prove its work is published: a
    // commit on a non-`HEAD` local branch, or a stash, is invisible to both
    // probes above. The linked-worktree form (`.git` FILE) shares its
    // parent's refs and keeps the HEAD-only probe, which is exactly why
    // `--branches` is wrong there.
    if std::fs::symlink_metadata(worktree.join(".git")).is_ok_and(|m| m.is_dir()) {
        if !clone_branches_published(worktree) {
            return WorktreeSafety::NotSafe("clone has local branches no remote holds");
        }
        if clone_has_stash(worktree) {
            return WorktreeSafety::NotSafe("clone has a stash");
        }
    }
    WorktreeSafety::Safe
}

/// `git rev-list --count --branches --not --remotes == 0` in a self-contained
/// clone. Fail-closed: any failure to run or parse reads as "not published".
fn clone_branches_published(clone: &Path) -> bool {
    let out = Command::new("git")
        .args(["--no-optional-locks", "-C"])
        .arg(clone)
        .args(["rev-list", "--count", "--branches", "--not", "--remotes"])
        .output();
    let Ok(out) = out else { return false };
    out.status.success()
        && String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u64>()
            .is_ok_and(|n| n == 0)
}

/// Whether a self-contained clone has a `refs/stash` entry. Fail-closed: a
/// failure to ask reads as "has a stash".
fn clone_has_stash(clone: &Path) -> bool {
    let out = Command::new("git")
        .args(["--no-optional-locks", "-C"])
        .arg(clone)
        .args(["rev-parse", "--verify", "--quiet", "refs/stash"])
        .output();
    let Ok(out) = out else { return true };
    // 0 = ref exists; 1 = `--quiet` and absent; anything else = could not ask.
    out.status.code() != Some(1)
}

/// The main repo a worktree belongs to, for `git worktree prune`: a `.git`
/// DIR means the worktree *is* the main repo; a `.git` FILE points at
/// `<main>/.git/worktrees/<name>`. `None` when the pointer cannot be parsed.
#[must_use]
pub fn parent_repo_of(worktree: &Path) -> Option<PathBuf> {
    let dot = worktree.join(".git");
    let Ok(meta) = std::fs::symlink_metadata(&dot) else {
        return None;
    };
    if meta.is_dir() {
        return Some(worktree.to_path_buf());
    }
    let text = std::fs::read_to_string(&dot).ok()?;
    let target = text.trim().strip_prefix("gitdir:")?.trim();
    let mut gitdir = PathBuf::from(target);
    if gitdir.is_relative() {
        gitdir = worktree.join(gitdir);
    }
    // gitdir = <main>/.git/worktrees/<name>
    let worktrees_dir = gitdir.parent()?;
    if worktrees_dir.file_name()?.to_str()? != "worktrees" {
        return None;
    }
    let main_git = worktrees_dir.parent()?;
    main_git.parent().map(Path::to_path_buf)
}

/// The `origin` remote URL of `worktree`, or `None` (no origin / git failed).
#[must_use]
pub fn origin_url(worktree: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

/// `owner/repo` for a GitHub `origin` URL (`https://` or `ssh`/`scp` forms),
/// or `None` for any other host/shape — a non-GitHub forge is unresolvable
/// and the root is preserved, never removed on a guess.
#[must_use]
pub fn github_slug_from_url(url: &str, gh_host: &str) -> Option<String> {
    let (host, path) = if let Some(rest) = url.strip_prefix("https://") {
        let (host, path) = rest.split_once('/')?;
        (host.split(':').next()?.to_string(), path.to_string())
    } else if let Some(rest) = url.strip_prefix("ssh://") {
        let rest = rest.strip_prefix("git@").unwrap_or(rest);
        let (host, path) = rest.split_once('/')?;
        (host.to_string(), path.to_string())
    } else {
        let rest = url.strip_prefix("git@")?;
        let (host, path) = rest.split_once(':')?;
        (host.to_string(), path.to_string())
    };
    if host != gh_host {
        return None;
    }
    let path = path.trim_matches('/');
    let name = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = name.split_once('/')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// The GitHub host scratch-root repo resolution accepts.
#[must_use]
fn gh_host() -> String {
    std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".to_string())
}

/// Gate 2's watch-pid reading. Fails closed: an existing `watch-pid` file
/// that cannot be read or parsed counts as **alive** — only a dead pid or
/// no file at all passes the gate.
enum WatchPidState {
    Absent,
    Dead,
    Alive,
    Unreadable,
}

fn watch_pid_state(root: &Path, pid_alive: &dyn Fn(u32) -> bool) -> WatchPidState {
    let marker = root.join("watch-pid");
    if std::fs::symlink_metadata(&marker).is_err() {
        return WatchPidState::Absent;
    }
    let Ok(text) = std::fs::read_to_string(&marker) else {
        return WatchPidState::Unreadable;
    };
    match text.trim().parse::<u32>() {
        Ok(pid) if pid_alive(pid) => WatchPidState::Alive,
        Ok(_) => WatchPidState::Dead,
        Err(_) => WatchPidState::Unreadable,
    }
}

/// One candidate's verdict: every gate proven eligible, or preserved with
/// the first reason that failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootVerdict {
    Remove {
        ticket: u32,
        repo: String,
        bytes: u64,
        /// Parent repos to `git worktree prune` after removal.
        prune: BTreeSet<PathBuf>,
    },
    Preserve(&'static str),
}

/// Injected probes so the gate chain is testable without a live forge,
/// process table or device view — the same shape
/// `worktree_ops::clean::WorktreeProbes` uses.
pub struct ScratchRootProbes<'a> {
    /// Wall clock the age/idle gates measure against.
    pub now: DateTime<Utc>,
    /// Gate-4 idle window (seconds).
    pub idle_window_secs: i64,
    /// Gate-6 age floor (seconds) — `scratchReclaim.maxAgeHours`.
    pub max_age_secs: i64,
    /// Whether `<owner>/<repo>`'s ticket is finished (`CLOSED`/`MERGED`).
    /// `None` = the probe could not answer (preserve).
    pub ticket_finished: &'a dyn Fn(&Path, &str, u32) -> Option<bool>,
    /// Whether a pid is alive (gate 2).
    pub pid_alive: &'a dyn Fn(u32) -> bool,
    /// Pids with cwd or an open fd under a path (gate 3).
    pub processes_using: &'a dyn Fn(&Path) -> Vec<u32>,
    /// Device id of a path, for the mount-point refusal (gate on candidate
    /// vs its parent differing). `None` = cannot determine → gate passes
    /// (defense in depth; the canonical deployment is Linux, where it works).
    pub device_of: &'a dyn Fn(&Path) -> Option<u64>,
}

/// Apply every removal gate, cheapest first, to one candidate root. Pure
/// decision logic: removes nothing.
#[must_use]
pub fn evaluate_root(root: &Path, probes: &ScratchRootProbes<'_>) -> RootVerdict {
    let Some(name) = root
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(parse_root_name)
    else {
        return RootVerdict::Preserve("name no longer matches the scratch-root pattern");
    };
    match std::fs::symlink_metadata(root) {
        Ok(m) if m.file_type().is_symlink() => {
            return RootVerdict::Preserve("root is a symlink (never followed)");
        }
        Ok(m) if !m.is_dir() => return RootVerdict::Preserve("not a directory"),
        _ => {}
    }
    match ((probes.device_of)(root), root.parent().map(|p| (probes.device_of)(p))) {
        (Some(child_dev), Some(Some(parent_dev))) if child_dev != parent_dev => {
            return RootVerdict::Preserve("root is a mount point (device differs from parent)");
        }
        _ => {}
    }
    let Some(root_mtime) = mtime_epoch(root) else {
        return RootVerdict::Preserve("root mtime unreadable");
    };
    let root_age = probes.now.timestamp() - root_mtime;
    if !crate::scratch_reclaim::is_reclaimable_age(root_age, probes.max_age_secs) {
        return RootVerdict::Preserve("root younger than scratchReclaim.maxAgeHours");
    }
    match probe_idle(root, probes.idle_window_secs, probes.now) {
        IdleProbe::Recent(_) => {
            return RootVerdict::Preserve("files changed within the 24h idle window");
        }
        IdleProbe::Unknown => {
            return RootVerdict::Preserve("idle walk could not answer (unreadable subtree)");
        }
        IdleProbe::Idle => {}
    }
    let Some(worktrees) = find_git_worktrees(root) else {
        return RootVerdict::Preserve("worktree search could not answer (unreadable subtree)");
    };
    if worktrees.is_empty() {
        return RootVerdict::Preserve("no git worktree inside the root");
    }
    let mut prune = BTreeSet::new();
    for worktree in &worktrees {
        match worktree_safety(worktree) {
            WorktreeSafety::Safe => {
                if let Some(parent) = parent_repo_of(worktree) {
                    prune.insert(parent);
                }
            }
            WorktreeSafety::NotSafe(reason) => return RootVerdict::Preserve(reason),
        }
    }
    // Ticket → repo: the first worktree whose `origin` names a GitHub repo.
    // The resolving worktree is also the cwd the ticket probe runs `gh` in.
    let host = gh_host();
    let Some((probe_cwd, slug)) = worktrees.iter().find_map(|worktree| {
        let url = origin_url(worktree)?;
        let slug = github_slug_from_url(&url, &host)?;
        Some((worktree.clone(), slug))
    }) else {
        return RootVerdict::Preserve("no resolvable GitHub origin repository");
    };
    match watch_pid_state(root, probes.pid_alive) {
        WatchPidState::Alive | WatchPidState::Unreadable => {
            return RootVerdict::Preserve("watch-pid file names a live process");
        }
        WatchPidState::Dead | WatchPidState::Absent => {}
    }
    match (probes.ticket_finished)(&probe_cwd, &slug, name.ticket) {
        Some(true) => {}
        Some(false) => return RootVerdict::Preserve("linked ticket is open"),
        None => return RootVerdict::Preserve("linked ticket state unknown (probe failed)"),
    }
    if !(probes.processes_using)(root).is_empty() {
        return RootVerdict::Preserve("a process has its cwd or an open file under the root");
    }
    RootVerdict::Remove {
        ticket: name.ticket,
        repo: slug,
        bytes: dir_size_bytes(root),
        prune,
    }
}

// ============================================================================
// Removal mechanics
// ============================================================================

/// Total size in bytes of everything under `path` (symlinks not followed).
/// Read failures contribute `0` — informational, never a removal decision.
fn dir_size_bytes(path: &Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return total;
    };
    for entry in entries.flatten() {
        if let Ok(ft) = entry.file_type() {
            if ft.is_dir() {
                total += dir_size_bytes(&entry.path());
            } else if let Ok(meta) = entry.metadata() {
                total += meta.len();
            }
        }
    }
    total
}

/// Remove one scratch root: `chmod -R u+w` first (Go module caches and cert
/// trust stores are read-only; a failed chmod is not fatal — removal then
/// fails and is reported), then `remove_dir_all`, which does not follow
/// symlinks.
fn remove_root(path: &Path) -> std::io::Result<()> {
    let _ = Command::new("chmod").args(["-R", "u+w"]).arg(path).status();
    std::fs::remove_dir_all(path)
}

/// `git worktree prune` on every parent repo that still exists — deleting a
/// worktree dir leaves a stale entry behind. Best effort: a prune failure is
/// logged, never a pass failure.
fn prune_parent_repos(parents: &BTreeSet<PathBuf>) {
    for parent in parents {
        if !parent.is_dir() {
            continue;
        }
        let out = Command::new("git")
            .arg("-C")
            .arg(parent)
            .args(["worktree", "prune"])
            .output();
        match out {
            Ok(o) if !o.status.success() => {
                log::debug!(
                    "scratch_root_reaper: git worktree prune failed in {}: {:?}",
                    parent.display(),
                    String::from_utf8_lossy(&o.stderr).trim()
                );
            }
            Err(e) => {
                log::debug!(
                    "scratch_root_reaper: git worktree prune failed to run in {parent:?}: {e}"
                );
            }
            Ok(_) => {}
        }
    }
}

// ============================================================================
// Records and the pass report
// ============================================================================

/// One (planned) root removal — the #14889 record shape: path, size,
/// ticket, repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemovalRecord {
    pub path: PathBuf,
    pub ticket: u32,
    pub repo: String,
    pub bytes: u64,
    pub dry_run: bool,
}

impl RemovalRecord {
    /// The event payload.
    #[must_use]
    pub fn payload(&self) -> serde_json::Value {
        serde_json::json!({
            "source": "scratch-root-reaper",
            "path": self.path.display().to_string(),
            "ticket": self.ticket,
            "repo": self.repo,
            "bytes": self.bytes,
            "dry_run": self.dry_run,
        })
    }
}

/// Where removal records go (test recorder: a `Mutex<Vec<_>>`).
pub trait Recorder {
    fn removal(&self, record: &RemovalRecord);
}

impl Recorder for Mutex<Vec<RemovalRecord>> {
    fn removal(&self, record: &RemovalRecord) {
        self.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(record.clone());
    }
}

static BUS: OnceLock<Arc<crate::event_bus::EventBus>> = OnceLock::new();

/// Give the production recorder the daemon's event bus. Idempotent.
pub fn install_event_bus(bus: Arc<crate::event_bus::EventBus>) {
    let _ = BUS.set(bus);
}

/// Logs and publishes every removal (or, under `dryRun`, "would remove").
pub struct ProductionRecorder;

impl Recorder for ProductionRecorder {
    fn removal(&self, r: &RemovalRecord) {
        log::warn!(
            "scratch_root_reaper: {} {} ({}, ticket #{}, repo {}){}",
            if r.dry_run { "would remove" } else { "removed" },
            r.path.display(),
            human_size(r.bytes),
            r.ticket,
            r.repo,
            if r.dry_run { " (dry-run)" } else { "" }
        );
        if let Some(bus) = BUS.get() {
            let _ = bus.publish_generic(EVENT_TOPIC, r.payload());
        }
    }
}

/// One preserved root and the first gate that refused it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreservedRoot {
    pub path: PathBuf,
    pub reason: &'static str,
}

/// One pass's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchRootReport {
    /// The repo whose pass ran (roots are derived from it).
    pub repo_root: PathBuf,
    pub enabled: bool,
    pub dry_run: bool,
    /// How many candidate roots were evaluated.
    pub candidates: usize,
    /// Roots removed (empty under dry-run).
    pub removed: Vec<RemovalRecord>,
    /// Roots dry-run *would* remove (empty unless dry-run).
    pub planned: Vec<RemovalRecord>,
    /// Every preserved root with its reason.
    pub preserved: Vec<PreservedRoot>,
    /// Present when the cooldown skipped this evaluation.
    pub deferred: Option<String>,
    pub at: DateTime<Utc>,
}

impl ScratchRootReport {
    /// Total bytes removed or (dry-run) that would be freed.
    #[must_use]
    pub fn reclaimed_bytes(&self) -> u64 {
        self.removed
            .iter()
            .chain(self.planned.iter())
            .map(|r| r.bytes)
            .sum()
    }

    /// Human-readable freed size, e.g. `"41.2G"` or `"nothing"`.
    #[must_use]
    pub fn reclaimed_human(&self) -> String {
        if self.removed.is_empty() && self.planned.is_empty() {
            return "nothing".to_string();
        }
        human_size(self.reclaimed_bytes())
    }

    /// One-line summary for the eager-pass log line.
    #[must_use]
    pub fn summary(&self) -> String {
        let acted: usize = self.removed.len() + self.planned.len();
        format!(
            "{} root(s) ({}){}, {} preserved with reasons",
            acted,
            self.reclaimed_human(),
            if self.dry_run { " [dry-run]" } else { "" },
            self.preserved.len()
        )
    }
}

// ============================================================================
// The pass
// ============================================================================

/// Evaluate every candidate under `roots` and remove (or plan, under
/// `dry_run`) the eligible ones, recording each removal via `recorder`.
/// `repo_root` is only reporting provenance.
fn sweep(
    repo_root: &Path,
    roots: &[PathBuf],
    probes: &ScratchRootProbes<'_>,
    dry_run: bool,
    recorder: &dyn Recorder,
) -> ScratchRootReport {
    let mut report = ScratchRootReport {
        repo_root: repo_root.to_path_buf(),
        enabled: true,
        dry_run,
        candidates: 0,
        removed: Vec::new(),
        planned: Vec::new(),
        preserved: Vec::new(),
        deferred: None,
        at: probes.now,
    };
    for candidate in discover_candidates(roots) {
        report.candidates += 1;
        match evaluate_root(&candidate, probes) {
            RootVerdict::Remove {
                ticket,
                repo,
                bytes,
                prune,
            } => {
                if dry_run {
                    let record = RemovalRecord {
                        path: candidate,
                        ticket,
                        repo,
                        bytes,
                        dry_run: true,
                    };
                    recorder.removal(&record);
                    report.planned.push(record);
                } else if let Err(e) = remove_root(&candidate) {
                    log::warn!(
                        "scratch_root_reaper: could not remove {}: {e}",
                        candidate.display()
                    );
                    report.preserved.push(PreservedRoot {
                        path: candidate,
                        reason: "removal failed (logged, retried next pass)",
                    });
                } else {
                    let record = RemovalRecord {
                        path: candidate,
                        ticket,
                        repo,
                        bytes,
                        dry_run: false,
                    };
                    recorder.removal(&record);
                    report.removed.push(record);
                    prune_parent_repos(&prune);
                }
            }
            RootVerdict::Preserve(reason) => {
                report.preserved.push(PreservedRoot {
                    path: candidate,
                    reason,
                });
            }
        }
    }
    report
}

/// Log one pass's outcome. A pass that removed (or planned) something logs
/// at `info`; a no-op logs at `debug`.
pub fn log_report(report: &ScratchRootReport) {
    if !report.enabled {
        log::debug!(
            "scratch_root_reaper: {} disabled (autonomous.worktreeReaper.scratchReclaim)",
            report.repo_root.display()
        );
        return;
    }
    if let Some(reason) = &report.deferred {
        log::debug!("scratch_root_reaper: {} skipped: {reason}", report.repo_root.display());
        return;
    }
    if report.removed.is_empty() && report.planned.is_empty() {
        log::debug!(
            "scratch_root_reaper: {} {} — nothing eligible",
            report.repo_root.display(),
            report.summary()
        );
    } else {
        log::info!(
            "scratch_root_reaper: {} {} — preserved: {:?}",
            report.repo_root.display(),
            report.summary(),
            report
                .preserved
                .iter()
                .map(|p| format!("{} ({})", p.path.display(), p.reason))
                .collect::<Vec<_>>()
        );
    }
}

// ============================================================================
// Process-global per-repo cooldown state
// ============================================================================

static LAST_RUN_AT: OnceLock<Mutex<std::collections::BTreeMap<PathBuf, DateTime<Utc>>>> =
    OnceLock::new();

fn last_run_slot() -> &'static Mutex<std::collections::BTreeMap<PathBuf, DateTime<Utc>>> {
    LAST_RUN_AT.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()))
}

fn last_run_at(repo_root: &Path) -> Option<DateTime<Utc>> {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(repo_root)
        .copied()
}

fn record_run(repo_root: &Path, now: DateTime<Utc>) {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(repo_root.to_path_buf(), now);
}

/// Drop cooldown state. Test-only seam.
#[doc(hidden)]
pub fn reset_state_for_test() {
    last_run_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
}

/// The production ticket probe: REST issue state of `<owner>/<repo>`'s
/// ticket via the bounded gh facade, resolved from inside the scratch root's
/// own worktree (its `origin` names the repo; auth is the host credential).
fn prod_ticket_finished(cwd: &Path, slug: &str, ticket: u32) -> Option<bool> {
    let (owner, repo) = slug.split_once('/')?;
    match crate::worktree_ops::gh::foreign_issue_state_rest(cwd, owner, repo, ticket).as_str() {
        "CLOSED" => Some(true),
        "OPEN" => Some(false),
        _ => None,
    }
}

fn prod_pid_alive(pid: u32) -> bool {
    crate::daemon_install_state::pid_alive(pid)
}

fn prod_processes_using(path: &Path) -> Vec<u32> {
    crate::worktree_ops::safety::find_processes_using_directory(path)
}

fn prod_device_of(path: &Path) -> Option<u64> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(path).ok().map(|m| m.dev())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Production probes for [`evaluate_root`].
#[must_use]
pub fn production_probes(
    now: DateTime<Utc>,
    idle_window_secs: i64,
    max_age_secs: i64,
) -> ScratchRootProbes<'static> {
    ScratchRootProbes {
        now,
        idle_window_secs,
        max_age_secs,
        ticket_finished: &prod_ticket_finished,
        pid_alive: &prod_pid_alive,
        processes_using: &prod_processes_using,
        device_of: &prod_device_of,
    }
}

/// Run one production scratch-root pass for `repo_root`, honoring the shared
/// `scratchReclaim` config surface and its own cooldown, and log the result.
#[must_use]
pub fn run_for(repo_root: &Path) -> ScratchRootReport {
    run_for_roots(repo_root, &candidate_roots(repo_root))
}

/// [`run_for`] over an explicit root list — the seam tests use so they never
/// scan (or remove from) the real `$HOME` / `$TMPDIR` / worktree volume.
fn run_for_roots(repo_root: &Path, roots: &[PathBuf]) -> ScratchRootReport {
    let config = crate::scratch_reclaim::read_scratch_reclaim_config(repo_root);
    let enabled = crate::scratch_reclaim::resolve_enabled(&config);
    let dry_run = crate::scratch_reclaim::resolve_dry_run(&config);
    let now = Utc::now();

    if !enabled {
        let report = ScratchRootReport {
            repo_root: repo_root.to_path_buf(),
            enabled: false,
            dry_run,
            candidates: 0,
            removed: Vec::new(),
            planned: Vec::new(),
            preserved: Vec::new(),
            deferred: None,
            at: now,
        };
        log_report(&report);
        return report;
    }

    let min_interval_secs = crate::scratch_reclaim::resolve_min_interval_secs(&config);
    if let Some(last) = last_run_at(repo_root) {
        let since_secs = (now - last).num_seconds();
        if since_secs >= 0 && since_secs < i64::try_from(min_interval_secs).unwrap_or(i64::MAX) {
            let report = ScratchRootReport {
                repo_root: repo_root.to_path_buf(),
                enabled: true,
                dry_run,
                candidates: 0,
                removed: Vec::new(),
                planned: Vec::new(),
                preserved: Vec::new(),
                deferred: Some(format!(
                    "a pass ran {since_secs}s ago (cooldown {min_interval_secs}s)"
                )),
                at: now,
            };
            log_report(&report);
            return report;
        }
    }

    let max_age_secs =
        i64::try_from(crate::scratch_reclaim::resolve_max_age_hours(&config).saturating_mul(3600))
            .unwrap_or(i64::MAX);
    let probes = production_probes(now, IDLE_WINDOW_SECS, max_age_secs);
    let report = sweep(repo_root, roots, &probes, dry_run, &ProductionRecorder);
    record_run(repo_root, now);
    log_report(&report);
    report
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serial_test::serial;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?} in {} failed: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Set a path's mtime (and a directory tree's, recursively) to
    /// `epoch_secs` ago — pure Rust, no `touch`, so the fixture works
    /// everywhere the daemon builds.
    fn backdate(path: &Path, epoch_secs: i64) {
        let when = SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(u64::try_from(epoch_secs.max(0)).unwrap_or(0));
        fn set_one(p: &Path, when: SystemTime) {
            if let Ok(f) = std::fs::File::options().read(true).write(true).open(p) {
                let _ = f.set_modified(when);
                return;
            }
            if let Ok(f) = std::fs::File::open(p) {
                let _ = f.set_modified(when);
            }
        }
        set_one(path, when);
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    backdate(&p, epoch_secs);
                } else {
                    set_one(&p, when);
                }
            }
        }
    }

    /// Named probe fns (never temporaries) for the common stub set.
    fn always_finished(_: &Path, _: &str, _: u32) -> Option<bool> {
        Some(true)
    }
    fn never_alive(_: u32) -> bool {
        false
    }
    fn no_processes(_: &Path) -> Vec<u32> {
        Vec::new()
    }
    fn same_device(_: &Path) -> Option<u64> {
        Some(1)
    }

    fn probes_now(window: i64, max_age: i64) -> (ScratchRootProbes<'static>, DateTime<Utc>) {
        let now = Utc::now();
        (
            ScratchRootProbes {
                now,
                idle_window_secs: window,
                max_age_secs: max_age,
                ticket_finished: &always_finished,
                pid_alive: &never_alive,
                processes_using: &no_processes,
                device_of: &same_device,
            },
            now,
        )
    }

    // ===================================================================
    // parse_root_name
    // ===================================================================

    #[test]
    fn test_parse_root_name_valid() {
        let name = parse_root_name("loom-4242-builder.a1b2c3d4").unwrap();
        assert_eq!(name.ticket, 4242);
        assert_eq!(name.role, "builder");
        assert!(parse_root_name("loom-1-judge.abcd").is_some());
        assert!(parse_root_name("loom-12-builder-2.XyZ9").is_some());
    }

    #[test]
    fn test_parse_root_name_rejects_non_shapes() {
        assert!(parse_root_name("loom-4242-builder").is_none(), "no suffix");
        assert!(parse_root_name("loom-4242-builder.ab").is_none(), "short suffix");
        assert!(parse_root_name("loom-4242-Builder.abcd").is_none(), "uppercase role");
        assert!(parse_root_name("loom-4242-builder.a1b2c3d4!").is_none(), "suffix punct");
        assert!(parse_root_name("other-4242-builder.abcd").is_none());
        assert!(parse_root_name("loom--builder.abcd").is_none());
        assert!(
            parse_root_name(&format!("loom-{}-builder.abcd", u64::from(u32::MAX) + 1)).is_none(),
            "ticket overflow"
        );
    }

    // ===================================================================
    // github_slug_from_url
    // ===================================================================

    #[test]
    fn test_github_slug_from_url_forms() {
        assert_eq!(
            github_slug_from_url("https://github.com/acme/widgets.git", "github.com").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            github_slug_from_url("https://github.com/acme/widgets", "github.com").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            github_slug_from_url("git@github.com:acme/widgets.git", "github.com").as_deref(),
            Some("acme/widgets")
        );
        assert_eq!(
            github_slug_from_url("ssh://git@github.com/acme/widgets.git", "github.com").as_deref(),
            Some("acme/widgets")
        );
    }

    #[test]
    fn test_github_slug_from_url_rejects_other_hosts_and_shapes() {
        assert!(
            github_slug_from_url("git@gitlab.example.com:acme/widgets.git", "github.com").is_none()
        );
        assert!(github_slug_from_url("https://ghe.example.com/acme/widgets.git", "github.com")
            .is_none());
        assert!(github_slug_from_url("file:///srv/git/widgets", "github.com").is_none());
        assert!(github_slug_from_url("https://github.com/acme", "github.com").is_none());
    }

    // ===================================================================
    // probe_idle
    // ===================================================================

    #[test]
    fn test_probe_idle_recent_idle_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("old.txt"), b"old").unwrap();
        let now = Utc::now();
        let old = now.timestamp() - 48 * 3600;
        backdate(root, old);
        assert_eq!(probe_idle(root, 24 * 3600, now), IdleProbe::Idle);

        std::fs::write(root.join("new.txt"), b"new").unwrap();
        assert!(matches!(probe_idle(root, 24 * 3600, now), IdleProbe::Recent(_)));

        // Unreadable subtree -> Unknown (unless running as root, where the
        // 0000 dir is still readable and the walk completes as Idle).
        let secret = root.join("secret");
        std::fs::create_dir_all(&secret).unwrap();
        std::fs::write(secret.join("f"), b"x").unwrap();
        backdate(root, old);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).unwrap();
            let probe = probe_idle(root, 24 * 3600, now);
            assert!(
                matches!(probe, IdleProbe::Unknown | IdleProbe::Idle),
                "unreadable subtree is never Recent: {probe:?}"
            );
            std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    #[test]
    fn test_probe_idle_future_mtime_is_recent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("skewed"), b"x").unwrap();
        // The write above is at real now; probe with now in the past so the
        // file's mtime is in the future.
        let past = Utc::now() - chrono::Duration::hours(48);
        assert!(matches!(probe_idle(tmp.path(), 24 * 3600, past), IdleProbe::Recent(_)));
    }

    // ===================================================================
    // worktree_safety / parent_repo_of
    // ===================================================================

    #[test]
    fn test_worktree_safety_clean_published_is_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "--initial-branch=main"]);
        git(&work, &["config", "user.email", "loom@example.com"]);
        git(&work, &["config", "user.name", "Loom Test"]);
        std::fs::write(work.join("f"), b"v1").unwrap();
        git(&work, &["add", "f"]);
        git(&work, &["commit", "-q", "-m", "seed"]);
        git(&work, &["clone", "-q", "--bare", ".", origin.to_str().unwrap()]);
        git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&work, &["fetch", "-q", "origin"]);
        git(&work, &["branch", "--set-upstream-to=origin/main", "main"]);
        assert_eq!(worktree_safety(&work), WorktreeSafety::Safe);
    }

    #[test]
    fn test_worktree_safety_tracked_change_is_not_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "--initial-branch=main"]);
        git(&work, &["config", "user.email", "loom@example.com"]);
        git(&work, &["config", "user.name", "Loom Test"]);
        std::fs::write(work.join("f"), b"v1").unwrap();
        git(&work, &["add", "f"]);
        git(&work, &["commit", "-q", "-m", "seed"]);
        git(&work, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        assert_eq!(worktree_safety(&work), WorktreeSafety::Safe);
        std::fs::write(work.join("f"), b"v2 uncommitted").unwrap();
        assert_eq!(worktree_safety(&work), WorktreeSafety::NotSafe("worktree has tracked changes"));
    }

    #[test]
    fn test_worktree_safety_failed_status_is_not_safe_never_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        // A .git file pointing at a nonexistent parent repo: git commands fail.
        std::fs::write(work.join(".git"), "gitdir: /nonexistent/repo/.git/worktrees/w").unwrap();
        assert_eq!(worktree_safety(&work), WorktreeSafety::NotSafe("git status failed"));
    }

    #[test]
    fn test_worktree_safety_unpushed_head_is_not_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "--initial-branch=main"]);
        git(&work, &["config", "user.email", "loom@example.com"]);
        git(&work, &["config", "user.name", "Loom Test"]);
        std::fs::write(work.join("f"), b"v1").unwrap();
        git(&work, &["add", "f"]);
        git(&work, &["commit", "-q", "-m", "seed"]);
        // No remote at all: every commit is unpushed.
        assert_eq!(
            worktree_safety(&work),
            WorktreeSafety::NotSafe("worktree HEAD has commits no remote holds")
        );
    }

    /// A clone in the root (`.git` dir) with `main` published and an unpushed
    /// commit on a non-HEAD branch must be preserved (judge finding 1).
    fn clone_with_published_main(tmp: &Path) -> PathBuf {
        let origin = tmp.join("origin.git");
        let work = tmp.join("clone");
        std::fs::create_dir_all(&work).unwrap();
        git(&work, &["init", "-q", "--initial-branch=main"]);
        git(&work, &["config", "user.email", "loom@example.com"]);
        git(&work, &["config", "user.name", "Loom Test"]);
        std::fs::write(work.join("f"), b"v1").unwrap();
        git(&work, &["add", "f"]);
        git(&work, &["commit", "-q", "-m", "base"]);
        git(&work, &["clone", "-q", "--bare", ".", origin.to_str().unwrap()]);
        git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&work, &["fetch", "-q", "origin"]);
        git(&work, &["branch", "--set-upstream-to=origin/main", "main"]);
        work
    }

    #[test]
    fn test_worktree_safety_clone_with_unpushed_side_branch_is_not_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let work = clone_with_published_main(tmp.path());
        assert_eq!(worktree_safety(&work), WorktreeSafety::Safe);
        git(&work, &["checkout", "-q", "-b", "experiment"]);
        git(
            &work,
            &[
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "UNPUSHED EXPERIMENT WORK",
            ],
        );
        git(&work, &["checkout", "-q", "main"]);
        // HEAD (main) is published and the tree is clean; only the side branch is not.
        assert_eq!(
            worktree_safety(&work),
            WorktreeSafety::NotSafe("clone has local branches no remote holds")
        );
    }

    #[test]
    fn test_worktree_safety_clone_with_stash_is_not_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let work = clone_with_published_main(tmp.path());
        std::fs::write(work.join("f"), b"v2 stashed").unwrap();
        git(&work, &["stash", "push", "-q", "-m", "scratch-test-stash"]);
        assert_eq!(worktree_safety(&work), WorktreeSafety::NotSafe("clone has a stash"));
    }

    #[test]
    fn test_parent_repo_of_dir_and_file_forms() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        git(&main, &["init", "-q", "--initial-branch=main"]);
        git(&main, &["config", "user.email", "loom@example.com"]);
        git(&main, &["config", "user.name", "Loom Test"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        let wt = tmp.path().join("wt");
        git(
            &main,
            &[
                "worktree",
                "add",
                "-q",
                wt.to_str().unwrap(),
                "-b",
                "branch",
            ],
        );
        assert_eq!(parent_repo_of(&main).as_deref(), Some(main.as_path()));
        assert_eq!(parent_repo_of(&wt).as_deref(), Some(main.as_path()));
    }

    // ===================================================================
    // discover_candidates / evaluate_root gates
    // ===================================================================

    #[test]
    fn test_discover_candidates_direct_children_only() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("loom-7-builder.abcd")).unwrap();
        std::fs::create_dir_all(tmp.path().join("plain")).unwrap();
        std::fs::create_dir_all(tmp.path().join("plain/loom-8-judge.ef01")).unwrap();
        let found = discover_candidates(&[tmp.path().to_path_buf()]);
        assert_eq!(found, vec![tmp.path().join("loom-7-builder.abcd")]);
    }

    #[test]
    fn test_discover_candidates_skips_plain_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("loom-9987-src.patch"), b"x").unwrap();
        std::fs::create_dir_all(tmp.path().join("loom-7-builder.abcd")).unwrap();
        let found = discover_candidates(&[tmp.path().to_path_buf()]);
        assert_eq!(found, vec![tmp.path().join("loom-7-builder.abcd")]);
    }

    #[test]
    fn test_evaluate_root_plain_file_reports_not_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("loom-9987-src.patch");
        std::fs::write(&file, b"x").unwrap();
        let (probes, _) = probes_now(24 * 3600, 24 * 3600);
        assert_eq!(evaluate_root(&file, &probes), RootVerdict::Preserve("not a directory"));
    }

    #[test]
    fn test_evaluate_root_young_root_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-9-builder.aa11");
        std::fs::create_dir_all(&root).unwrap();
        let (probes, _) = probes_now(24 * 3600, 24 * 3600);
        // Fresh mtime: fails both the age floor and the idle window; the age
        // floor is checked first.
        assert_eq!(
            evaluate_root(&root, &probes),
            RootVerdict::Preserve("root younger than scratchReclaim.maxAgeHours")
        );
    }

    #[test]
    fn test_evaluate_root_no_worktree_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-9-builder.aa11");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("data.bin"), b"x").unwrap();
        let (probes, now) = probes_now(24 * 3600, 24 * 3600);
        backdate(&root, now.timestamp() - 48 * 3600);
        assert_eq!(
            evaluate_root(&root, &probes),
            RootVerdict::Preserve("no git worktree inside the root")
        );
    }

    #[test]
    fn test_evaluate_root_symlink_and_mount_point_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::create_dir_all(&target).unwrap();
        let link = tmp.path().join("loom-10-judge.bb22");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let (probes, _) = probes_now(24 * 3600, 24 * 3600);
        assert_eq!(
            evaluate_root(&link, &probes),
            RootVerdict::Preserve("root is a symlink (never followed)")
        );

        let mount = tmp.path().join("loom-11-judge.cc33");
        std::fs::create_dir_all(&mount).unwrap();
        // The candidate reports a different device than its parent.
        let mount_device = |p: &Path| if p == mount { Some(2) } else { Some(1) };
        let probing = ScratchRootProbes {
            now: probes.now,
            idle_window_secs: 24 * 3600,
            max_age_secs: 24 * 3600,
            ticket_finished: probes.ticket_finished,
            pid_alive: probes.pid_alive,
            processes_using: probes.processes_using,
            device_of: &mount_device,
        };
        assert_eq!(
            evaluate_root(&mount, &probing),
            RootVerdict::Preserve("root is a mount point (device differs from parent)")
        );
    }

    #[test]
    fn test_evaluate_root_watch_pid_live_and_dead() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-12-builder.dd44");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("watch-pid"), std::process::id().to_string()).unwrap();
        let (probes, now) = probes_now(24 * 3600, 24 * 3600);
        backdate(&root, now.timestamp() - 48 * 3600);
        // watch-pid names THIS test process: alive.
        let probing = ScratchRootProbes {
            now,
            idle_window_secs: 24 * 3600,
            max_age_secs: 24 * 3600,
            ticket_finished: probes.ticket_finished,
            pid_alive: &crate::daemon_install_state::pid_alive,
            processes_using: probes.processes_using,
            device_of: probes.device_of,
        };
        assert_eq!(
            evaluate_root(&root, &probing),
            RootVerdict::Preserve("no git worktree inside the root")
        );

        // Dead pid passes the gate (falls through to the next).
        std::fs::write(root.join("watch-pid"), "999999997").unwrap();
        backdate(&root, now.timestamp() - 48 * 3600);
        assert_eq!(
            evaluate_root(&root, &probing),
            RootVerdict::Preserve("no git worktree inside the root")
        );

        // Unparseable watch-pid counts as alive -> but the worktree gate
        // fires first here; assert on a root with a worktree below instead.
        let with_wt = root.join("source");
        std::fs::create_dir_all(&with_wt).unwrap();
        git(&with_wt, &["init", "-q", "--initial-branch=main"]);
        git(&with_wt, &["config", "user.email", "loom@example.com"]);
        git(&with_wt, &["config", "user.name", "Loom Test"]);
        git(&with_wt, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(
            &with_wt,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        git(&with_wt, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(root.join("watch-pid"), "not-a-pid").unwrap();
        backdate(&root, now.timestamp() - 48 * 3600);
        assert_eq!(
            evaluate_root(&root, &probing),
            RootVerdict::Preserve("watch-pid file names a live process")
        );
    }

    #[test]
    fn test_evaluate_root_open_and_unknown_ticket_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-13-judge.ee55");
        std::fs::create_dir_all(&root).unwrap();
        let (probes, now) = probes_now(24 * 3600, 24 * 3600);
        backdate(&root, now.timestamp() - 48 * 3600);
        let open_answer = |_: &Path, _: &str, _: u32| Some(false);
        let unknown_answer = |_: &Path, _: &str, _: u32| None;
        let open_probes = ScratchRootProbes {
            now,
            idle_window_secs: 24 * 3600,
            max_age_secs: 24 * 3600,
            ticket_finished: &open_answer,
            pid_alive: probes.pid_alive,
            processes_using: probes.processes_using,
            device_of: probes.device_of,
        };
        let unknown_probes = ScratchRootProbes {
            ticket_finished: &unknown_answer,
            ..open_probes
        };
        assert_eq!(
            evaluate_root(&root, &open_probes),
            RootVerdict::Preserve("no git worktree inside the root")
        );
        // With a resolvable worktree, the ticket gate decides.
        let with_wt = root.join("source");
        std::fs::create_dir_all(&with_wt).unwrap();
        git(&with_wt, &["init", "-q", "--initial-branch=main"]);
        git(&with_wt, &["config", "user.email", "loom@example.com"]);
        git(&with_wt, &["config", "user.name", "Loom Test"]);
        git(&with_wt, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(
            &with_wt,
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        git(&with_wt, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        backdate(&root, now.timestamp() - 48 * 3600);
        assert_eq!(
            evaluate_root(&root, &open_probes),
            RootVerdict::Preserve("linked ticket is open")
        );
        assert_eq!(
            evaluate_root(&root, &unknown_probes),
            RootVerdict::Preserve("linked ticket state unknown (probe failed)")
        );
    }

    #[test]
    fn test_evaluate_root_processes_preserved() {
        fn holder_processes(_: &Path) -> Vec<u32> {
            vec![4242]
        }
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-14-builder.ff66");
        std::fs::create_dir_all(&root).unwrap();
        let (probes, now) = probes_now(24 * 3600, 24 * 3600);
        backdate(&root, now.timestamp() - 48 * 3600);
        let probing = ScratchRootProbes {
            now,
            idle_window_secs: 24 * 3600,
            max_age_secs: 24 * 3600,
            ticket_finished: probes.ticket_finished,
            pid_alive: probes.pid_alive,
            processes_using: &holder_processes,
            device_of: probes.device_of,
        };
        assert_eq!(
            evaluate_root(&root, &probing),
            RootVerdict::Preserve("no git worktree inside the root")
        );
    }

    // ===================================================================
    // remove_root: read-only trees
    // ===================================================================

    #[cfg(unix)]
    #[test]
    fn test_remove_root_chmods_read_only_trees_first() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-15-hermit.007a");
        let ro = root.join("pkg/mod/cache/download");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("file"), b"go module cache entry").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(ro.join("file"), std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        remove_root(&root).expect("chmod -R u+w makes the read-only tree removable");
        assert!(!root.exists());
    }

    // ===================================================================
    // The fixture: one eligible root beside siblings each failing one gate
    // ===================================================================

    /// A published, clean worktree (its branch pushed to the repo's LOCAL
    /// bare origin) at `<dest>`, as a worktree of `main` — so `main` keeps a
    /// registration that `git worktree prune` must clear when the root is
    /// removed. Callers swap the `origin` URL for a GitHub-shaped one AFTER
    /// this, once the remote-tracking refs already exist locally.
    fn build_published_worktree_under(main: &Path, dest: &Path, branch: &str) {
        git(
            main,
            &[
                "worktree",
                "add",
                "-q",
                dest.to_str().unwrap(),
                "-b",
                branch,
            ],
        );
        git(
            dest,
            &[
                "push",
                "-q",
                "origin",
                format!("HEAD:refs/heads/{branch}").as_str(),
            ],
        );
        git(dest, &["fetch", "-q", "origin"]);
        git(
            dest,
            &[
                "branch",
                "--set-upstream-to",
                &format!("origin/{branch}"),
                branch,
            ],
        );
    }

    fn init_main_with_origin(main: &Path, origin: &Path) {
        std::fs::create_dir_all(main).unwrap();
        git(main, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(main, &["init", "-q", "--initial-branch=main"]);
        git(main, &["config", "user.email", "loom@example.com"]);
        git(main, &["config", "user.name", "Loom Test"]);
        git(main, &["commit", "-q", "--allow-empty", "-m", "seed"]);
        git(main, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(main, &["push", "-q", "origin", "main"]);
        git(main, &["fetch", "-q", "origin"]);
        git(main, &["branch", "--set-upstream-to=origin/main", "main"]);
    }

    #[test]
    fn fixture_only_the_eligible_root_disappears() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let now = Utc::now();
        let old = now.timestamp() - 48 * 3600;

        // -- eligible: ticket 100, closed, everything clean and idle, the
        //    worktree linked to a main repo OUTSIDE the root so prune is
        //    observable.
        let eligible = base.join("loom-100-builder.aaaa");
        std::fs::create_dir_all(&eligible).unwrap();
        let outside_main = base.join("outside-main");
        let outside_origin = base.join("outside-origin.git");
        init_main_with_origin(&outside_main, &outside_origin);
        build_published_worktree_under(
            &outside_main,
            &eligible.join("worktrees/loom/issue-100/wt"),
            "feature/issue-100",
        );
        // Only now does `origin` look like GitHub — the remote-tracking refs
        // already exist, so reachability still holds.
        git(
            &outside_main,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        backdate(&eligible, old);

        // Helpers for siblings that need a resolved, published worktree but
        // must fail a *later* gate.
        let make_sibling = |name: &str| -> PathBuf {
            let root = base.join(name);
            std::fs::create_dir_all(&root).unwrap();
            let main = root.join("source");
            let origin = root.join("origin.git");
            init_main_with_origin(&main, &origin);
            git(
                &main,
                &[
                    "remote",
                    "set-url",
                    "origin",
                    "https://github.com/acme/widgets.git",
                ],
            );
            main
        };

        // 1. open ticket (101)
        let open_ticket = make_sibling("loom-101-builder.bbbb");
        backdate(open_ticket.parent().unwrap(), old);

        // 2. live watch-pid (102)
        let live_pid = make_sibling("loom-102-builder.cccc");
        let live_root = live_pid.parent().unwrap();
        std::fs::write(live_root.join("watch-pid"), std::process::id().to_string()).unwrap();
        backdate(live_root, old);

        // 3. open file under the root (103)
        let _open_file = make_sibling("loom-103-builder.dddd");
        backdate(&base.join("loom-103-builder.dddd"), old);

        // 4. recent mtime (104): everything backdated, then ONE file's mtime
        //    re-freshened — the root's own mtime stays old, so only the idle
        //    gate fires.
        let recent = make_sibling("loom-104-builder.eeee");
        let recent_root = recent.parent().unwrap();
        std::fs::write(recent_root.join("touched-now"), b"fresh").unwrap();
        backdate(recent_root, old);
        std::fs::File::options()
            .write(true)
            .open(recent_root.join("touched-now"))
            .unwrap()
            .set_modified(SystemTime::now())
            .unwrap();

        // 5. tracked change (105): a committed file with a remote-tracking
        //    ref at HEAD (published), then modified — and an untracked file,
        //    which must NOT trip the tracked-change gate.
        let tracked = make_sibling("loom-105-builder.ffff");
        std::fs::write(tracked.join("f"), b"v1").unwrap();
        git(&tracked, &["add", "f"]);
        git(&tracked, &["commit", "-q", "-m", "add f"]);
        git(&tracked, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        std::fs::write(tracked.join("f"), b"v2 uncommitted").unwrap();
        std::fs::write(tracked.join("untracked.txt"), b"untracked").unwrap();
        backdate(tracked.parent().unwrap(), old);

        // 6. unpushed commit (106)
        let unpushed = make_sibling("loom-106-judge.1111");
        git(&unpushed, &["commit", "-q", "--allow-empty", "-m", "unpushed"]);
        backdate(unpushed.parent().unwrap(), old);

        // 7. failing git status (107): worktree whose parent repo is gone
        let broken = base.join("loom-107-judge.2222");
        let broken_wt = broken.join("wt");
        std::fs::create_dir_all(&broken_wt).unwrap();
        std::fs::write(broken_wt.join(".git"), "gitdir: /nonexistent/parent/.git/worktrees/wt")
            .unwrap();
        backdate(&broken, old);

        // 8. unresolvable repo (108): published worktree, non-GitHub origin
        let no_repo = make_sibling("loom-108-judge.3333");
        git(
            &no_repo,
            &[
                "remote",
                "set-url",
                "origin",
                "https://gitlab.example.com/acme/widgets.git",
            ],
        );
        backdate(no_repo.parent().unwrap(), old);

        // 9. symlinked root (109)
        let link_target = base.join("link-target");
        std::fs::create_dir_all(&link_target).unwrap();
        std::os::unix::fs::symlink(&link_target, base.join("loom-109-judge.4444")).unwrap();

        // 10. mount-point root (110): injected device view
        let mount_root = base.join("loom-110-judge.5555");
        std::fs::create_dir_all(&mount_root).unwrap();
        backdate(&mount_root, old);

        // Every ticket is closed except 101 (open).
        let ticket_probe = |_: &Path, _: &str, ticket: u32| {
            if ticket == 101 {
                Some(false)
            } else {
                Some(true)
            }
        };
        // Root 103 "has" a live holder.
        let open_file_root = base.join("loom-103-builder.dddd");
        let process_probe = |p: &Path| {
            if p == open_file_root {
                vec![4242]
            } else {
                Vec::new()
            }
        };
        // Root 110 sits on another device.
        let device_probe = |p: &Path| if p == mount_root { Some(2) } else { Some(1) };
        let probes = ScratchRootProbes {
            now,
            idle_window_secs: 24 * 3600,
            max_age_secs: 24 * 3600,
            ticket_finished: &ticket_probe,
            pid_alive: &crate::daemon_install_state::pid_alive,
            processes_using: &process_probe,
            device_of: &device_probe,
        };

        let recorder = Mutex::new(Vec::new());
        let report =
            sweep(base, &[base.to_path_buf()], &probes, /* dry_run = */ false, &recorder);

        assert!(!eligible.exists(), "eligible root is removed");
        assert_eq!(report.removed.len(), 1, "exactly one removal: {report:?}");
        assert_eq!(report.removed[0].ticket, 100);
        assert_eq!(report.removed[0].repo, "acme/widgets");
        assert!(report.removed[0].bytes > 0);

        for kept in [
            "loom-101-builder.bbbb",
            "loom-102-builder.cccc",
            "loom-103-builder.dddd",
            "loom-104-builder.eeee",
            "loom-105-builder.ffff",
            "loom-106-judge.1111",
            "loom-107-judge.2222",
            "loom-108-judge.3333",
            "loom-109-judge.4444",
            "loom-110-judge.5555",
        ] {
            assert!(base.join(kept).exists(), "{kept} must survive the pass");
        }
        let reason_for = |name: &str| -> &'static str {
            report
                .preserved
                .iter()
                .find(|p| p.path.ends_with(name))
                .unwrap_or_else(|| panic!("{name} missing from preserved list"))
                .reason
        };
        assert_eq!(reason_for("loom-101-builder.bbbb"), "linked ticket is open");
        assert_eq!(reason_for("loom-102-builder.cccc"), "watch-pid file names a live process");
        assert_eq!(
            reason_for("loom-103-builder.dddd"),
            "a process has its cwd or an open file under the root"
        );
        assert_eq!(reason_for("loom-104-builder.eeee"), "files changed within the 24h idle window");
        assert_eq!(reason_for("loom-105-builder.ffff"), "worktree has tracked changes");
        assert_eq!(reason_for("loom-106-judge.1111"), "worktree HEAD has commits no remote holds");
        assert_eq!(reason_for("loom-107-judge.2222"), "git status failed");
        assert_eq!(reason_for("loom-108-judge.3333"), "no resolvable GitHub origin repository");
        assert_eq!(reason_for("loom-109-judge.4444"), "root is a symlink (never followed)");
        assert_eq!(
            reason_for("loom-110-judge.5555"),
            "root is a mount point (device differs from parent)"
        );

        // Removal records went to the recorder with the #14889 shape.
        let records = recorder.into_inner().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].ticket, 100);
        assert_eq!(records[0].repo, "acme/widgets");
        assert!(!records[0].dry_run);
        let payload = records[0].payload();
        assert_eq!(payload["source"], "scratch-root-reaper");
        assert_eq!(payload["ticket"], 100);

        // git worktree prune ran on the surviving parent repo.
        let list = Command::new("git")
            .arg("-C")
            .arg(&outside_main)
            .args(["worktree", "list", "--porcelain"])
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&list.stdout).to_string();
        assert!(
            !listing.contains("loom-100-builder.aaaa"),
            "stale worktree entry must be pruned from the parent repo:\n{listing}"
        );
    }

    #[test]
    fn fixture_dry_run_removes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let now = Utc::now();
        let root = base.join("loom-200-builder.cafe");
        std::fs::create_dir_all(&root).unwrap();
        let main = root.join("source");
        let origin = root.join("origin.git");
        init_main_with_origin(&main, &origin);
        git(
            &main,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        backdate(&root, now.timestamp() - 48 * 3600);

        let probes = ScratchRootProbes {
            now,
            idle_window_secs: 24 * 3600,
            max_age_secs: 24 * 3600,
            ticket_finished: &always_finished,
            pid_alive: &never_alive,
            processes_using: &no_processes,
            device_of: &same_device,
        };
        let recorder = Mutex::new(Vec::new());
        let report = sweep(base, &[base.to_path_buf()], &probes, true, &recorder);
        assert!(root.exists(), "dry-run removes nothing");
        assert!(report.removed.is_empty());
        assert_eq!(report.planned.len(), 1);
        assert_eq!(report.planned[0].ticket, 200);
        let records = recorder.into_inner().unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].dry_run);
        assert_eq!(records[0].payload()["dry_run"], true);
    }

    // ===================================================================
    // run_for — cooldown and config
    // ===================================================================

    #[test]
    #[serial]
    fn test_run_for_respects_cooldown_and_disabled() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        // The roots seam keeps the test inside its own tempdir — a real
        // `candidate_roots` list would scan (and could remove from) the
        // actual $HOME / $TMPDIR of the host running the suite.
        let roots = [tmp.path().to_path_buf()];

        std::env::set_var(crate::scratch_reclaim::SCRATCH_RECLAIM_ENABLE_ENV, "0");
        let report = run_for_roots(tmp.path(), &roots);
        std::env::remove_var(crate::scratch_reclaim::SCRATCH_RECLAIM_ENABLE_ENV);
        assert!(!report.enabled);

        // Enabled: first pass runs (nothing to find), second is deferred.
        let first = run_for_roots(tmp.path(), &roots);
        assert!(first.enabled);
        assert!(first.deferred.is_none());
        let second = run_for_roots(tmp.path(), &roots);
        assert!(second.deferred.is_some(), "cooldown defers the second pass");
    }

    #[test]
    #[serial]
    fn test_run_for_dry_run_env() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(crate::scratch_reclaim::SCRATCH_RECLAIM_DRY_RUN_ENV, "1");
        let report = run_for_roots(tmp.path(), &[tmp.path().to_path_buf()]);
        std::env::remove_var(crate::scratch_reclaim::SCRATCH_RECLAIM_DRY_RUN_ENV);
        assert!(report.dry_run);
    }
}
