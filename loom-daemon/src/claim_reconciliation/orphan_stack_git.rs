//! The git half of the orphaned-stacked-child pass (#12760): replay a PR
//! branch's own commits onto a new base in a scratch worktree and publish the
//! result with a pinned `--force-with-lease`.
//!
//! Split from [`super::orphan_stack`] so the decision module reads as policy
//! and this one as mechanics.
//!
//! # Why a scratch worktree
//!
//! `reconcile_stack::rebase` runs `git rebase --onto <target> <parent>
//! <child>` in whichever checkout holds the child branch, or else in the
//! repository directory, which checks the child branch out *there*. That is
//! fine for an operator at a terminal. It is not fine for a daemon pass whose
//! repository directory is the workspace's primary clone: the rebase would
//! switch the primary clone's branch. So every replay here runs in a detached
//! worktree created for the one replay and removed afterwards, the shape the
//! consolidation constructor already uses (`merge_pr::consolidate::construct`).
//! The child's local branch, if any, is never touched; only the remote branch
//! is updated, by an explicit `<sha>:refs/heads/<branch>` push.
//!
//! # The lease
//!
//! The same discipline `reconcile-stack.sh` follows (#9487,
//! `defaults/docs/push-lease-pinning.md`): the push is
//! `--force-with-lease=refs/heads/<branch>:<old>`, where `<old>` is the head
//! the replay started from, and [`live_tip`] confirms that the remote still has
//! exactly `<old>` (asked of the remote, never read from a remote-tracking ref)
//! before anything is rewritten. A sibling that pushed in between makes the
//! lease fail, and the pass retries on a later tick from the new head.
//!
//! # The guarded replay
//!
//! [`guarded_replay`] is the whole sequence both stack reconcilers run — the
//! orphaned-stacked-child pass ([`super::orphan_stack`]) and the merged-parent
//! one ([`super::merged_stack`]) — for a child and again for each descendant:
//! fetch the listed head, confirm the remote still has it, refuse a rewritten
//! parent, replay, ask the #9282 rebase guard, push leased. The callers bring
//! only what differs between them, the scratch worktree's path and the name
//! their diagnostics go under, and map [`ReplayOutcome`] onto their own flag
//! and retry handling. Discovery, retargeting, cascade decisions and forge
//! comments stay with the callers.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::cmd_out::{run_command, CmdOutcome};
use crate::rebase_guard::Verdict as GuardVerdict;

/// The remote every replay fetches from and pushes to, as in
/// `reconcile-stack.sh`.
pub const REMOTE: &str = "origin";

/// Network steps (`fetch`, `ls-remote`, `push`).
const NET_TIMEOUT: Duration = Duration::from_secs(600);
/// Local steps (`rev-parse`, `worktree`, `rebase`).
const LOCAL_TIMEOUT: Duration = Duration::from_secs(300);

fn git(dir: &Path, args: &[&str], timeout: Duration) -> CmdOutcome {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null());
    run_command(cmd, timeout)
}

/// Is `sha` a commit object readable in `repo`?
#[must_use]
pub fn has_commit(repo: &Path, sha: &str) -> bool {
    git(repo, &["cat-file", "-e", &format!("{sha}^{{commit}}")], LOCAL_TIMEOUT).succeeded()
}

/// What [`ensure_pr_head`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeadFetch {
    /// The commit is readable in the repository.
    Present,
    /// The fetch itself failed (network, auth, a timeout, a ref the remote
    /// did not serve). Transient: the caller retries on a later tick.
    FetchFailed(String),
    /// The fetch succeeded, but the commit is still not readable: the PR ref
    /// does not contain it.
    Missing,
}

/// Make `sha` (PR `pr`'s head) readable in `repo`, fetching
/// `refs/pull/<pr>/head` once if it is not. `refs/pull/<n>/head` survives the
/// head branch's deletion, which is the case for a parent whose branch was
/// removed after it closed.
#[must_use]
pub fn ensure_pr_head(repo: &Path, pr: u32, sha: &str) -> HeadFetch {
    if has_commit(repo, sha) {
        return HeadFetch::Present;
    }
    // `--` before the ref operand (#9106).
    let refname = format!("refs/pull/{pr}/head");
    let out = git(repo, &["fetch", "--quiet", REMOTE, "--", &refname], NET_TIMEOUT);
    if !out.succeeded() {
        return HeadFetch::FetchFailed(
            out.failure_reason(&format!("git fetch {REMOTE} {refname}")),
        );
    }
    if has_commit(repo, sha) {
        HeadFetch::Present
    } else {
        HeadFetch::Missing
    }
}

/// Is `ancestor` an ancestor of (or equal to) `descendant`?
///
/// The precondition for replaying `ancestor..descendant` as "the commits
/// `descendant` added on top of `ancestor`": when it does not hold, `git
/// rebase --onto <x> <ancestor>` does not fail, it replays every commit of
/// `descendant` that `ancestor` cannot reach, including commits of a parent
/// that was rewritten after the child branched (`reconcile_stack`'s
/// `Prerequisite::ParentAncestry`).
///
/// # Errors
/// `git merge-base` could not answer (exit other than 0 or 1, or it did not
/// run).
pub fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Result<bool, String> {
    let out = git(repo, &["merge-base", "--is-ancestor", ancestor, descendant], LOCAL_TIMEOUT);
    match &out {
        CmdOutcome::Ran(o) if o.status.success() => Ok(true),
        CmdOutcome::Ran(o) if o.status.code() == Some(1) => Ok(false),
        _ => Err(out.failure_reason("git merge-base --is-ancestor")),
    }
}

/// The commits in `range` (e.g. `a..b`), one local `git rev-list`.
///
/// # Errors
/// `git rev-list` could not run or exited non-zero.
pub fn rev_list(repo: &Path, range: &str) -> Result<std::collections::BTreeSet<String>, String> {
    let out = git(repo, &["rev-list", range], LOCAL_TIMEOUT);
    if !out.succeeded() {
        return Err(out.failure_reason("git rev-list"));
    }
    Ok(out.stdout_trimmed().lines().map(str::to_string).collect())
}

/// The BEST common ancestors of `a` and `b` (`git merge-base --all`), one
/// local spawn. Empty when the two histories share nothing (git's exit-1
/// "no common ancestor" answer, not a failure).
///
/// `--all`, never the single-answer form: a history that merged the other
/// side in (a child carrying an older default branch) has SEVERAL best
/// common ancestors, and git picks among them by commit date — the single
/// answer can name the default-branch side instead of the head being asked
/// about, silently missing the match. Membership in this set positively
/// proves ancestry; absence proves nothing and must fail to "no match".
///
/// # Errors
/// `git merge-base` could not answer (exit other than 0 or 1, or it did
/// not run).
pub fn merge_bases(
    repo: &Path,
    a: &str,
    b: &str,
) -> Result<std::collections::BTreeSet<String>, String> {
    let out = git(repo, &["merge-base", "--all", a, b], LOCAL_TIMEOUT);
    match &out {
        CmdOutcome::Ran(o) if o.status.success() || o.status.code() == Some(1) => {
            Ok(out.stdout_trimmed().lines().map(str::to_string).collect())
        }
        _ => Err(out.failure_reason("git merge-base --all")),
    }
}

/// The remote's live head for `branch`: `Ok(Some(sha))`, `Ok(None)` when the
/// remote answered and has no such branch, `Err` when it could not be asked.
///
/// # Errors
/// `git ls-remote` could not run or exited non-zero.
pub fn live_tip(repo: &Path, branch: &str) -> Result<Option<String>, String> {
    let out = git(repo, &["ls-remote", REMOTE, &format!("refs/heads/{branch}")], NET_TIMEOUT);
    if !out.succeeded() {
        return Err(out.failure_reason("git ls-remote"));
    }
    Ok(out
        .stdout_trimmed()
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .filter(|s| !s.is_empty())
        .map(str::to_string))
}

/// Why a replay did not produce a new head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayError {
    /// The replay stopped on conflicts in these paths. Nothing was published.
    Conflict(Vec<String>),
    /// Something else failed (the scratch worktree, a missing identity, …).
    /// Nothing was published; the caller retries on a later tick.
    Failed(String),
}

/// A detached scratch worktree, removed when dropped.
struct Scratch {
    repo: PathBuf,
    path: PathBuf,
}

impl Scratch {
    fn create(repo: &Path, path: &Path, at: &str) -> Result<Self, String> {
        // A leftover from a crashed run would make `worktree add` refuse.
        remove_worktree(repo, path);
        let path_str = path.to_str().ok_or("scratch path is not UTF-8")?;
        let out =
            git(repo, &["worktree", "add", "--detach", "--quiet", path_str, at], LOCAL_TIMEOUT);
        if !out.succeeded() {
            return Err(out.failure_reason("git worktree add"));
        }
        Ok(Self {
            repo: repo.to_path_buf(),
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        // A rebase stopped on a conflict holds the worktree; abort it first so
        // the removal below does not leave rebase state behind.
        let _ = git(&self.path, &["rebase", "--abort"], LOCAL_TIMEOUT);
        remove_worktree(&self.repo, &self.path);
    }
}

fn remove_worktree(repo: &Path, path: &Path) {
    if let Some(p) = path.to_str() {
        let _ = git(repo, &["worktree", "remove", "--force", p], LOCAL_TIMEOUT);
    }
    if path.exists() {
        let _ = std::fs::remove_dir_all(path);
    }
    let _ = git(repo, &["worktree", "prune"], LOCAL_TIMEOUT);
}

/// The scratch worktree path for PR `pr` under `repo`.
#[must_use]
pub fn scratch_path(repo: &Path, pr: u32) -> PathBuf {
    repo.join(format!(".loom/worktrees/orphan-restack-pr-{pr}"))
}

/// Replay the commits in `upstream..head` onto `onto`, in a scratch worktree
/// detached at `head`, and return the new head.
///
/// `head` must be readable in `repo` (the caller fetched it). Every commit
/// reachable from `upstream` is excluded, which is why `upstream` can be a
/// parent head that never merged anywhere: the replayed range is still
/// exactly the commits the child added on top of it.
///
/// # Errors
/// [`ReplayError::Conflict`] with the unmerged paths when the replay stops on
/// a conflict, [`ReplayError::Failed`] otherwise.
pub fn replay(
    repo: &Path,
    scratch: &Path,
    head: &str,
    onto: &str,
    upstream: &str,
) -> Result<String, ReplayError> {
    let wt = Scratch::create(repo, scratch, head).map_err(ReplayError::Failed)?;
    let out = git(&wt.path, &["rebase", "--onto", onto, "--", upstream], LOCAL_TIMEOUT);
    if !out.succeeded() {
        let unmerged = git(&wt.path, &["diff", "--name-only", "--diff-filter=U"], LOCAL_TIMEOUT)
            .stdout_trimmed();
        let paths: Vec<String> = unmerged
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect();
        if paths.is_empty() {
            return Err(ReplayError::Failed(out.failure_reason("git rebase --onto")));
        }
        return Err(ReplayError::Conflict(paths));
    }
    git(&wt.path, &["rev-parse", "HEAD"], LOCAL_TIMEOUT)
        .ok_stdout_trimmed()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ReplayError::Failed("could not read the replayed head".to_string()))
}

/// Publish `new_head` as `branch` on the remote, leased to `old_head`.
///
/// # Errors
/// The push was rejected (the lease failed, or the remote refused it).
pub fn push_leased(
    repo: &Path,
    branch: &str,
    new_head: &str,
    old_head: &str,
) -> Result<(), String> {
    let lease = format!("--force-with-lease=refs/heads/{branch}:{old_head}");
    let refspec = format!("{new_head}:refs/heads/{branch}");
    let out = git(repo, &["push", &lease, REMOTE, &refspec], NET_TIMEOUT);
    if out.succeeded() {
        Ok(())
    } else {
        Err(out.failure_reason("git push --force-with-lease"))
    }
}

/// Why the #9282 rebase guard refused a replayed head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardRefusal {
    /// The replayed head has the base's exact tree although the branch had a
    /// change of its own (`EMPTIED`).
    Emptied,
    /// The replay flattened `merge`, which held a hand-made conflict
    /// resolution the replayed head does not carry (`RESOLUTION_DISCARDED`).
    ResolutionDiscarded { merge: String },
}

impl GuardRefusal {
    /// The clause each reconciler's flag comment gives for this refusal.
    #[must_use]
    pub fn reason(&self) -> String {
        match self {
            Self::Emptied => "replaying it would leave no change of its own (the rebase guard \
                              reported EMPTIED, #9282)"
                .to_string(),
            Self::ResolutionDiscarded { merge } => format!(
                "replaying it would drop the hand resolution in merge {merge} (the rebase \
                 guard reported RESOLUTION_DISCARDED, #9282)"
            ),
        }
    }
}

/// Why a guarded replay refused to publish anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayRefusal {
    /// `upstream` is not an ancestor of `head`: the parent was rewritten after
    /// this branch was cut, so `upstream..head` is not "this branch's own
    /// commits".
    ParentRewritten,
    /// The replay stopped on conflicts in these paths.
    Conflict(Vec<String>),
    /// The rebase guard refused the replayed head.
    Guard(GuardRefusal),
}

/// What one guarded replay produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayOutcome {
    /// Published under a lease on `head`; the new head.
    Pushed(String),
    /// Nothing was published: the replay was refused before any push, and the
    /// caller flags it.
    Refused(ReplayRefusal),
    /// Nothing was published; worth retrying on a later tick.
    Transient(String),
}

/// The inputs of one [`guarded_replay`]: a PR branch's replay, and the two
/// things that differ between the reconcilers that run it.
#[derive(Debug, Clone, Copy)]
pub struct ReplayRequest<'a> {
    /// The PR whose head branch is replayed: `refs/pull/<pr>/head` is where
    /// `head` is fetched from, and the number its diagnostics name.
    pub pr: u32,
    /// The PR's head branch on the remote.
    pub branch: &'a str,
    /// The head the listing reported for `branch`.
    pub head: &'a str,
    /// What `upstream..head` is replayed onto.
    pub onto: &'a str,
    /// The last commit that is NOT replayed: the parent's head.
    pub upstream: &'a str,
    /// The detached scratch worktree the replay runs in, removed afterwards.
    pub scratch: &'a Path,
    /// The calling pass, as its other log lines name it.
    pub pass: &'static str,
}

/// Replay `upstream..head` of `branch` onto `onto`, check the result with the
/// #9282 rebase guard, and publish it with a lease on `head`.
///
/// Nothing is published unless every step holds. `head` is fetched through
/// `refs/pull/<pr>/head`, and the remote must still be at exactly `head`
/// ([`live_tip`]) when the replay starts; a sibling that pushed in between
/// makes the attempt [`ReplayOutcome::Transient`], retried from the new head
/// on a later tick. `upstream` must be readable in `repo` (the caller fetched
/// it) and must be an ancestor of `head` ([`is_ancestor`]), or the replayed
/// range is not "this branch's own commits" and the attempt is refused. A
/// replay conflict and a guard refusal ([`GuardVerdict::Emptied`],
/// [`GuardVerdict::ResolutionDiscarded`]) are refused too. A guard that gives
/// no answer does not block — `reconcile-stack.sh` pushes unchecked in that
/// case as well — so it is warned about under `pass`, and the push goes on.
pub fn guarded_replay(repo: &Path, req: &ReplayRequest<'_>) -> ReplayOutcome {
    let ReplayRequest {
        pr,
        branch,
        head,
        onto,
        upstream,
        scratch,
        pass,
    } = *req;
    match ensure_pr_head(repo, pr, head) {
        HeadFetch::Present => {}
        HeadFetch::FetchFailed(e) => {
            return ReplayOutcome::Transient(format!("its head {head} could not be fetched: {e}"))
        }
        HeadFetch::Missing => {
            return ReplayOutcome::Transient(format!(
                "its head {head} is not in refs/pull/{pr}/head"
            ))
        }
    }
    match live_tip(repo, branch) {
        Ok(Some(live)) if live == head => {}
        Ok(Some(live)) => {
            return ReplayOutcome::Transient(format!(
                "the remote branch is at {live}, not the listed head {head}"
            ))
        }
        Ok(None) => {
            return ReplayOutcome::Transient(format!("the remote has no branch '{branch}'"))
        }
        Err(e) => return ReplayOutcome::Transient(e),
    }
    // `rebase --onto <onto> <upstream>` with a non-ancestor upstream does not
    // fail: it replays every commit `upstream` cannot reach, which for a
    // parent rewritten after this branch was cut includes the parent's OLD
    // commits (reconcile_stack's `Prerequisite::ParentAncestry`). Refuse.
    match is_ancestor(repo, upstream, head) {
        Ok(true) => {}
        Ok(false) => return ReplayOutcome::Refused(ReplayRefusal::ParentRewritten),
        Err(e) => return ReplayOutcome::Transient(e),
    }
    let new_head = match replay(repo, scratch, head, onto, upstream) {
        Ok(h) => h,
        Err(ReplayError::Conflict(paths)) => {
            return ReplayOutcome::Refused(ReplayRefusal::Conflict(paths))
        }
        Err(ReplayError::Failed(e)) => return ReplayOutcome::Transient(e),
    };
    match crate::rebase_guard::assess(repo, onto, head, &new_head) {
        GuardVerdict::Clean(_) => {}
        GuardVerdict::Emptied { .. } => {
            return ReplayOutcome::Refused(ReplayRefusal::Guard(GuardRefusal::Emptied))
        }
        GuardVerdict::ResolutionDiscarded { merge, .. } => {
            return ReplayOutcome::Refused(ReplayRefusal::Guard(
                GuardRefusal::ResolutionDiscarded { merge },
            ))
        }
        GuardVerdict::Unanswered(why) => log::warn!(
            "claim_reconciliation ({pass}): PR #{pr}: rebase guard gave no answer ({why}); \
             pushing unchecked, as reconcile-stack.sh does (#9282)"
        ),
    }
    match push_leased(repo, branch, &new_head, head) {
        Ok(()) => ReplayOutcome::Pushed(new_head),
        Err(e) => ReplayOutcome::Transient(e),
    }
}

/// The commits in `upstream..head`, oldest first, as full shas, with a flag
/// for whether any of them is a merge commit.
///
/// # Errors
/// `git rev-list` could not answer.
pub fn range_commits(
    repo: &Path,
    upstream: &str,
    head: &str,
) -> Result<(Vec<String>, bool), String> {
    let range = format!("{upstream}..{head}");
    let out = git(repo, &["rev-list", "--reverse", &range], LOCAL_TIMEOUT);
    if !out.succeeded() {
        return Err(out.failure_reason("git rev-list"));
    }
    let commits: Vec<String> = out
        .stdout_trimmed()
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();
    let merges = git(repo, &["rev-list", "--merges", "--max-count=1", &range], LOCAL_TIMEOUT);
    if !merges.succeeded() {
        return Err(merges.failure_reason("git rev-list --merges"));
    }
    Ok((commits, !merges.stdout_trimmed().is_empty()))
}

/// Publish `new_head` as the NEW branch `branch` on the remote: a plain push,
/// never forced, so it can neither rewrite an existing branch nor race one.
///
/// `Ok(())` also when the remote already has `branch` at exactly `new_head`
/// (a retry of an earlier run that pushed but did not finish).
///
/// # Errors
/// The branch exists at another commit, or the push was rejected.
pub fn push_new_branch(repo: &Path, branch: &str, new_head: &str) -> Result<(), String> {
    match live_tip(repo, branch)? {
        Some(live) if live == new_head => return Ok(()),
        Some(live) => {
            return Err(format!(
                "the remote already has branch '{branch}' at {live}, not {new_head}"
            ))
        }
        None => {}
    }
    let refspec = format!("{new_head}:refs/heads/{branch}");
    let out = git(repo, &["push", REMOTE, &refspec], NET_TIMEOUT);
    if out.succeeded() {
        Ok(())
    } else {
        Err(out.failure_reason("git push"))
    }
}

/// Make the remote branch `branch`, currently at `tip`, readable in `repo`.
///
/// # Errors
/// The fetch failed or did not bring `tip`.
pub fn fetch_branch(repo: &Path, branch: &str, tip: &str) -> Result<(), String> {
    if has_commit(repo, tip) {
        return Ok(());
    }
    let refname = format!("refs/heads/{branch}");
    let out = git(repo, &["fetch", "--quiet", REMOTE, "--", &refname], NET_TIMEOUT);
    if !out.succeeded() {
        return Err(out.failure_reason(&format!("git fetch {REMOTE} {refname}")));
    }
    if has_commit(repo, tip) {
        Ok(())
    } else {
        Err(format!("fetched {refname} but {tip} is not in it"))
    }
}

#[cfg(test)]
#[path = "orphan_stack_git_tests.rs"]
mod tests;
