//! Stacked children of a parent that merged outside `merge-pr.sh` (#14903,
//! child of #14900): the daemon-side counterpart of the shell's post-merge
//! stacked reconcile, for a merge the shell never made — and for the children
//! the shell's own pass deferred because a Builder was still active.
//!
//! # The gap this closes
//!
//! `merge-pr.sh` brackets every merge it makes with stacked-child care: the
//! pre-merge guard pins the parent tip to `refs/loom/parent/<branch>`
//! ([`crate::merge_pr::stacked_children`]), the post-merge pass rebases or
//! defers each child ([`crate::merge_pr::stacked_reconcile`]), and the
//! pre-delete pass retargets every remaining child before the branch ref is
//! removed ([`crate::merge_pr::retarget_children`]). A merge made directly on
//! the forge — GitHub's own merge button, a queue-merged PR, a human — runs
//! none of that. With `delete_branch_on_merge` the branch and the child
//! relationship vanish synchronously inside the merge call: GitHub retargets
//! each open child onto the parent's own base, but nothing rebases it, so the
//! child still carries the parent's (now squash-merged) commits on top of a
//! base that already contains their content. Without auto-delete the children
//! keep targeting a branch whose PR is merged and will never advance.
//!
//! This pass runs inside the per-workspace reconciliation tick, AFTER
//! [`super::pass_loop::merge_observation`] so a merge observed this tick is
//! acted on this tick, and reuses the merged-PR records that pass pins
//! (#14902) as parent identities.
//!
//! # Discovery — two shapes, both positively evidenced
//!
//! - **Still based on the parent branch** (no auto-delete, or the shell's own
//!   merge before its delete step). The same predicate the orphan pass
//!   ([`super::orphan_stack`]) uses — an open PR whose base is a stackable
//!   Loom branch that is no open PR's head — but taking the
//!   [`super::orphan_stack::ParentState::Merged`] arm that pass deliberately
//!   leaves alone. The merged PR record supplies the pre-merge tip.
//! - **Already retargeted by GitHub** (auto-delete). The base is now the
//!   default branch, so the forge no longer records the relationship. It is
//!   recovered from git, in a bounded number of local calls per child
//!   (#15209): a child is matched to the NEWEST recorded merged parent whose
//!   landed head it positively carries — the head is one of the child's own
//!   commits or one of the best common ancestors with the default branch —
//!   while the merge commit is not an ancestor of it: exactly the state "was
//!   stacked on that parent, has not been replayed onto the merged default
//!   branch". A child that already contains the merge commit has been
//!   reconciled (by the shell, by hand, or by an earlier tick) and is left
//!   alone.
//!
//! # Parent-tip evidence: preserve or positively recover
//!
//! The pre-merge tip comes from the merged PR record (`refs/pull/<n>/head`
//! makes it fetchable long after the branch is deleted), never from a guess.
//! Whenever the pass acts on a child it (re-)writes the
//! `refs/loom/parent/<branch>` pin via [`crate::merge_pr::stacked_children::try_establish_pin`],
//! so the operator's `reconcile-stack.sh` fallback keeps working — the pin is
//! *preserved* when the shell wrote it and *positively recovered* when only
//! the forge's PR ref remains. A pin that disagrees with the recorded head
//! loses: the recorded head is what actually merged, and the guarded replay's
//! ancestry check ([`super::orphan_stack_git::guarded_replay`]) is what makes
//! a wrong boundary refuse rather than replay the wrong range.
//!
//! # Defer vs retarget-before-delete — the distinction is kept
//!
//! The shell's two decisions stay exactly where they were, and this pass
//! keeps them apart the same way: a Builder-active child
//! ([`crate::merge_pr::reconcile::child_route`] + [`route_with_lease`], the
//! stacked-reconcile decision) gets its rebase DEFERRED and one comment; a
//! child whose base must move gets a RETARGET. This pass never deletes a
//! branch ref, so the shell's pre-delete guard — including its unanswered
//! final child query keeping the branch — remains the only path that can.
//!
//! # Idempotency and overlap with the shell
//!
//! A restacked child contains the merge commit afterwards, so no record
//! matches it again (no duplicate push); a retargeted child's base is the
//! default branch, so the still-based discovery no longer matches it. The
//! deferral comment is deduplicated against BOTH bodies — this pass's and the
//! shell's byte-frozen [`crate::merge_pr::reconcile::defer_comment`], which
//! share their heading and hand command — so a shell deferral followed by a
//! tick (or vice versa) posts one comment, not two. A push whose retarget
//! failed leaves a pushed marker naming the new head, and the next tick
//! retries the retarget alone instead of misreading the rebased branch as a
//! rewritten parent (the #12769 shape).
//!
//! # Fail direction
//!
//! The rebase publishes nothing it cannot positively establish: an unanswered
//! parent lookup, an unfetchable head, a remote tip that is not the listed
//! head, a non-ancestor boundary, or an unanswered lease read each skip or
//! flag without writing. Hold and in-flight labels are respected via the
//! orphan pass's [`gate`](super::orphan_stack::gate), and only Loom-owned
//! heads in this repository are ever rewritten
//! ([`head_owner`](super::orphan_stack::head_owner)).
//!
//! Kill switch: [`MERGED_STACK_ENABLED_ENV`] (default ON), on the same
//! role-runner shard the orphan pass takes (#10634).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::open_pr_listing::RestPull;
use super::orphan_stack as orphan;
use super::orphan_stack_git as git;
use super::stack_reconcile;
use crate::branch_update::Strategy;
use crate::merge_pr::reconcile::{
    child_route, issue_from_branch, route_with_lease, LeaseState, Route,
};
use crate::merge_pr::stacked_children::{is_stackable_parent_branch, pin_ref, try_establish_pin};

/// Kill switch for this pass (`0`/`false`/`no`/`off` disables). Defaults ON.
pub const MERGED_STACK_ENABLED_ENV: &str = "LOOM_MERGED_STACK_RECONCILE";

/// Prefix of the marker on a deferral comment (this pass's own; the shell's
/// byte-frozen deferral is matched by content instead — see
/// [`defer_already_posted`]).
pub const DEFERRED_MARKER_PREFIX: &str = "<!-- loom:merged-stack deferred";

/// Prefix of the marker on the comment that reports a successful restack.
pub const RESTACKED_MARKER_PREFIX: &str = "<!-- loom:merged-stack restacked";

/// Prefix of the marker on a merge-mode base-only retarget.
pub const RETARGETED_MARKER_PREFIX: &str = "<!-- loom:merged-stack retargeted";

/// Prefix of the marker on the comment recording a restack whose push landed
/// but whose base retarget failed (the #12769 shape).
pub const PUSHED_MARKER_PREFIX: &str = "<!-- loom:merged-stack pushed";

/// Prefix of the marker on a flag comment (a child this pass could not
/// reconcile and will not guess at).
pub const FLAGGED_MARKER_PREFIX: &str = "<!-- loom:merged-stack flagged";

/// How many stack levels above a restacked child are replayed in one pass.
pub const MAX_CASCADE_DEPTH: usize = stack_reconcile::MAX_CASCADE_DEPTH;

/// Is this pass enabled? See [`MERGED_STACK_ENABLED_ENV`].
#[must_use]
pub fn merged_stack_enabled() -> bool {
    match std::env::var(MERGED_STACK_ENABLED_ENV) {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "false" | "no" | "off"),
        Err(_) => true,
    }
}

/// The recorded identity of one merged parent, from whichever discovery
/// source found it: the merged-PR record store (#14902) or the newest-merged
/// PR lookup the orphan pass uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedParent {
    pub pr: u32,
    /// The parent's head branch (`feature/issue-<N>` shaped).
    pub branch: String,
    /// The landed head — the pre-merge tip, fetchable through
    /// `refs/pull/<pr>/head` after the branch disappears.
    pub head: String,
    /// The merge commit (empty when the discovery source did not report one).
    pub merge_commit: String,
    /// RFC-3339 merge time (empty when unknown) — newest-first sort key.
    pub merged_at: String,
}

/// The `MergedParent` list from the merge-observation record store (#14902):
/// every record with a stackable branch and a pinnable identity, NEWEST first,
/// so a child stacked through several merged parents reconciles against the
/// nearest one.
#[must_use]
pub fn parents_from_records(records: &[merge_observation_record::Record]) -> Vec<MergedParent> {
    let mut parents: Vec<MergedParent> = records
        .iter()
        .filter(|r| is_stackable_parent_branch(&r.branch))
        .filter(|r| !r.head.is_empty() && !r.merge_commit.is_empty())
        .map(|r| MergedParent {
            pr: u32::try_from(r.pr).unwrap_or_default(),
            branch: r.branch.clone(),
            head: r.head.clone(),
            merge_commit: r.merge_commit.clone(),
            merged_at: r.merged_at.clone(),
        })
        .filter(|p| p.pr > 0)
        .collect();
    // `merged_at` is RFC-3339, so lexicographic order is chronological; the
    // PR number breaks ties (same second merges).
    parents.sort_by(|a, b| (b.merged_at.as_str(), b.pr).cmp(&(a.merged_at.as_str(), a.pr)));
    // The store is never pruned: bound the working set so a pass's cost cannot
    // grow with the repository's whole merge history.
    parents.truncate(MAX_MERGED_RECORDS);
    parents
}

/// The newest merge records a pass considers (older ones belong to stacks that
/// closed long ago).
pub const MAX_MERGED_RECORDS: usize = 300;

/// The slice of [`super::pass_loop::merge_observation::MergeRecord`] this
/// module reads, named so the mapping function above can stay pure-testable
/// without importing the whole sibling module into every caller's scope.
pub mod merge_observation_record {
    /// One stored merge record (field subset used by this pass).
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct Record {
        pub pr: u64,
        pub branch: String,
        pub head: String,
        pub merge_commit: String,
        pub merged_at: String,
    }
}

/// The marker line for a deferral this pass posted.
#[must_use]
pub fn deferred_marker(parent: u32, branch: &str) -> String {
    format!("{DEFERRED_MARKER_PREFIX} parent={parent} branch={branch} -->")
}

/// The marker line recording a restack whose push landed but whose retarget
/// failed. `head` is the child's current head: the marker counts only while
/// it names it as the pushed head.
#[must_use]
pub fn pushed_marker(parent: u32, old_head: &str, new_head: &str) -> String {
    stack_reconcile::pushed_marker(PUSHED_MARKER_PREFIX, parent, old_head, new_head)
}

/// If an earlier pass already pushed this child's replay and only its retarget
/// failed, the head it had before that push.
#[must_use]
pub fn pushed_earlier(bodies: &[String], parent: u32, head: &str) -> Option<String> {
    stack_reconcile::pushed_earlier(PUSHED_MARKER_PREFIX, bodies, parent, head)
}

/// The marker line for a flag on `head` caused by parent PR `parent`.
#[must_use]
pub fn flagged_marker(parent: u32, head: &str) -> String {
    stack_reconcile::flagged_marker(FLAGGED_MARKER_PREFIX, parent, head)
}

/// Has this pass already flagged the child at `head` for `parent`?
#[must_use]
pub fn already_flagged(bodies: &[String], parent: u32, head: &str) -> bool {
    stack_reconcile::already_flagged(FLAGGED_MARKER_PREFIX, bodies, parent, head)
}

/// Was this child already deferred for `parent_branch` — by this pass OR by
/// the shell's byte-frozen `defer_comment`? Both bodies carry the heading
/// `## Stacked parent merged — reconciliation deferred` and the hand command
/// `./.loom/scripts/reconcile-stack.sh <pr> <parent_branch>`, and those two
/// lines are the key: content, not authorship, is what overlap dedupe needs
/// (script-then-tick and tick-then-script must converge on one comment).
#[must_use]
pub fn defer_already_posted(bodies: &[String], parent_branch: &str, child_pr: u32) -> bool {
    let command = format!("./.loom/scripts/reconcile-stack.sh {child_pr} {parent_branch}");
    bodies.iter().any(|b| {
        b.contains("## Stacked parent merged — reconciliation deferred") && b.contains(&command)
    })
}

/// Why a child was flagged instead of restacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagReason {
    /// The replay stopped on conflicts in these paths.
    Conflict(Vec<String>),
    /// The recorded parent head is not an ancestor of this branch's head: the
    /// parent was rewritten after this branch was cut (or the evidence is
    /// stale), so the replay range cannot be established positively.
    ParentRewritten,
    /// The head branch is not a Loom branch in this repository.
    NotLoomBranch,
    /// The replay could not run or its result was refused.
    Refused(String),
}

impl From<stack_reconcile::CascadeFlag> for FlagReason {
    fn from(flag: stack_reconcile::CascadeFlag) -> Self {
        match flag {
            stack_reconcile::CascadeFlag::Conflict(paths) => Self::Conflict(paths),
            stack_reconcile::CascadeFlag::ParentRewritten => Self::ParentRewritten,
            stack_reconcile::CascadeFlag::NotLoomBranch => Self::NotLoomBranch,
            stack_reconcile::CascadeFlag::Refused(why) => Self::Refused(why),
        }
    }
}

/// Everything a flag comment interpolates.
#[derive(Debug, Clone)]
pub struct FlagContext<'a> {
    pub pr: u32,
    pub branch: &'a str,
    pub head: &'a str,
    pub parent_pr: u32,
    pub parent_branch: &'a str,
    pub upstream: &'a str,
    pub default_branch: &'a str,
    /// The base to retarget to afterwards (`None` for a cascade).
    pub retarget_to: Option<&'a str>,
    /// `Some(new_head)`: a cascade — `parent_pr` is still OPEN and was rewritten
    /// to this head; this PR keeps its base and must be replayed onto it.
    pub cascade_onto: Option<&'a str>,
}

/// The flag comment body.
#[must_use]
pub fn flag_comment_body(ctx: &FlagContext<'_>, reason: &FlagReason) -> String {
    if let Some(new_head) = ctx.cascade_onto {
        return cascade_flag_comment_body(ctx, reason, new_head);
    }
    let pin = pin_ref(ctx.parent_branch);
    let why = match reason {
        FlagReason::Conflict(paths) => format!(
            "replaying only this branch's own commits stopped on conflicts in: {}.",
            paths
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        FlagReason::ParentRewritten => format!(
            "the merged parent's recorded head `{}` is not an ancestor of this branch's head \
             `{}`. The parent was rewritten (amended or force-pushed) after this branch was cut \
             from it, or the recorded evidence is stale, so the replay range cannot be \
             established positively.",
            ctx.upstream, ctx.head
        ),
        FlagReason::Refused(why) => format!("Loom could not establish the replay: {why}."),
        FlagReason::NotLoomBranch => format!(
            "this PR's head branch `{}` is not a Loom branch in this repository (a human's \
             branch or a fork's), and Loom never rewrites those.",
            ctx.branch
        ),
    };
    let retarget = ctx
        .retarget_to
        .map_or(String::new(), |base| format!("gh pr edit {} --base {base}\n", ctx.pr));
    // A rewritten parent's recorded head is exactly the WRONG upstream for
    // the recipe: name a placeholder there instead, as the orphan pass's
    // flag does.
    let upstream = if matches!(reason, FlagReason::ParentRewritten) || ctx.upstream.is_empty() {
        "<the parent commit this branch was cut from>"
    } else {
        ctx.upstream
    };
    format!(
        "{}\n\
         ## Stacked parent merged — this branch could not be reconciled automatically\n\n\
         #{} (`{}`) merged while this PR was stacked on it, but Loom did not rewrite this \
         branch: {why}\n\n\
         To finish by hand, from a clean checkout:\n\n\
         ```\n\
         git fetch origin\n\
         git update-ref {pin} {}\n\
         git checkout --detach {head}\n\
         git rebase --onto origin/{default} {pin}\n\
         # resolve any conflicts, then:\n\
         git push --force-with-lease=refs/heads/{branch}:{head} origin HEAD:refs/heads/{branch}\n\
         {retarget}\
         ```\n\n\
         Loom checks again once this branch's head moves.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#14903)*",
        flagged_marker(ctx.parent_pr, ctx.head),
        ctx.parent_pr,
        ctx.parent_branch,
        upstream,
        head = ctx.head,
        branch = ctx.branch,
        default = ctx.default_branch,
    )
}

/// The marker of a cascade flag. Keyed by the rewritten parent's new head too,
/// so it can never be mistaken for the "parent merged" flag
/// ([`flagged_marker`]) that `process_child` honours once that parent itself
/// merges.
#[must_use]
pub fn cascade_flagged_marker(parent: u32, head: &str, onto: &str) -> String {
    format!("{FLAGGED_MARKER_PREFIX} parent={parent} head={head} onto={onto} -->")
}

/// The flag comment for a grandchild whose parent is still OPEN and was only
/// rewritten (restacked) — not merged. The recipe replays onto the parent's
/// new head and leaves the base alone.
fn cascade_flag_comment_body(ctx: &FlagContext<'_>, reason: &FlagReason, new_head: &str) -> String {
    let why = match reason {
        FlagReason::Conflict(paths) => format!(
            "replaying only this branch's own commits stopped on conflicts in: {}.",
            paths
                .iter()
                .map(|p| format!("`{p}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        FlagReason::ParentRewritten => format!(
            "the parent's previous head `{}` is not an ancestor of this branch's head `{}`, so \
             the replay range cannot be established positively.",
            ctx.upstream, ctx.head
        ),
        FlagReason::Refused(why) => format!("Loom could not establish the replay: {why}."),
        FlagReason::NotLoomBranch => format!(
            "this PR's head branch `{}` is not a Loom branch in this repository (a human's \
             branch or a fork's), and Loom never rewrites those.",
            ctx.branch
        ),
    };
    format!(
        "{}\n\
         ## Stacked parent rewritten — this branch could not be restacked automatically\n\n\
         #{} (`{}`), the PR this one is stacked on, is still open but Loom rewrote it (new head \
         `{new_head}`, previously `{}`), and did not restack this branch: {why}\n\n\
         This PR's base stays `{}`. To finish by hand, from a clean checkout:\n\n\
         ```\n\
         git fetch origin\n\
         git checkout --detach {head}\n\
         git rebase --onto {new_head} {}\n\
         # resolve any conflicts, then:\n\
         git push --force-with-lease=refs/heads/{branch}:{head} origin HEAD:refs/heads/{branch}\n\
         ```\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#14903)*",
        cascade_flagged_marker(ctx.parent_pr, ctx.head, new_head),
        ctx.parent_pr,
        ctx.parent_branch,
        ctx.upstream,
        ctx.parent_branch,
        ctx.upstream,
        head = ctx.head,
        branch = ctx.branch,
    )
}

/// The comment on a grandchild restacked because its still-open parent was
/// rewritten (base unchanged).
#[must_use]
pub fn cascaded_comment_body(
    parent: u32,
    parent_branch: &str,
    old_head: &str,
    new_head: &str,
    parent_new_head: &str,
) -> String {
    let marker =
        format!("{RESTACKED_MARKER_PREFIX} parent={parent} old={old_head} new={new_head} -->");
    format!(
        "{marker}\n\
         ## Stacked parent rewritten — replayed onto its new head\n\n\
         #{parent} (`{parent_branch}`), the open PR this one is stacked on, was rewritten by \
         Loom (new head `{parent_new_head}`). Loom replayed only this branch's own commits onto \
         it, kept this PR's base, and pushed the result with a lease pinned to `{old_head}`.\n\n\
         - Old head: `{old_head}`\n\
         - New head: `{new_head}`\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#14903)*"
    )
}

/// The comment reporting a successful restack (+ retarget).
#[must_use]
pub fn restacked_comment_body(
    parent: u32,
    parent_branch: &str,
    old_head: &str,
    new_head: &str,
    default_branch: &str,
    target_commit: &str,
    retargeted: bool,
) -> String {
    let base_clause = if retargeted {
        format!(" and changed this PR's base to `{default_branch}`")
    } else {
        format!(" (its base was already `{default_branch}` — GitHub retargeted it when the parent's branch was deleted)")
    };
    let marker =
        format!("{RESTACKED_MARKER_PREFIX} parent={parent} old={old_head} new={new_head} -->");
    format!(
        "{marker}\n\
         ## Stacked parent merged — replayed onto `{default_branch}`\n\n\
         #{parent} (`{parent_branch}`) merged while this PR was stacked on it. Loom replayed \
         only this branch's own commits onto `{default_branch}` ({target_commit}){base_clause}, \
         and pushed the result with a lease pinned to `{old_head}`.\n\n\
         - Old head: `{old_head}`\n\
         - New head: `{new_head}`\n\n\
         The parent's pre-merge tip is preserved at `{pin}` for `reconcile-stack.sh`.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#14903)*",
        pin = pin_ref(parent_branch),
    )
}

/// The merge-mode (base-only) comment.
#[must_use]
pub fn retargeted_comment_body(parent: u32, parent_branch: &str, default_branch: &str) -> String {
    format!(
        "{RETARGETED_MARKER_PREFIX} parent={parent} -->\n\
         ## Stacked parent merged — base moved to `{default_branch}`\n\n\
         #{parent} (`{parent_branch}`) merged while this PR was stacked on it. This repository \
         does not rebase PR branches (#10771), so Loom changed only this PR's base. The branch \
         still carries #{parent}'s commits, whose content is already on `{default_branch}` \
         through the merge; if the PR shows duplicated changes, move it by hand per this \
         repository's branch-update policy.\n\n\
         ---\n\
         *Automated by loom-daemon claim reconciliation (#14903)*"
    )
}

/// The deferral comment body. Same heading and hand command as the shell's
/// byte-frozen `defer_comment` — that overlap is what
/// [`defer_already_posted`] dedupes on.
#[must_use]
pub fn defer_comment_body(
    child_issue: &str,
    child_pr: u32,
    parent_pr: u32,
    parent_branch: &str,
) -> String {
    format!(
        "{}\n\
         ## Stacked parent merged — reconciliation deferred\n\n\
         Parent branch `{parent_branch}` merged, but this child's issue #{child_issue} is still \
         `loom:building` — a Builder likely has this branch checked out. Auto-reconciliation \
         was **skipped** to avoid racing that in-progress work with an out-of-band `git rebase \
         --onto` + `push --force-with-lease`.\n\n\
         **What happens next**: once issue #{child_issue} is no longer `loom:building` (or its \
         lease has gone stale), a later reconciliation pass will reconcile this PR \
         automatically. You can also reconcile it by hand now (from a clean checkout, only \
         once the Builder has finished):\n\n\
         ```\n\
         ./.loom/scripts/reconcile-stack.sh {child_pr} {parent_branch}\n\
         ```\n\n\
         ---\n\
         *Deferred by loom-daemon claim reconciliation (#14903)*",
        deferred_marker(parent_pr, parent_branch),
    )
}

/// The forge operations this pass needs, injectable for tests.
pub trait MergedForge {
    /// The newest closed PR whose head is `branch`. `Ok(None)` = none.
    ///
    /// # Errors
    /// The lookup could not be answered.
    fn newest_closed_pr_for_head(&self, branch: &str) -> Result<Option<orphan::ParentPr>, String>;
    /// Trusted comment bodies on PR `pr`, oldest first. `None` = unreadable.
    fn trusted_comments(&self, pr: u32) -> Option<Vec<String>>;
    /// The child issue's labels, one per line; empty when unreadable (the
    /// stacked-reconcile disposition: force-with-lease is the protection).
    fn issue_labels(&self, issue: u32) -> String;
    /// The child issue's live lease state.
    fn lease(&self, issue: u32) -> LeaseState;
    /// The repository's branch-update strategy.
    fn strategy(&self) -> Strategy;
    /// This repository's `owner/name`. `None` = could not tell.
    fn repository(&self) -> Option<String>;
    /// Change PR `pr`'s base to `base`.
    ///
    /// # Errors
    /// The forge refused or could not be reached.
    fn retarget(&self, pr: u32, base: &str) -> Result<(), String>;
    /// Post `body` on PR `pr`.
    ///
    /// # Errors
    /// The forge refused or could not be reached.
    fn comment(&self, pr: u32, body: &str) -> Result<(), String>;
}

/// Per-pass counts, for the log.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MergedStats {
    /// Children matched to a merged parent this pass.
    pub children: usize,
    /// Children whose branch was replayed and published.
    pub restacked: usize,
    /// Children whose base was moved (a merge-mode retarget counts here too).
    pub retargeted: usize,
    /// Stack levels above a restacked child that were replayed.
    pub cascaded: usize,
    /// Children deferred to a later pass with one comment.
    pub deferred: usize,
    /// Children flagged with one comment and no rewrite.
    pub flagged: usize,
    /// Children left for a later tick (a gate, an unknown, a transient failure).
    pub skipped: usize,
}

/// One pass over `open` for the repository checked out at `repo`. `records`
/// are the merged-parent identities from the merge-observation store,
/// newest first (see [`parents_from_records`]).
pub fn reconcile_with(
    forge: &impl MergedForge,
    repo: &Path,
    default_branch: &str,
    open: &[RestPull],
    records: &[MergedParent],
) -> MergedStats {
    let mut stats = MergedStats::default();
    let open_heads: BTreeSet<&str> = open
        .iter()
        .filter(|p| p.state == "open")
        .filter_map(|p| p.head_ref.as_deref())
        .collect();
    // The default-branch tip, fetched at most once per pass and only when a
    // child is actually being restacked.
    let mut target: Option<Result<String, String>> = None;
    // This repository's `owner/name`, read at most once per pass.
    let mut repo_identity: Option<Option<String>> = None;
    // Children already reconciled against one record this pass (a deeper
    // merged ancestor must not trigger a second replay).
    let mut settled: BTreeSet<u32> = BTreeSet::new();

    // --- shape A: children still based on the merged parent's branch -------
    for child in open
        .iter()
        .filter(|p| orphan::is_candidate(p, default_branch, &open_heads))
    {
        if let Some(skip) = orphan::gate(child) {
            log::debug!(
                "claim_reconciliation (merged stack): PR #{} (a stacked child) skipped: {skip:?}",
                child.number
            );
            continue;
        }
        if orphan::head_owner(child, default_branch, None) == orphan::HeadOwner::DefaultBranch {
            continue;
        }
        let base = child.base_ref.clone().unwrap_or_default();
        let parent = match forge.newest_closed_pr_for_head(&base) {
            Ok(p) => p,
            Err(e) => {
                log::debug!(
                    "claim_reconciliation (merged stack): PR #{}: parent lookup for '{base}' \
                     failed: {e}",
                    child.number
                );
                continue;
            }
        };
        let orphan::ParentState::Merged = orphan::parent_state(&open_heads, &base, parent.as_ref())
        else {
            continue;
        };
        let Some(parent_pr) = parent else {
            continue;
        };
        let parent = MergedParent {
            pr: parent_pr.number,
            branch: base.clone(),
            head: parent_pr.head_sha.clone().unwrap_or_default(),
            merge_commit: String::new(),
            merged_at: String::new(),
        };
        process_child(
            forge,
            repo,
            default_branch,
            open,
            child,
            &parent,
            true,
            &mut target,
            &mut repo_identity,
            &mut stats,
        );
    }

    // --- shape B: children GitHub already retargeted onto the default branch
    // Records newest-first; the first whose landed head the child carries and
    // whose merge commit it lacks is the nearest merged parent. (The
    // `settled` check lives in the body, not a filter closure, so the set can
    // be updated inside the loop.)
    let shape_b: Vec<&RestPull> = open
        .iter()
        .filter(|p| {
            p.state == "open"
                && p.base_ref.as_deref() == Some(default_branch)
                && orphan::gate(p).is_none()
                && p.head_ref
                    .as_deref()
                    .is_some_and(is_stackable_parent_branch)
        })
        .collect();
    // Merge mode can only retarget a base, and a shape-B child's base is
    // already the default branch: nothing it may do, so no per-tick reads.
    let merge_mode = forge.strategy() != Strategy::Rebase;
    if merge_mode || records.is_empty() {
        return stats;
    }
    for child in shape_b {
        if settled.contains(&child.number) {
            continue;
        }
        let head = child.head_sha.as_deref().unwrap_or_default();
        if !crate::merge_pr::sequence::is_full_sha(head) {
            continue;
        }
        if !matches!(git::ensure_pr_head(repo, child.number, head), git::HeadFetch::Present) {
            continue;
        }
        let Some(onto) = fetched_target(&mut target, repo, default_branch) else {
            continue;
        };
        // Three local git calls per child bound the records worth a closer
        // look, with zero per-record spawns (#15209). A merged parent's merge
        // commit is on the default branch but not in the child (`behind`);
        // and the parent's landed head the child carries is either one of
        // the child's own commits (`ahead` — a squash-merged parent, whose
        // head never reached the default branch) or one of the best common
        // ancestors (`bases` — a `--no-ff`-merged parent, whose head the
        // merge commit put ON the default branch). Membership positively
        // proves ancestry, so the match stays evidenced; absence of any
        // answer — a git error — leaves no record matched, per the pass's
        // fail direction.
        let Ok(behind) = git::rev_list(repo, &format!("{head}..{onto}")) else {
            continue;
        };
        let Ok(ahead) = git::rev_list(repo, &format!("{onto}..{head}")) else {
            continue;
        };
        let Ok(bases) = git::merge_bases(repo, head, &onto) else {
            continue;
        };
        for rec in records {
            if !behind.contains(&rec.merge_commit) {
                continue;
            }
            // `--all` best common ancestors, never the single answer: a
            // child that merged an older default branch into itself (the
            // #13593 Doctor shape) has several, and git's single answer
            // picks among them by commit date — it can name the
            // default-branch side and miss the parent.
            if !(ahead.contains(&rec.head) || bases.contains(&rec.head)) {
                continue;
            }
            process_child(
                forge,
                repo,
                default_branch,
                open,
                child,
                rec,
                false,
                &mut target,
                &mut repo_identity,
                &mut stats,
            );
            settled.insert(child.number);
            break;
        }
    }
    stats
}

/// The memoized default-branch tip fetch shared by discovery and the restack.
fn fetched_target(
    target: &mut Option<Result<String, String>>,
    repo: &Path,
    default_branch: &str,
) -> Option<String> {
    target
        .get_or_insert_with(|| {
            crate::reconcile_stack::fetch_and_pin_target(repo, git::REMOTE, default_branch)
                .map(|(sha, _)| sha)
                .map_err(|e| e.to_string())
        })
        .clone()
        .ok()
}

/// Whether a restack may run here at all: only Loom-owned heads in this
/// repository are rewritten. Cached per pass.
fn loom_owned(
    forge: &impl MergedForge,
    child: &RestPull,
    default_branch: &str,
    repo_identity: &mut Option<Option<String>>,
) -> Option<bool> {
    let this_repo = repo_identity
        .get_or_insert_with(|| forge.repository())
        .clone();
    match orphan::head_owner(child, default_branch, this_repo.as_deref()) {
        orphan::HeadOwner::Loom => Some(true),
        orphan::HeadOwner::Foreign => Some(false),
        orphan::HeadOwner::Unknown | orphan::HeadOwner::DefaultBranch => None,
    }
}

#[allow(clippy::too_many_arguments)]
fn process_child(
    forge: &impl MergedForge,
    repo: &Path,
    default_branch: &str,
    open: &[RestPull],
    child: &RestPull,
    parent: &MergedParent,
    retarget_needed: bool,
    target: &mut Option<Result<String, String>>,
    repo_identity: &mut Option<Option<String>>,
    stats: &mut MergedStats,
) {
    stats.children += 1;
    let head = child.head_sha.as_deref().unwrap_or_default();
    let branch = child.head_ref.as_deref().unwrap_or_default();
    let Some(bodies) = forge.trusted_comments(child.number) else {
        stats.skipped += 1;
        return;
    };
    if already_flagged(&bodies, parent.pr, head) {
        return;
    }

    // Merge mode (#10771): never rewrite a pushed branch. The parent's content
    // already reached the default branch through the merge, so a base-only
    // retarget is safe and is all the policy allows — it races no Builder and
    // needs no replay evidence, so it runs before the rebase-only deferral and
    // parent-tip checks below.
    if forge.strategy() != Strategy::Rebase {
        if !retarget_needed {
            return;
        }
        if let Err(e) = forge.retarget(child.number, default_branch) {
            log::warn!(
                "claim_reconciliation (merged stack): PR #{}: merge-mode retarget to \
                 '{default_branch}' failed (retrying next tick): {e}",
                child.number
            );
            stats.skipped += 1;
            return;
        }
        stats.retargeted += 1;
        log::info!(
            "claim_reconciliation (merged stack): PR #{} was stacked on merged #{}; \
             merge-mode repository, retargeted the base only (#14903)",
            child.number,
            parent.pr
        );
        // Best effort: keep the operator's `reconcile-stack.sh` fallback pin
        // when the recorded head is positively fetchable.
        if crate::merge_pr::sequence::is_full_sha(&parent.head)
            && matches!(git::ensure_pr_head(repo, parent.pr, &parent.head), git::HeadFetch::Present)
        {
            if let Err(e) = try_establish_pin(repo, &parent.branch, &parent.head) {
                log::debug!(
                    "claim_reconciliation (merged stack): PR #{}: pin {} not refreshed: {e}",
                    child.number,
                    pin_ref(&parent.branch)
                );
            }
        }
        if let Err(e) = forge.comment(
            child.number,
            &retargeted_comment_body(parent.pr, &parent.branch, default_branch),
        ) {
            log::warn!(
                "claim_reconciliation (merged stack): PR #{}: merge-mode comment failed: {e}",
                child.number
            );
        }
        return;
    }

    // The stacked-reconcile decision, verbatim: `loom:building` defers, a
    // lease-free building issue reconciles, an unreadable lease still defers.
    let issue = issue_from_branch(branch);
    let mut decision = child_route(issue.as_deref(), forge_labels(forge, &issue).as_bytes());
    if decision.route == Route::Defer {
        if let Some(n) = issue.as_deref().and_then(|i| i.parse::<u32>().ok()) {
            decision = route_with_lease(decision, &forge.lease(n));
        }
    }
    if decision.route == Route::Defer {
        if defer_already_posted(&bodies, &parent.branch, child.number) {
            return;
        }
        let body = defer_comment_body(
            issue.as_deref().unwrap_or("?"),
            child.number,
            parent.pr,
            &parent.branch,
        );
        if let Err(e) = forge.comment(child.number, &body) {
            log::warn!(
                "claim_reconciliation (merged stack): PR #{}: deferral comment failed: {e}",
                child.number
            );
            stats.skipped += 1;
            return;
        }
        stats.deferred += 1;
        log::info!(
            "claim_reconciliation (merged stack): PR #{} is stacked on merged #{} but its issue \
             is loom:building with a live lease — deferred (#14903)",
            child.number,
            parent.pr
        );
        return;
    }

    // Parent-tip evidence: the recorded head, positively fetchable through
    // the PR ref long after the branch disappeared.
    if !crate::merge_pr::sequence::is_full_sha(&parent.head) {
        return flag(
            forge,
            &flag_ctx(child, parent, default_branch, "<the-parent-tip>"),
            &FlagReason::Refused(format!(
                "the merged parent #{}'s PR record names no head commit",
                parent.pr
            )),
            stats,
        );
    }
    match git::ensure_pr_head(repo, parent.pr, &parent.head) {
        git::HeadFetch::Present => {}
        git::HeadFetch::FetchFailed(e) => {
            log::info!(
                "claim_reconciliation (merged stack): PR #{}: merged parent #{}'s head did not \
                 fetch (retrying next tick): {e}",
                child.number,
                parent.pr
            );
            stats.skipped += 1;
            return;
        }
        git::HeadFetch::Missing => {
            return flag(
                forge,
                &flag_ctx(child, parent, default_branch, &parent.head),
                &FlagReason::Refused(format!(
                    "`refs/pull/{}/head` fetched, but the merged parent #{}'s recorded head {} \
                     is not in it",
                    parent.pr, parent.pr, parent.head
                )),
                stats,
            );
        }
    }
    // Preserve or positively recover the pin: `reconcile-stack.sh`'s fallback
    // boundary. A disagreeing pin loses to the recorded head — best effort,
    // never a gate (a failed write costs the operator one update-ref).
    if let Err(e) = try_establish_pin(repo, &parent.branch, &parent.head) {
        log::debug!(
            "claim_reconciliation (merged stack): PR #{}: pin {} could not be refreshed for \
             merged #{} ({e}) — the recorded head remains the replay boundary",
            child.number,
            pin_ref(&parent.branch),
            parent.pr
        );
    }

    match loom_owned(forge, child, default_branch, repo_identity) {
        Some(true) => {}
        Some(false) => {
            // Flagged once per head, as the orphan pass does, so later ticks
            // spend no parent lookup or fetch on it.
            return flag(
                forge,
                &flag_ctx(child, parent, default_branch, &parent.head),
                &FlagReason::NotLoomBranch,
                stats,
            );
        }
        None => {
            log::info!(
                "claim_reconciliation (merged stack): PR #{}: this repository's identity is \
                 unreadable, so its head's owner is unknown — left for a later tick",
                child.number
            );
            stats.skipped += 1;
            return;
        }
    }

    let onto = match fetched_target(target, repo, default_branch) {
        Some(sha) => sha,
        None => {
            let e = target
                .as_ref()
                .and_then(|r| r.as_ref().err().cloned())
                .unwrap_or_else(|| "the fetch gave no answer".to_string());
            log::warn!(
                "claim_reconciliation (merged stack): {}: could not fetch '{default_branch}' — \
                 no child restacked this pass: {e}",
                repo.display()
            );
            stats.skipped += 1;
            return;
        }
    };

    // A branch pushed by an earlier pass whose retarget then failed is
    // already replayed: only the retarget is retried (#12769 shape), and the
    // cascade must not re-run against an already-published head.
    let (old_head, outcome, replayed_now) = match pushed_earlier(&bodies, parent.pr, head) {
        Some(old) => (old, git::ReplayOutcome::Pushed(head.to_string()), false),
        None => (
            head.to_string(),
            git::guarded_replay(
                repo,
                &git::ReplayRequest {
                    pr: child.number,
                    branch,
                    head,
                    onto: &onto,
                    upstream: &parent.head,
                    scratch: &scratch_path(repo, child.number),
                    pass: "merged stack",
                },
            ),
            true,
        ),
    };
    let head = old_head.as_str();
    match outcome {
        git::ReplayOutcome::Pushed(new_head) => {
            if replayed_now {
                stats.restacked += 1;
                // The grandchildren follow the push even when the retarget
                // below fails — the branch is already published.
                let this_repo = repo_identity.as_ref().and_then(Option::as_deref);
                cascade_children(
                    forge,
                    repo,
                    open,
                    this_repo,
                    default_branch,
                    child,
                    head,
                    &new_head,
                    stats,
                );
            }
            if retarget_needed {
                if let Err(e) = forge.retarget(child.number, default_branch) {
                    log::warn!(
                        "claim_reconciliation (merged stack): PR #{} restacked onto {onto} but \
                         the retarget to '{default_branch}' failed (retrying next tick): {e}",
                        child.number
                    );
                    if pushed_earlier(&bodies, parent.pr, &new_head).is_none() {
                        let note = format!(
                            "{}\nLoom replayed this PR's own commits onto `{default_branch}` and \
                             pushed `{new_head}`, but could not change its base afterwards. It \
                             retries the base change on its next pass.",
                            pushed_marker(parent.pr, head, &new_head)
                        );
                        if let Err(e) = forge.comment(child.number, &note) {
                            log::warn!(
                                "claim_reconciliation (merged stack): PR #{}: pushed-marker \
                                 comment failed: {e}",
                                child.number
                            );
                        }
                    }
                    stats.skipped += 1;
                    return;
                }
                stats.retargeted += 1;
            }
            log::info!(
                "claim_reconciliation (merged stack): PR #{} was stacked on merged #{} \
                 ({parent_branch}); replayed its own commits onto {onto}, pushed {new_head} \
                 leased to {head}{retarget_note} (#14903)",
                child.number,
                parent.pr,
                parent_branch = parent.branch,
                retarget_note = if retarget_needed {
                    format!(", retargeted to '{default_branch}'")
                } else {
                    String::new()
                },
            );
            if let Err(e) = forge.comment(
                child.number,
                &restacked_comment_body(
                    parent.pr,
                    &parent.branch,
                    head,
                    &new_head,
                    default_branch,
                    &onto,
                    retarget_needed,
                ),
            ) {
                log::warn!(
                    "claim_reconciliation (merged stack): PR #{}: report comment failed: {e}",
                    child.number
                );
            }
        }
        git::ReplayOutcome::Refused(r) => flag(
            forge,
            &flag_ctx(child, parent, default_branch, &parent.head),
            &FlagReason::from(stack_reconcile::CascadeFlag::from(r)),
            stats,
        ),
        git::ReplayOutcome::Transient(e) => {
            log::info!(
                "claim_reconciliation (merged stack): PR #{} not restacked this tick: {e}",
                child.number
            );
            stats.skipped += 1;
        }
    }
}

fn forge_labels(forge: &impl MergedForge, issue: &Option<String>) -> String {
    issue
        .as_deref()
        .and_then(|n| n.parse::<u32>().ok())
        .map(|n| forge.issue_labels(n))
        .unwrap_or_default()
}

fn flag_ctx<'a>(
    child: &'a RestPull,
    parent: &'a MergedParent,
    default_branch: &'a str,
    upstream: &'a str,
) -> FlagContext<'a> {
    FlagContext {
        pr: child.number,
        branch: child.head_ref.as_deref().unwrap_or_default(),
        head: child.head_sha.as_deref().unwrap_or_default(),
        parent_pr: parent.pr,
        parent_branch: &parent.branch,
        upstream,
        default_branch,
        retarget_to: Some(default_branch),
        cascade_onto: None,
    }
}

fn scratch_path(repo: &Path, pr: u32) -> PathBuf {
    repo.join(format!(".loom/worktrees/merged-restack-pr-{pr}"))
}

#[allow(clippy::too_many_arguments)]
fn cascade_children(
    forge: &impl MergedForge,
    repo: &Path,
    open: &[RestPull],
    this_repo: Option<&str>,
    default_branch: &str,
    child: &RestPull,
    old_head: &str,
    new_head: &str,
    stats: &mut MergedStats,
) {
    let counts = stack_reconcile::cascade(
        child,
        old_head,
        new_head,
        &mut stack_reconcile::Cascade {
            repo,
            open,
            this_repo,
            default_branch,
            pass: "merged stack",
            issue_tag: "#14903",
            scratch_path,
            issue_lease_blocks: &|i| {
                matches!(forge.lease(i), LeaseState::Held | LeaseState::Unknown)
            },
            on_cascaded: &mut |s, g_new| {
                let body = cascaded_comment_body(
                    s.parent_pr,
                    s.parent_branch,
                    s.head,
                    g_new,
                    s.new_parent_head,
                );
                if let Err(e) = forge.comment(s.pr, &body) {
                    log::warn!(
                        "claim_reconciliation (merged stack): PR #{}: cascade comment failed: {e}",
                        s.pr
                    );
                }
            },
            on_flag: &mut |s, r| {
                let ctx = FlagContext {
                    pr: s.pr,
                    branch: s.branch,
                    head: s.head,
                    parent_pr: s.parent_pr,
                    parent_branch: s.parent_branch,
                    upstream: s.old_parent_head,
                    default_branch,
                    retarget_to: None,
                    cascade_onto: Some(s.new_parent_head),
                };
                flag(forge, &ctx, &r.into(), stats);
            },
        },
    );
    stats.cascaded += counts.cascaded;
    stats.skipped += counts.skipped;
}

/// Comment first (so a failed post is retried next tick: no marker yet), and
/// count only a comment that landed.
fn flag(
    forge: &impl MergedForge,
    ctx: &FlagContext<'_>,
    reason: &FlagReason,
    stats: &mut MergedStats,
) {
    if let Err(e) = forge.comment(ctx.pr, &flag_comment_body(ctx, reason)) {
        log::warn!("claim_reconciliation (merged stack): PR #{}: flag comment failed: {e}", ctx.pr);
        return;
    }
    stats.flagged += 1;
    log::info!(
        "claim_reconciliation (merged stack): flagged PR #{} — {reason:?} (#14903)",
        ctx.pr
    );
}

/// The live forge, through the claim-reconciliation `gh` facade.
pub struct GhMergedForge<'a> {
    pub gh_bin: &'a Path,
    pub root: &'a Path,
}

impl GhMergedForge<'_> {
    fn api_write(&self, op: &'static str, args: &[&str]) -> Result<(), String> {
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
}

impl MergedForge for GhMergedForge<'_> {
    fn newest_closed_pr_for_head(&self, branch: &str) -> Result<Option<orphan::ParentPr>, String> {
        let path = format!(
            "repos/{{owner}}/{{repo}}/pulls?head={{owner}}:{branch}&state=closed&per_page=100"
        );
        let out = super::gh_call::output(
            super::gh_call::read("merged_stack.parent_lookup", self.gh_bin, self.root)
                .args(["api", &path]),
        )
        .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err(super::gh_call::stderr(&out));
        }
        orphan::parse_newest_closed(&out.stdout, branch)
    }

    fn trusted_comments(&self, pr: u32) -> Option<Vec<String>> {
        crate::merge_pr::sequence::fetch_trusted_bodies_as(
            "merged_stack.comments",
            &self.gh_bin.to_string_lossy(),
            self.root,
            "{owner}/{repo}",
            pr,
        )
    }

    fn issue_labels(&self, issue: u32) -> String {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}");
        let Ok(out) = super::gh_call::output(
            super::gh_call::read("merged_stack.issue_labels", self.gh_bin, self.root)
                .args(["api", &path]),
        ) else {
            return String::new();
        };
        if !out.status.success() {
            return String::new();
        }
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else {
            return String::new();
        };
        v.get("labels")
            .and_then(|l| l.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name").and_then(|n| n.as_str()))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    }

    fn lease(&self, issue: u32) -> LeaseState {
        use super::forge::LeaseProbe;
        match super::forge::fetch_freshest_lease_updated_at(self.gh_bin, self.root, issue) {
            LeaseProbe::Found(at) => {
                if super::lease_is_fresh(at, chrono::Utc::now(), super::resolve_lease_ttl_minutes())
                {
                    LeaseState::Held
                } else {
                    LeaseState::NoneFresh
                }
            }
            LeaseProbe::NotFound => LeaseState::NoneFresh,
            LeaseProbe::ReadFailed => LeaseState::Unknown,
        }
    }

    fn strategy(&self) -> Strategy {
        let nwo = std::env::var("LOOM_REPO").ok();
        crate::branch_update::resolve(Some(self.root), nwo.as_deref(), true).strategy
    }

    fn repository(&self) -> Option<String> {
        crate::worktree_ops::gh::resolve_owner_repo(self.root).map(|(o, r)| format!("{o}/{r}"))
    }

    fn retarget(&self, pr: u32, base: &str) -> Result<(), String> {
        let path = format!("repos/{{owner}}/{{repo}}/pulls/{pr}");
        let field = format!("base={base}");
        self.api_write("merged_stack.retarget", &["-X", "PATCH", &path, "-f", &field])
    }

    fn comment(&self, pr: u32, body: &str) -> Result<(), String> {
        let path = format!("repos/{{owner}}/{{repo}}/issues/{pr}/comments");
        let field = format!("body={body}");
        self.api_write("merged_stack.comment", &["-X", "POST", &path, "-f", &field])
    }
}

/// The daemon entry point: one pass for `root`, on the shared open-PR listing
/// when the preceding pass handed one on.
pub(super) fn reconcile(
    gh_bin: &Path,
    root: &Path,
    prefetched: Option<&[RestPull]>,
) -> MergedStats {
    if !merged_stack_enabled() {
        return MergedStats::default();
    }
    // One host per workspace writes (#10634's shard, as the orphan pass
    // uses): two daemons racing the same child would both read, and the
    // loser's lease would fail, but both would comment.
    if !crate::role_shard::decide(root).admits_role_tick() {
        return MergedStats::default();
    }
    let Some(default_branch) = crate::worktree_cli::default_branch::resolve(root) else {
        log::debug!(
            "claim_reconciliation (merged stack): {}: default branch unknown — pass skipped",
            root.display()
        );
        return MergedStats::default();
    };
    let listed;
    let open = match prefetched {
        Some(rows) => rows,
        None => match super::open_pr_listing::list_open_prs(gh_bin, root) {
            Ok(rows) => {
                listed = rows;
                &listed[..]
            }
            Err(e) => {
                log::warn!("claim_reconciliation (merged stack): {}: {e}", root.display());
                return MergedStats::default();
            }
        },
    };
    let store = super::pass_loop::merge_observation::RecordStore::for_root(root);
    let records: Vec<merge_observation_record::Record> = store
        .all()
        .into_iter()
        .map(|r| merge_observation_record::Record {
            pr: r.pr,
            branch: r.branch,
            head: r.head,
            merge_commit: r.merge_commit,
            merged_at: r.merged_at,
        })
        .collect();
    let parents = parents_from_records(&records);
    let stats =
        reconcile_with(&GhMergedForge { gh_bin, root }, root, &default_branch, open, &parents);
    if stats != MergedStats::default() {
        log::info!(
            "claim_reconciliation (merged stack): {}: {} child(ren) of merged parents, {} \
             restacked, {} retargeted, {} cascaded, {} deferred, {} flagged, {} skipped (#14903)",
            root.display(),
            stats.children,
            stats.restacked,
            stats.retargeted,
            stats.cascaded,
            stats.deferred,
            stats.flagged,
            stats.skipped
        );
    }
    stats
}

#[cfg(test)]
#[path = "merged_stack_tests.rs"]
mod tests;
