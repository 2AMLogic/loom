//! Shared descendant-cascade walk, issue-lease check, comment-marker, live
//! `gh` forge-adapter and `reconcile()` preamble helpers for
//! [`super::orphan_stack`] and [`super::merged_stack`] (#15597, #15638).

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use super::open_pr_listing::RestPull;
use super::orphan_stack::{self as orphan, HeadOwner, Skip};
use super::orphan_stack_git as git;
use crate::branch_update::Strategy;

/// How many stack levels above a restacked child are replayed in one pass.
pub(super) const MAX_CASCADE_DEPTH: usize = 8;

#[must_use]
pub(super) fn pushed_marker(prefix: &str, parent: u32, old_head: &str, new_head: &str) -> String {
    format!("{prefix} parent={parent} old={old_head} new={new_head} -->")
}

#[must_use]
pub(super) fn pushed_earlier(
    marker_prefix: &str,
    bodies: &[String],
    parent: u32,
    head: &str,
) -> Option<String> {
    let suffix = format!(" new={head} -->");
    let prefix = format!("{marker_prefix} parent={parent} old=");
    bodies.iter().find_map(|b| {
        b.lines().find_map(|l| {
            let old = l.trim().strip_prefix(&prefix)?.strip_suffix(&suffix)?;
            (!old.is_empty() && !old.contains(' ')).then(|| old.to_string())
        })
    })
}

#[must_use]
pub(super) fn flagged_marker(prefix: &str, parent: u32, head: &str) -> String {
    format!("{prefix} parent={parent} head={head} -->")
}

#[must_use]
pub(super) fn already_flagged(prefix: &str, bodies: &[String], parent: u32, head: &str) -> bool {
    let marker = flagged_marker(prefix, parent, head);
    bodies.iter().any(|b| b.contains(&marker))
}

/// Parse the Loom issue number out of `branch` (`feature/issue-<N>`) and ask
/// `issue_blocks` whether its lease is live or unreadable.
pub(super) fn branch_lease_blocks(branch: &str, issue_blocks: impl FnOnce(u32) -> bool) -> bool {
    crate::merge_pr::reconcile::issue_from_branch(branch)
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(issue_blocks)
}

/// Why a replay or descendant cascade step was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CascadeFlag {
    Conflict(Vec<String>),
    ParentRewritten,
    NotLoomBranch,
    Refused(String),
}

impl From<git::ReplayRefusal> for CascadeFlag {
    fn from(refusal: git::ReplayRefusal) -> Self {
        match refusal {
            git::ReplayRefusal::ParentRewritten => Self::ParentRewritten,
            git::ReplayRefusal::Conflict(paths) => Self::Conflict(paths),
            git::ReplayRefusal::Guard(g) => Self::Refused(g.reason()),
        }
    }
}

/// One open descendant PR visited during [`cascade`], paired with its rewritten parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CascadeStep<'a> {
    pub pr: u32,
    pub branch: &'a str,
    pub head: &'a str,
    pub parent_pr: u32,
    pub parent_branch: &'a str,
    pub old_parent_head: &'a str,
    pub new_parent_head: &'a str,
}

/// Pass configuration and callbacks for [`cascade`].
pub(super) struct Cascade<'a> {
    pub repo: &'a Path,
    pub open: &'a [RestPull],
    pub this_repo: Option<&'a str>,
    pub default_branch: &'a str,
    pub pass: &'static str,
    pub issue_tag: &'static str,
    pub scratch_path: fn(&Path, u32) -> PathBuf,
    pub issue_lease_blocks: &'a dyn Fn(u32) -> bool,
    pub on_cascaded: &'a mut dyn FnMut(&CascadeStep<'_>, &str),
    pub on_flag: &'a mut dyn FnMut(&CascadeStep<'_>, CascadeFlag),
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct CascadeCounts {
    pub cascaded: usize,
    pub skipped: usize,
}

/// Replay every open PR stacked on `child` from `old_head` onto `new_head`
/// (base unchanged), up to [`MAX_CASCADE_DEPTH`] levels.
pub(super) fn cascade(
    child: &RestPull,
    old_head: &str,
    new_head: &str,
    spec: &mut Cascade<'_>,
) -> CascadeCounts {
    let mut counts = CascadeCounts::default();
    cascade_at(child, old_head, new_head, 1, spec, &mut counts);
    counts
}

fn cascade_at(
    child: &RestPull,
    old_head: &str,
    new_head: &str,
    depth: usize,
    spec: &mut Cascade<'_>,
    counts: &mut CascadeCounts,
) {
    let Some(child_branch) = child.head_ref.as_deref() else {
        return;
    };
    for g in spec
        .open
        .iter()
        .filter(|p| p.state == "open" && p.number != child.number)
        .filter(|p| p.base_ref.as_deref() == Some(child_branch))
    {
        let head = g.head_sha.as_deref().unwrap_or_default();
        let branch = g.head_ref.as_deref().unwrap_or_default();
        let step = CascadeStep {
            pr: g.number,
            branch,
            head,
            parent_pr: child.number,
            parent_branch: child_branch,
            old_parent_head: old_head,
            new_parent_head: new_head,
        };
        let reason = if depth > MAX_CASCADE_DEPTH {
            Some(CascadeFlag::Refused(format!(
                "the stack above #{} is deeper than {MAX_CASCADE_DEPTH} levels",
                child.number
            )))
        } else {
            match orphan::gate(g) {
                Some(Skip::Held) => {
                    log::warn!(
                        "claim_reconciliation ({pass}): PR #{} is stacked on rewritten #{} \
                         but carries a hold label — not touched",
                        g.number,
                        child.number,
                        pass = spec.pass,
                    );
                    counts.skipped += 1;
                    continue;
                }
                Some(Skip::NoHead) => Some(CascadeFlag::Refused(
                    "its head branch or head commit is not usable".to_string(),
                )),
                Some(Skip::InFlight) => Some(CascadeFlag::Refused(
                    "an agent is working on it (loom:reviewing / loom:treating)".to_string(),
                )),
                None => match orphan::head_owner(g, spec.default_branch, spec.this_repo) {
                    HeadOwner::DefaultBranch => {
                        log::debug!(
                            "claim_reconciliation ({pass}): PR #{}'s head is '{default}' — \
                             not a stacked child, skipped",
                            g.number,
                            pass = spec.pass,
                            default = spec.default_branch,
                        );
                        continue;
                    }
                    HeadOwner::Foreign | HeadOwner::Unknown => Some(CascadeFlag::NotLoomBranch),
                    HeadOwner::Loom => None,
                },
            }
        };
        let reason = match reason {
            None if branch_lease_blocks(branch, spec.issue_lease_blocks) => {
                Some(CascadeFlag::Refused(
                    "its issue lease is held, so a Builder may be pushing to it".to_string(),
                ))
            }
            other => other,
        };
        if let Some(reason) = reason {
            (spec.on_flag)(&step, reason);
            continue;
        }
        let scratch = (spec.scratch_path)(spec.repo, g.number);
        match git::guarded_replay(
            spec.repo,
            &git::ReplayRequest {
                pr: g.number,
                branch,
                head,
                onto: new_head,
                upstream: old_head,
                scratch: &scratch,
                pass: spec.pass,
            },
        ) {
            git::ReplayOutcome::Pushed(g_new) => {
                counts.cascaded += 1;
                log::info!(
                    "claim_reconciliation ({pass}): PR #{} (stacked on #{}) replayed onto \
                     {new_head}, pushed {g_new} leased to {head} ({tag})",
                    g.number,
                    child.number,
                    pass = spec.pass,
                    tag = spec.issue_tag,
                );
                (spec.on_cascaded)(&step, &g_new);
                cascade_at(g, head, &g_new, depth + 1, spec, counts);
            }
            git::ReplayOutcome::Refused(r) => (spec.on_flag)(&step, r.into()),
            // The discovery query will not see this PR again once its parent
            // reconciled, so a transient failure here is flagged, not retried.
            git::ReplayOutcome::Transient(e) => (spec.on_flag)(&step, CascadeFlag::Refused(e)),
        }
    }
}

/// The telemetry names a stack pass's shared forge calls are booked under.
/// Each pass keeps its own, so sharing one adapter moves no call from one
/// pass's operation to the other's.
pub(super) struct StackForgeOps {
    pub parent_lookup: &'static str,
    pub comments: &'static str,
    pub retarget: &'static str,
    pub comment: &'static str,
}

/// The live forge plumbing both stack passes share, through the
/// claim-reconciliation `gh` facade.
pub(super) struct GhStackForge<'a> {
    gh_bin: &'a Path,
    root: &'a Path,
    ops: &'static StackForgeOps,
}

impl<'a> GhStackForge<'a> {
    pub(super) fn new(gh_bin: &'a Path, root: &'a Path, ops: &'static StackForgeOps) -> Self {
        Self { gh_bin, root, ops }
    }

    /// `gh api` with write intent; any non-zero exit is its trimmed stderr.
    pub(super) fn api_write(&self, op: &'static str, args: &[&str]) -> Result<(), String> {
        let out = super::gh_call::output(
            super::gh_call::write(op, self.gh_bin, self.root)
                .args(["api"])
                .args(args),
        )
        .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            Err(super::gh_call::stderr(&out))
        }
    }

    /// The newest closed PR whose head is `branch` (`Ok(None)` = none),
    /// read under the pass's `parent_lookup` op.
    pub(super) fn newest_closed_pr_for_head(
        &self,
        branch: &str,
    ) -> Result<Option<orphan::ParentPr>, String> {
        let path = format!(
            "repos/{{owner}}/{{repo}}/pulls?head={{owner}}:{branch}&state=closed&per_page=100"
        );
        let out = super::gh_call::output(
            super::gh_call::read(self.ops.parent_lookup, self.gh_bin, self.root)
                .args(["api", &path]),
        )
        .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(super::gh_call::stderr(&out));
        }
        orphan::parse_newest_closed(&out.stdout, branch)
    }

    /// Trusted comment bodies on PR `pr`, oldest first; `None` = unreadable.
    pub(super) fn trusted_comments(&self, pr: u32) -> Option<Vec<String>> {
        crate::merge_pr::sequence::fetch_trusted_bodies_as(
            self.ops.comments,
            &self.gh_bin.to_string_lossy(),
            self.root,
            "{owner}/{repo}",
            pr,
        )
    }

    /// Is issue `issue`'s lease fresh? `None` = could not tell.
    pub(super) fn issue_lease_fresh(&self, issue: u32) -> Option<bool> {
        use super::forge::LeaseProbe;
        match super::forge::fetch_freshest_lease_updated_at(self.gh_bin, self.root, issue) {
            LeaseProbe::Found(at) => Some(super::lease_is_fresh(
                at,
                chrono::Utc::now(),
                super::resolve_lease_ttl_minutes(),
            )),
            LeaseProbe::NotFound => Some(false),
            LeaseProbe::ReadFailed => None,
        }
    }

    /// The repository's branch-update strategy.
    pub(super) fn strategy(&self) -> Strategy {
        let nwo = std::env::var("LOOM_REPO").ok();
        crate::branch_update::resolve(Some(self.root), nwo.as_deref(), true).strategy
    }

    /// This repository's `owner/name`. `None` = could not tell.
    pub(super) fn repository(&self) -> Option<String> {
        crate::worktree_ops::gh::resolve_owner_repo(self.root).map(|(o, r)| format!("{o}/{r}"))
    }

    /// Change PR `pr`'s base to `base`.
    pub(super) fn retarget(&self, pr: u32, base: &str) -> Result<(), String> {
        let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
        let field = format!("base={base}");
        self.api_write(self.ops.retarget, &["-X", "PATCH", &path, "-f", &field])
    }

    /// Post `body` on PR `pr`.
    pub(super) fn comment(&self, pr: u32, body: &str) -> Result<(), String> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments");
        let field = format!("body={body}");
        self.api_write(self.ops.comment, &["-X", "POST", &path, "-f", &field])
    }
}

/// The setup both `reconcile()` entry points share after their enabled
/// check: the role-shard write gate, the default branch, and the open-PR
/// listing — the one the preceding pass handed down, else a fresh one.
/// `None` = the pass is skipped for this tick (`pass` names it in the log).
pub(super) fn prepare_reconcile<'a>(
    pass: &'static str,
    gh_bin: &Path,
    root: &Path,
    prefetched: Option<&'a [RestPull]>,
) -> Option<(String, Cow<'a, [RestPull]>)> {
    // One host per workspace writes (#10634's shard, as the merge-sequence
    // pass uses): two daemons racing the same stack would both read, and the
    // loser's lease would fail, but both would comment.
    if !crate::role_shard::decide(root).admits_role_tick() {
        return None;
    }
    let Some(default_branch) = crate::worktree_cli::default_branch::resolve(root) else {
        log::debug!(
            "claim_reconciliation ({pass}): {}: default branch unknown — pass skipped",
            root.display()
        );
        return None;
    };
    let open = match prefetched {
        Some(rows) => Cow::Borrowed(rows),
        None => match super::open_pr_listing::list_open_prs(gh_bin, root) {
            Ok(rows) => Cow::Owned(rows),
            Err(e) => {
                log::warn!("claim_reconciliation ({pass}): {}: {e}", root.display());
                return None;
            }
        },
    };
    Some((default_branch, open))
}

#[cfg(test)]
#[path = "stack_reconcile_tests.rs"]
mod tests;
