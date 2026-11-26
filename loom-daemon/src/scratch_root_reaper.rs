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
//! follows symlinks. It preserves symlink roots and any candidate whose tree
//! cannot be verified to share its parent’s mount.
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
//! 3. a complete process scan finds no cwd, executable, open fd or mapped file under the root;
//! 4. the bounded idle walk finds no change within the idle window (24 h).
//!    An unreadable subtree is `Unknown` and preserves the root.
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
//! Removal opens directories without following links, verifies their mount
//! and grants owner write access to read-only directories before unlinking entries.
//! Deleting a worktree directory leaves a stale entry in its
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

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use regex::Regex;

use crate::fs_util::human_size;

mod mount_boundary;
mod process_activity;

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

/// Maximum directories one idle walk examines. Once the budget is spent,
/// the walk skips remaining subtrees and can miss recent writes there.
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
    let home = std::env::var("HOME").unwrap_or_default();
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".to_string());
    candidate_roots_with_dirs(repo_root, Path::new(&home), Path::new(&tmp))
}

fn candidate_roots_with_dirs(repo_root: &Path, home: &Path, tmp: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    if let Some(top) = volume_root_of(&crate::worktree_root::worktree_root(repo_root)) {
        roots.push(top);
    }
    if !home.as_os_str().is_empty() {
        roots.push(home.to_path_buf());
        roots.push(home.join(".cache"));
    }
    if !tmp.as_os_str().is_empty() {
        roots.push(tmp.to_path_buf());
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

/// Whether anything under `root` changed within `window_secs`, from a bounded
/// recursive walk. Symlinks are never followed (a link's own mtime is still
/// examined); a future-dated mtime reads as age zero (maximally recent), the
/// same clock-skew convention as `worktree_activity`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleProbe {
    /// Something under the root was written within the window (payload:
    /// how long ago, in seconds; 0 for a future mtime).
    Recent(u64),
    /// The walk found no recent change in the directories it examined,
    /// including when it reached [`IDLE_WALK_ENTRY_CAP`].
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
    let recent =
        |mtime| IdleProbe::Recent(u64::try_from(now_epoch.saturating_sub(mtime)).unwrap_or(0));
    if mtime >= cutoff_epoch || mtime > now_epoch {
        return recent(mtime);
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
                return recent(m);
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
                return recent(m);
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
/// A common store inside the removal boundary also needs every registered
/// checkout accounted for, every ref published, and an empty stash.
///
/// `--no-optional-locks` is load-bearing, not hygiene: a plain `git status`
/// opportunistically refreshes and **writes** the index, which would stamp
/// a fresh mtime under the root on every pass — the 24h idle gate would then
/// never let a root the pass itself looked at become eligible.
#[must_use]
pub fn worktree_safety(worktree: &Path) -> WorktreeSafety {
    let Ok(root) = worktree.canonicalize() else {
        return WorktreeSafety::NotSafe("worktree path could not be resolved");
    };
    worktree_safety_in_root(&root, &root, std::slice::from_ref(&root))
}

fn worktree_safety_in_root(worktree: &Path, root: &Path, discovered: &[PathBuf]) -> WorktreeSafety {
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
    // A gitfile may also point at a separate Git directory inside the root.
    // Use the directory that will actually be lost to select the ref scope.
    let common_dir =
        git_answer(worktree, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .and_then(|s| Path::new(s.strip_suffix('\n')?).canonicalize().ok());
    let Some(common_dir) = common_dir else {
        return WorktreeSafety::NotSafe("cannot locate the common Git directory");
    };
    if common_dir.starts_with(root) {
        if !registered_worktrees_checked(worktree, discovered) {
            return WorktreeSafety::NotSafe("clone backs a checkout outside the inspected worktrees or its registrations are unreadable");
        }
        if clone_has_stash(worktree) {
            return WorktreeSafety::NotSafe("clone has a stash");
        }
        let unpublished = git_answer(
            worktree,
            &[
                "--no-replace-objects",
                "rev-list",
                "--count",
                "--all",
                "--not",
                "--remotes",
            ],
        );
        if unpublished.as_deref() != Some("0\n") {
            return WorktreeSafety::NotSafe("clone has local refs not proven published");
        }
    }
    WorktreeSafety::Safe
}

fn git_answer(worktree: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .current_dir(worktree)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() || !out.stderr.is_empty() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// A surviving checkout needs this store even if every commit is published.
/// Each registered path must be one of the worktrees evaluated by this pass.
fn registered_worktrees_checked(worktree: &Path, discovered: &[PathBuf]) -> bool {
    let Some(list) = git_answer(worktree, &["worktree", "list", "--porcelain", "-z"]) else {
        return false;
    };
    let Some(records) = list.strip_suffix("\0\0") else {
        return false;
    };
    records.split("\0\0").all(|record| {
        let Some((path, _)) = record
            .strip_prefix("worktree ")
            .and_then(|s| s.split_once('\0'))
        else {
            return false;
        };
        Path::new(path)
            .canonicalize()
            .is_ok_and(|p| discovered.contains(&p))
    })
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
    let parsed;
    let (host, path) = if let Some(rest) = url.strip_prefix("git@") {
        rest.split_once(':')?
    } else if url.starts_with("https://") || url.starts_with("ssh://") {
        parsed = reqwest::Url::parse(url).ok()?;
        // HTTPS parsing already folds its default port; SSH retains explicit :22.
        if !matches!((parsed.scheme(), parsed.port()), ("https", None) | ("ssh", None | Some(22)))
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return None;
        }
        (parsed.host_str()?, parsed.path())
    } else {
        return None;
    };
    let path = path.trim_matches('/');
    let slug = path.strip_suffix(".git").unwrap_or(path);
    (host.eq_ignore_ascii_case(gh_host) && crate::forge_slug::valid_slug(slug))
        .then(|| slug.to_owned())
}

/// Read the origin of this checkout only. Git would otherwise walk up to
/// an enclosing repository when a registered checkout has disappeared.
fn github_origin(worktree: &Path, host: &str) -> Option<String> {
    if !worktree.join(".git").exists() {
        return None;
    }
    github_slug_from_url(&origin_url(worktree)?, host)
}

/// The GitHub host scratch-root repo resolution accepts.
#[must_use]
fn gh_host() -> String {
    std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".to_string())
}

/// A scratch root's origin can identify a workspace only if exactly one
/// registered checkout has that origin. Include disabled checkouts in this
/// test: their configuration is just as authoritative as an enabled one's.
fn unique_workspace_slug(workspace: &Path) -> Option<String> {
    let host = gh_host();
    let workspace = workspace.canonicalize().ok()?;
    let slug = github_origin(&workspace, &host)?;
    let registry = crate::workspace_registry::WorkspaceRegistry::load_default().ok()?;
    let roots = registry.effective_roots(&workspace);
    if !roots.contains(&workspace) {
        return None;
    }
    for peer in roots.iter().filter(|path| **path != workspace) {
        // An unknown peer could be another checkout with its own cleanup policy.
        if github_origin(peer, &host)?.eq_ignore_ascii_case(&slug) {
            return None;
        }
    }
    Some(slug)
}

/// Gate 2's watch-pid reading. Fails closed: an existing `watch-pid` file
/// that cannot be read or parsed preserves the root. Only a dead pid or
/// no file at all passes the gate.
enum WatchPidState {
    Absent,
    Dead,
    Alive,
    Unreadable,
    Unknown,
}

fn watch_pid_state(root: &Path, pid_alive: &dyn Fn(u32) -> Option<bool>) -> WatchPidState {
    let marker = root.join("watch-pid");
    match std::fs::symlink_metadata(&marker) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return WatchPidState::Absent,
        Ok(meta) if meta.is_file() => {}
        _ => return WatchPidState::Unreadable,
    }
    let pid = std::fs::read_to_string(&marker)
        .ok()
        .and_then(|text| text.trim().parse().ok());
    match pid {
        Some(pid) => match pid_alive(pid) {
            Some(true) => WatchPidState::Alive,
            Some(false) => WatchPidState::Dead,
            None => WatchPidState::Unknown,
        },
        None => WatchPidState::Unreadable,
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
    pub pid_alive: &'a dyn Fn(u32) -> Option<bool>,
    /// Whether any process holds the root. An incomplete scan returns `None`.
    pub directory_in_use: &'a dyn Fn(&Path) -> Option<bool>,
    /// Mount identity of the entry itself, including symlinks. Unknown preserves.
    pub mount_id_of: &'a dyn Fn(&Path) -> Option<u64>,
}

/// Apply every removal gate, cheapest first, to one candidate root. Pure
/// decision logic: removes nothing.
#[must_use]
pub fn evaluate_root(root: &Path, owner: &str, probes: &ScratchRootProbes<'_>) -> RootVerdict {
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
        Err(_) => return RootVerdict::Preserve("root metadata unreadable"),
        Ok(_) => {}
    }
    let mount = match ((probes.mount_id_of)(root), root.parent().and_then(probes.mount_id_of)) {
        (Some(root_id), Some(parent_id)) if root_id == parent_id => root_id,
        (Some(_), Some(_)) => return RootVerdict::Preserve("root is a mount point"),
        _ => return RootVerdict::Preserve(mount_boundary::UNKNOWN),
    };
    let Some(root_mtime) = mtime_epoch(root) else {
        return RootVerdict::Preserve("root mtime unreadable");
    };
    let root_age = probes.now.timestamp() - root_mtime;
    if !crate::scratch_reclaim::is_reclaimable_age(root_age, probes.max_age_secs) {
        return RootVerdict::Preserve("root younger than scratchReclaim.maxAgeHours");
    }
    match watch_pid_state(root, probes.pid_alive) {
        WatchPidState::Alive => {
            return RootVerdict::Preserve("watch-pid file names a live process");
        }
        WatchPidState::Unreadable => {
            return RootVerdict::Preserve("watch-pid file could not be read or parsed");
        }
        WatchPidState::Unknown => return RootVerdict::Preserve("watch-pid liveness unknown"),
        WatchPidState::Dead | WatchPidState::Absent => {}
    }
    let Ok(canonical_root) = root.canonicalize() else {
        return RootVerdict::Preserve("root path could not be resolved");
    };
    let bytes = match mount_boundary::tree_size(&canonical_root, mount, probes.mount_id_of) {
        Ok(bytes) => bytes,
        Err(reason) => return RootVerdict::Preserve(reason),
    };
    let Some(worktrees) = find_git_worktrees(&canonical_root) else {
        return RootVerdict::Preserve("worktree search could not answer (unreadable subtree)");
    };
    let Some(probe_cwd) = worktrees.first() else {
        return RootVerdict::Preserve("no git worktree inside the root");
    };
    let host = gh_host();
    for worktree in &worktrees {
        let Some(slug) = github_origin(worktree, &host) else {
            return RootVerdict::Preserve("no resolvable GitHub origin repository");
        };
        if !slug.eq_ignore_ascii_case(owner) {
            return RootVerdict::Preserve("worktree belongs to another repository");
        }
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
    let mut prune = BTreeSet::new();
    for worktree in &worktrees {
        match worktree_safety_in_root(worktree, &canonical_root, &worktrees) {
            WorktreeSafety::Safe => {
                if let Some(parent) = parent_repo_of(worktree) {
                    prune.insert(parent);
                }
            }
            WorktreeSafety::NotSafe(reason) => return RootVerdict::Preserve(reason),
        }
    }
    match (probes.ticket_finished)(probe_cwd, owner, name.ticket) {
        Some(true) => {}
        Some(false) => return RootVerdict::Preserve("linked ticket is open"),
        None => return RootVerdict::Preserve("linked ticket state unknown (probe failed)"),
    }
    match (probes.directory_in_use)(&canonical_root) {
        Some(true) => {
            return RootVerdict::Preserve(
                "a process has its cwd, executable, open file or mapped file under the root",
            )
        }
        None => {
            return RootVerdict::Preserve("process activity under the root could not be verified")
        }
        Some(false) => {}
    }
    RootVerdict::Remove {
        ticket: name.ticket,
        repo: owner.to_owned(),
        bytes,
        prune,
    }
}

// ============================================================================
// Removal mechanics
// ============================================================================

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
/// Only the workspace uniquely identified by the candidate origins may act.
fn sweep(
    repo_root: &Path,
    owner: Option<&str>,
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
        let verdict = owner.map_or(
            RootVerdict::Preserve("workspace has no unique registered GitHub origin"),
            |slug| evaluate_root(&candidate, slug, probes),
        );
        match verdict {
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
                } else if let Err(e) = mount_boundary::remove_root(&candidate) {
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

/// Log completed passes with candidates at `info`, including the reason for
/// each preserved root. Empty, disabled and deferred passes log at `debug`.
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
    if report.candidates == 0 {
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
        pid_alive: &process_activity::pid_alive,
        directory_in_use: &process_activity::directory_in_use,
        mount_id_of: &mount_boundary::mount_id,
    }
}

/// Run one production scratch-root pass for `repo_root`, honoring the shared
/// `scratchReclaim` config surface and its own cooldown, and log the result.
#[must_use]
pub fn run_for(repo_root: &Path) -> ScratchRootReport {
    run_for_roots(
        repo_root,
        &candidate_roots(repo_root),
        &prod_ticket_finished,
        &process_activity::directory_in_use,
        &ProductionRecorder,
    )
}

/// [`run_for`] over an explicit root list — the seam tests use so they never
/// scan (or remove from) the real `$HOME` / `$TMPDIR` / worktree volume.
fn run_for_roots(
    repo_root: &Path,
    roots: &[PathBuf],
    ticket_finished: &dyn Fn(&Path, &str, u32) -> Option<bool>,
    directory_in_use: &dyn Fn(&Path) -> Option<bool>,
    recorder: &dyn Recorder,
) -> ScratchRootReport {
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
    let probes = ScratchRootProbes {
        ticket_finished,
        directory_in_use,
        ..production_probes(now, IDLE_WINDOW_SECS, max_age_secs)
    };
    let owner = unique_workspace_slug(repo_root);
    let report = sweep(repo_root, owner.as_deref(), roots, &probes, dry_run, recorder);
    record_run(repo_root, now);
    log_report(&report);
    report
}

/// Scheduled and eager ticks share one queued or running job per workspace.
static SCANS: Mutex<BTreeMap<PathBuf, tokio::task::JoinHandle<()>>> = Mutex::new(BTreeMap::new());
/// Shared directories are walked by at most one worker at a time.
static WALK_SLOT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Queue a scan and return immediately. The worker reports actual removals
/// and previews separately when it completes.
pub fn schedule(repo_root: &Path) -> bool {
    schedule_with(repo_root, |root| {
        let _ = run_for(root);
    })
}

pub(crate) fn schedule_with(repo_root: &Path, scan: impl FnOnce(&Path) + Send + 'static) -> bool {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        log::warn!("scratch_root_reaper: cannot schedule a scan outside the daemon runtime");
        return false;
    };
    let root = crate::workspace_registry::normalize_path(repo_root);
    let mut scans = SCANS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    scans.retain(|_, task| !task.is_finished());
    scans.entry(root.clone()).or_insert_with(|| {
        runtime.spawn(async move {
            let Ok(permit) = WALK_SLOT.acquire().await else {
                return;
            };
            if let Err(err) = tokio::task::spawn_blocking(move || {
                let _permit = permit;
                scan(&root);
            })
            .await
            {
                log::error!("scratch_root_reaper: scan worker failed: {err}");
            }
        })
    });
    true
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
    fn never_alive(_: u32) -> Option<bool> {
        Some(false)
    }
    fn no_processes(_: &Path) -> Option<bool> {
        Some(false)
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
                directory_in_use: &no_processes,
                mount_id_of: &same_device,
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
        for url in [
            "https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets",
            "git@github.com:acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
            "ssh://git@github.com:22/acme/widgets.git",
            "https://alice@github.com/acme/widgets.git",
            "https://GITHUB.COM:443/acme/widgets.git/",
        ] {
            assert_eq!(
                github_slug_from_url(url, "github.com").as_deref(),
                Some("acme/widgets"),
                "{url}"
            );
        }
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
        for url in [
            "ssh://git@github.com:2222/acme/widgets.git",
            "https://github.com:8443/acme/widgets.git",
            "https://github.com@other.example/acme/widgets.git",
            "https://github.com/acme/widgets.git?mirror=1",
            "https://github.com/acme/widgets/extra",
            "https:github.com/acme/widgets",
        ] {
            assert!(github_slug_from_url(url, "github.com").is_none(), "{url}");
        }
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

        backdate(&root.join("old.txt"), now.timestamp() - 90);
        assert_eq!(probe_idle(root, 24 * 3600, now), IdleProbe::Recent(90));

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
        assert_eq!(probe_idle(tmp.path(), 24 * 3600, past), IdleProbe::Recent(0));
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
            WorktreeSafety::NotSafe("clone has local refs not proven published")
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
    fn preview_preserves_clone_backing_another_checkout_or_holding_a_local_tag() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = [tmp.path().to_owned()];
        let root = tmp.path().join("loom-201-builder.cafe");
        let source = root.join("source");
        init_main_with_origin(&source, &tmp.path().join("origin.git"));
        git(
            &source,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        let (probes, now) = probes_now(IDLE_WINDOW_SECS, IDLE_WINDOW_SECS);
        let preview = || {
            backdate(&root, now.timestamp() - 2 * IDLE_WINDOW_SECS);
            let records = Mutex::new(Vec::new());
            let report = sweep(tmp.path(), Some("acme/widgets"), &roots, &probes, true, &records);
            assert_eq!(report.planned, records.into_inner().unwrap());
            report
        };
        let sibling = tmp.path().join("loom-201-builder.cafe-sibling");
        git(&source, &["worktree", "add", "--detach", sibling.to_str().unwrap()]);
        assert!(preview().planned.is_empty(), "the sibling still needs the clone");
        git(
            &source,
            &[
                "worktree",
                "move",
                sibling.to_str().unwrap(),
                root.join("linked").to_str().unwrap(),
            ],
        );
        assert_eq!(preview().planned.len(), 1, "both clean checkouts are inside and checked");

        git(&source, &["checkout", "--detach"]);
        git(&source, &["commit", "--allow-empty", "-m", "tagged unpublished work"]);
        git(&source, &["tag", "experiment"]);
        git(&source, &["checkout", "main"]);
        assert!(preview().planned.is_empty(), "only the local tag retains the commit");
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

    #[cfg(unix)]
    #[test]
    #[serial]
    fn candidate_roots_includes_configured_worktree_volume() {
        use crate::env_test_support::EnvGuard;

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let worktrees = tmp.path().join("worktrees");
        let configured = worktrees.join("repo");
        std::fs::create_dir_all(repo.join(".loom")).unwrap();
        std::fs::create_dir_all(&configured).unwrap();
        // Only the override exists. Keep HOME and TMPDIR below the mount point
        // so neither can put the expected volume in the list by coincidence.
        let _env = EnvGuard::set("LOOM_WORKTREE_ROOT", &worktrees);
        let roots = || candidate_roots_with_dirs(&repo, tmp.path(), tmp.path());
        let volume = volume_root_of(&configured).unwrap();
        assert!(roots().contains(&volume), "LOOM_WORKTREE_ROOT");

        std::fs::write(
            repo.join(".loom/config.json"),
            serde_json::json!({ "worktree": { "root": worktrees } }).to_string(),
        )
        .unwrap();
        std::env::remove_var("LOOM_WORKTREE_ROOT");
        assert!(roots().contains(&volume), "worktree.root");
    }

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
        assert_eq!(
            evaluate_root(&file, "acme/widgets", &probes),
            RootVerdict::Preserve("not a directory")
        );
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
            evaluate_root(&root, "acme/widgets", &probes),
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
            evaluate_root(&root, "acme/widgets", &probes),
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
            evaluate_root(&link, "acme/widgets", &probes),
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
            directory_in_use: probes.directory_in_use,
            mount_id_of: &mount_device,
        };
        assert_eq!(
            evaluate_root(&mount, "acme/widgets", &probing),
            RootVerdict::Preserve("root is a mount point")
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
            pid_alive: &process_activity::pid_alive,
            directory_in_use: probes.directory_in_use,
            mount_id_of: probes.mount_id_of,
        };
        assert_eq!(
            evaluate_root(&root, "acme/widgets", &probing),
            RootVerdict::Preserve("watch-pid file names a live process")
        );

        // Dead pid passes the gate (falls through to the next).
        std::fs::write(root.join("watch-pid"), "999999997").unwrap();
        backdate(&root, now.timestamp() - 48 * 3600);
        assert_eq!(
            evaluate_root(&root, "acme/widgets", &probing),
            RootVerdict::Preserve("no git worktree inside the root")
        );

        // A malformed marker also preserves the root before any worktree scan.
        std::fs::write(root.join("watch-pid"), "not-a-pid").unwrap();
        backdate(&root, now.timestamp() - 48 * 3600);
        assert_eq!(
            evaluate_root(&root, "acme/widgets", &probing),
            RootVerdict::Preserve("watch-pid file could not be read or parsed")
        );
    }

    #[test]
    fn incomplete_process_checks_never_emit_or_apply_a_removal() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = [tmp.path().to_owned()];
        let root = tmp.path().join("loom-202-builder.cafe");
        let source = root.join("source");
        init_main_with_origin(&source, &tmp.path().join("origin.git"));
        git(
            &source,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        let (probes, now) = probes_now(IDLE_WINDOW_SECS, IDLE_WINDOW_SECS);
        let probes = ScratchRootProbes {
            pid_alive: &|_| None,
            directory_in_use: &|_| None,
            ..probes
        };
        let assert_preserved = |reason| {
            backdate(&root, now.timestamp() - 2 * IDLE_WINDOW_SECS);
            let events = Mutex::new(Vec::new());
            for dry_run in [true, false] {
                let report =
                    sweep(tmp.path(), Some("acme/widgets"), &roots, &probes, dry_run, &events);
                assert_eq!(
                    report.preserved,
                    [PreservedRoot {
                        path: root.clone(),
                        reason
                    }]
                );
                assert!(report.planned.is_empty() && report.removed.is_empty());
                assert!(root.is_dir());
            }
            assert!(events.into_inner().unwrap().is_empty());
        };
        assert_preserved("process activity under the root could not be verified");
        let marker = root.join("watch-pid");
        std::fs::write(&marker, "42").unwrap();
        assert_preserved("watch-pid liveness unknown");
        // ENOTDIR looking up a marker below a regular file is not absence.
        assert!(matches!(watch_pid_state(&marker, &never_alive), WatchPidState::Unreadable));
    }

    fn published_scratch_fixture() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-203-builder.cafe");
        let source = root.join("source");
        init_main_with_origin(&source, &tmp.path().join("origin.git"));
        git(
            &source,
            &[
                "remote",
                "set-url",
                "origin",
                "https://github.com/acme/widgets.git",
            ],
        );
        (tmp, root)
    }

    fn checked_sweep(
        base: &Path,
        probes: &ScratchRootProbes<'_>,
        dry_run: bool,
    ) -> ScratchRootReport {
        let events = Mutex::new(Vec::new());
        let report =
            sweep(base, Some("acme/widgets"), &[base.to_owned()], probes, dry_run, &events);
        assert_eq!(
            if dry_run {
                &report.planned
            } else {
                &report.removed
            },
            &events.into_inner().unwrap()
        );
        report
    }

    #[test]
    fn nested_or_unknown_mount_preserves_every_byte_in_preview_and_live_cleanup() {
        let (tmp, root) = published_scratch_fixture();
        let canonical = root.canonicalize().unwrap();
        let cache = canonical.join("build/cache");
        let payload = cache.join("payload");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(&payload, b"mounted data").unwrap();
        let (probes, now) = probes_now(IDLE_WINDOW_SECS, IDLE_WINDOW_SECS);
        backdate(&root, now.timestamp() - 2 * IDLE_WINDOW_SECS);
        assert_eq!(checked_sweep(tmp.path(), &probes, true).planned.len(), 1);

        for (boundary, id) in [
            (&cache, Some(2)),
            (&payload, Some(2)),
            (&payload, None),
            (&root, None),
        ] {
            let probe = |p: &Path| if p == boundary { id } else { Some(1) };
            let probes = ScratchRootProbes {
                mount_id_of: &probe,
                ..probes
            };
            for dry_run in [true, false] {
                let report = checked_sweep(tmp.path(), &probes, dry_run);
                assert_eq!(report.preserved.len(), 1);
                assert_eq!(
                    report.preserved[0].reason,
                    if id.is_some() {
                        "root contains a mount point"
                    } else {
                        mount_boundary::UNKNOWN
                    }
                );
                assert!(report.planned.is_empty() && report.removed.is_empty());
                assert_eq!(std::fs::read(&payload).unwrap(), b"mounted data");
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn closed_descriptor_keeps_root_active_until_file_is_unmapped() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::symlink;
        let (tmp, root) = published_scratch_fixture();
        let data = root.join("mapped index");
        std::fs::write(&data, b"x").unwrap();
        let procfs = tmp.path().join("proc");
        std::fs::create_dir(&procfs).unwrap();
        symlink("/proc/self", procfs.join(std::process::id().to_string())).unwrap();
        let (probes, now) = probes_now(IDLE_WINDOW_SECS, IDLE_WINDOW_SECS);
        let probes = ScratchRootProbes {
            directory_in_use: &|path| process_activity::scan_proc(&procfs, path),
            ..probes
        };
        backdate(&root, now.timestamp() - 2 * IDLE_WINDOW_SECS);
        assert_eq!(checked_sweep(tmp.path(), &probes, true).planned.len(), 1);

        let file = std::fs::File::open(data).unwrap();
        // SAFETY: a readable, nonempty file backs the one-byte read-only map.
        let mapping = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                1,
                libc::PROT_READ,
                libc::MAP_PRIVATE,
                file.as_raw_fd(),
                0,
            )
        };
        assert_ne!(mapping, libc::MAP_FAILED);
        drop(file);
        for dry_run in [true, false] {
            let report = checked_sweep(tmp.path(), &probes, dry_run);
            assert!(report.planned.is_empty() && report.removed.is_empty());
            assert_eq!(
                report.preserved[0].reason,
                "a process has its cwd, executable, open file or mapped file under the root"
            );
        }
        // SAFETY: release the successful mmap above with its original length.
        assert_eq!(unsafe { libc::munmap(mapping, 1) }, 0);
        assert_eq!(checked_sweep(tmp.path(), &probes, true).planned.len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn preview_skips_a_running_executable_with_cwd_and_fds_outside_the_root() {
        use std::os::unix::fs::symlink;
        use std::process::Stdio;

        let (tmp, root) = published_scratch_fixture();
        // Multicall coreutils selects the applet by the executable's basename.
        let worker = root.join("cat");
        std::fs::copy("/bin/cat", &worker).unwrap();

        // Limit the scan to real procfs entries we can inspect on any CI host.
        let procfs = tmp.path().join("proc");
        std::fs::create_dir(&procfs).unwrap();
        symlink("/proc/self", procfs.join(std::process::id().to_string())).unwrap();
        let (probes, now) = probes_now(IDLE_WINDOW_SECS, IDLE_WINDOW_SECS);
        let probes = ScratchRootProbes {
            directory_in_use: &|path| process_activity::scan_proc(&procfs, path),
            ..probes
        };
        backdate(&root, now.timestamp() - 2 * IDLE_WINDOW_SECS);
        let preview = || checked_sweep(tmp.path(), &probes, true);
        assert_eq!(preview().planned.len(), 1);

        // cat waits for EOF on its input pipe. Its executable is the only
        // reference to the candidate while it runs.
        let child = Command::new(&worker)
            .current_dir(tmp.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        symlink(format!("/proc/{}", child.id()), procfs.join(child.id().to_string())).unwrap();
        let active = preview();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "scratch cat exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(active.planned.is_empty());
        assert_eq!(
            active.preserved[0].reason,
            "a process has its cwd, executable, open file or mapped file under the root"
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
            directory_in_use: probes.directory_in_use,
            mount_id_of: probes.mount_id_of,
        };
        let unknown_probes = ScratchRootProbes {
            ticket_finished: &unknown_answer,
            ..open_probes
        };
        assert_eq!(
            evaluate_root(&root, "acme/widgets", &open_probes),
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
            evaluate_root(&root, "acme/widgets", &open_probes),
            RootVerdict::Preserve("linked ticket is open")
        );
        assert_eq!(
            evaluate_root(&root, "acme/widgets", &unknown_probes),
            RootVerdict::Preserve("linked ticket state unknown (probe failed)")
        );
    }

    #[test]
    fn test_evaluate_root_processes_preserved() {
        fn holder_processes(_: &Path) -> Option<bool> {
            Some(true)
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
            directory_in_use: &holder_processes,
            mount_id_of: probes.mount_id_of,
        };
        assert_eq!(
            evaluate_root(&root, "acme/widgets", &probing),
            RootVerdict::Preserve("no git worktree inside the root")
        );
    }

    // ===================================================================
    // remove_root: read-only trees
    // ===================================================================

    #[cfg(unix)]
    #[test]
    fn test_remove_root_unlinks_read_only_trees() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-15-hermit.007a");
        let ro = root.join("pkg/mod/cache/download");
        std::fs::create_dir_all(&ro).unwrap();
        std::fs::write(ro.join("file"), b"go module cache entry").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
        std::fs::set_permissions(ro.join("file"), std::fs::Permissions::from_mode(0o444)).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        mount_boundary::remove_root(&root)
            .expect("owner-write permission on directories permits removal");
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
        let open_file_root = base.join("loom-103-builder.dddd").canonicalize().unwrap();
        let process_probe = |p: &Path| Some(p == open_file_root);
        // Root 110 sits on another device.
        let device_probe = |p: &Path| if p == mount_root { Some(2) } else { Some(1) };
        let probes = ScratchRootProbes {
            now,
            idle_window_secs: 24 * 3600,
            max_age_secs: 24 * 3600,
            ticket_finished: &ticket_probe,
            pid_alive: &process_activity::pid_alive,
            directory_in_use: &process_probe,
            mount_id_of: &device_probe,
        };

        let recorder = Mutex::new(Vec::new());
        let report = sweep(
            base,
            Some("acme/widgets"),
            &[base.to_path_buf()],
            &probes,
            /* dry_run = */ false,
            &recorder,
        );

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
            "a process has its cwd, executable, open file or mapped file under the root"
        );
        assert_eq!(reason_for("loom-104-builder.eeee"), "files changed within the 24h idle window");
        assert_eq!(reason_for("loom-105-builder.ffff"), "worktree has tracked changes");
        assert_eq!(reason_for("loom-106-judge.1111"), "worktree HEAD has commits no remote holds");
        assert_eq!(reason_for("loom-107-judge.2222"), "no resolvable GitHub origin repository");
        assert_eq!(reason_for("loom-108-judge.3333"), "no resolvable GitHub origin repository");
        assert_eq!(reason_for("loom-109-judge.4444"), "root is a symlink (never followed)");
        assert_eq!(reason_for("loom-110-judge.5555"), "root is a mount point");

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
    #[serial]
    fn fixture_unconfigured_pass_previews_eligible_root() {
        use crate::scratch_reclaim::{
            read_scratch_reclaim_config, resolve_dry_run, SCRATCH_RECLAIM_DRY_RUN_ENV,
        };
        let _env = crate::env_test_support::EnvGuard::clear(&[SCRATCH_RECLAIM_DRY_RUN_ENV]);
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
            directory_in_use: &no_processes,
            mount_id_of: &same_device,
        };
        let recorder = Mutex::new(Vec::new());
        let config = read_scratch_reclaim_config(base);
        assert_eq!(config.dry_run, None);
        let bytes = mount_boundary::tree_size(&root, 1, &same_device).unwrap();
        assert!(bytes > 0);
        let report = sweep(
            base,
            Some("acme/widgets"),
            &[base.to_path_buf()],
            &probes,
            resolve_dry_run(&config),
            &recorder,
        );
        assert!(root.exists(), "dry-run removes nothing");
        assert!(report.removed.is_empty());
        assert_eq!(report.planned.len(), 1);
        assert_eq!(report.planned[0].ticket, 200);
        let records = recorder.into_inner().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records, report.planned);
        let payload = records[0].payload();
        assert_eq!(payload["path"], root.to_str().unwrap());
        assert_eq!(payload["bytes"], bytes);
        assert_eq!(payload["dry_run"], true);
    }

    // ===================================================================
    // run_for — cooldown and config
    // ===================================================================

    #[test]
    fn preview_logs_path_and_reason_when_every_candidate_is_preserved() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("loom-9-builder.aa11");
        std::fs::create_dir(&root).unwrap();
        let (probes, _) = probes_now(IDLE_WINDOW_SECS, 24 * 3600);
        let report = sweep(
            tmp.path(),
            Some("acme/widgets"),
            &[tmp.path().to_owned()],
            &probes,
            true,
            &Mutex::new(Vec::new()),
        );
        let logs = crate::test_log_capture::capture_logs(|| log_report(&report));

        assert!(
            logs.iter().any(|(level, message)| {
                *level == log::Level::Info
                    && message.contains("0 root(s) (nothing) [dry-run], 1 preserved")
                    && message.contains(&format!(
                        "{} (root younger than scratchReclaim.maxAgeHours)",
                        root.display()
                    ))
            }),
            "preview summary: {logs:?}"
        );
    }

    #[test]
    #[serial]
    fn shared_root_uses_only_its_owners_policy() {
        use crate::env_test_support::EnvGuard;
        use crate::scratch_reclaim::{
            SCRATCH_RECLAIM_DRY_RUN_ENV, SCRATCH_RECLAIM_ENABLE_ENV,
            SCRATCH_RECLAIM_MAX_AGE_HOURS_ENV,
        };
        use crate::workspace_registry::{WorkspaceRegistry, REGISTRY_PATH_ENV};

        let tmp = tempfile::tempdir().unwrap();
        let registry_file = tmp.path().join("workspaces.json");
        let _env = EnvGuard::clear(&[
            SCRATCH_RECLAIM_DRY_RUN_ENV,
            SCRATCH_RECLAIM_ENABLE_ENV,
            SCRATCH_RECLAIM_MAX_AGE_HOURS_ENV,
            "GH_HOST",
        ])
        .and_set(REGISTRY_PATH_ENV, Some(registry_file.as_os_str()));
        let roots = [tmp.path().join("shared")];
        let candidate = roots[0].join("loom-200-builder.cafe");
        let source = candidate.join("source");
        init_main_with_origin(&source, &tmp.path().join("origin.git"));
        let url_b = "https://github.com/acme/b.git";
        git(
            &source,
            &[
                "remote",
                "set-url",
                "origin",
                "https://alice@GITHUB.COM/acme/b.git",
            ],
        );
        backdate(&candidate, Utc::now().timestamp() - 48 * 3600);

        let mut registry = WorkspaceRegistry::default();
        let configure = |ws: &Path, enabled: bool, dry_run: bool| {
            let config = serde_json::json!({"autonomous":{"worktreeReaper":{"scratchReclaim":{
                "enabled": enabled, "dryRun": dry_run
            }}}});
            std::fs::write(ws.join(".loom/config.json"), config.to_string()).unwrap();
        };
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        let url_a = "https://github.com/acme/a.git";
        for (ws, url) in [(&a, url_a), (&b, "ssh://git@github.com:22/Acme/B.git")] {
            std::fs::create_dir_all(ws.join(".loom")).unwrap();
            git(ws, &["init", "-q"]);
            git(ws, &["remote", "add", "origin", url]);
        }
        registry.add(&b, None).unwrap();
        registry.save(&registry_file).unwrap();
        configure(&a, true, false);
        configure(&b, true, true);
        let records = Mutex::new(Vec::new());
        let run = |ws: &Path| run_for_roots(ws, &roots, &always_finished, &no_processes, &records);
        let single = run(&b);
        assert_eq!(single.planned.len(), 1);
        assert_eq!(single.planned[0].path, candidate);
        assert!(single.removed.is_empty());
        assert!(candidate.is_dir());

        registry.add(&a, None).unwrap();
        registry.save(&registry_file).unwrap();
        reset_state_for_test();
        records.lock().unwrap().clear();
        for _ in 0..2 {
            assert!(run(&a).removed.is_empty());
            run(&b);
        }
        let events = records.lock().unwrap().clone();
        assert_eq!(events.len(), 1);
        assert!(events[0].dry_run);
        assert_eq!(events[0].path, candidate);
        assert!(events[0].repo.eq_ignore_ascii_case("acme/b"));
        assert!(candidate.is_dir());

        configure(&b, false, false);
        reset_state_for_test();
        assert!(run(&a).removed.is_empty());
        assert!(!run(&b).enabled);
        assert_eq!(records.lock().unwrap().len(), 1);
        assert!(candidate.is_dir());

        // Both can now claim the same candidate using different transports.
        git(&a, &["remote", "set-url", "origin", url_b]);
        configure(&b, true, true);
        let assert_no_actions = || {
            reset_state_for_test();
            for ws in [&a, &b] {
                let report = run(ws);
                assert!(report.planned.is_empty());
                assert!(report.removed.is_empty());
            }
            assert_eq!(records.lock().unwrap().len(), 1);
            assert!(candidate.is_dir());
        };
        assert_no_actions();
        configure(&b, false, false);
        assert_no_actions();

        // Without B's origin, A still cannot prove unique ownership.
        git(&b, &["remote", "remove", "origin"]);
        assert_no_actions();
    }

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
        let report =
            run_for_roots(tmp.path(), &roots, &always_finished, &no_processes, &ProductionRecorder);
        std::env::remove_var(crate::scratch_reclaim::SCRATCH_RECLAIM_ENABLE_ENV);
        assert!(!report.enabled);

        // Enabled: first pass runs (nothing to find), second is deferred.
        let first =
            run_for_roots(tmp.path(), &roots, &always_finished, &no_processes, &ProductionRecorder);
        assert!(first.enabled);
        assert!(first.deferred.is_none());
        let second =
            run_for_roots(tmp.path(), &roots, &always_finished, &no_processes, &ProductionRecorder);
        assert!(second.deferred.is_some(), "cooldown defers the second pass");
    }

    #[test]
    #[serial]
    fn test_run_for_dry_run_env() {
        reset_state_for_test();
        let tmp = tempfile::tempdir().unwrap();
        let roots = [tmp.path().to_path_buf()];
        let _env = crate::env_test_support::EnvGuard::clear(&[
            crate::scratch_reclaim::SCRATCH_RECLAIM_DRY_RUN_ENV,
        ]);
        assert!(
            run_for_roots(tmp.path(), &roots, &always_finished, &no_processes, &ProductionRecorder)
                .dry_run
        );

        std::env::set_var(crate::scratch_reclaim::SCRATCH_RECLAIM_DRY_RUN_ENV, "0");
        reset_state_for_test();
        assert!(
            !run_for_roots(
                tmp.path(),
                &roots,
                &always_finished,
                &no_processes,
                &ProductionRecorder
            )
            .dry_run
        );
    }
}
