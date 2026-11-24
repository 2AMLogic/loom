//! Observe confirmed merges and finish what they skipped (#14902, child of
//! #14900): the daemon-side counterpart of `merge-pr.sh`'s three post-merge
//! issue-bookkeeping passes, Champion's Step 4, and — since #14904 — the
//! script's post-merge worktree cleanup.
//!
//! # The gap this closes
//!
//! A merge made directly on the forge (GitHub's own merge button, a
//! queue-merged PR, a human) bypasses every bookkeeping caller that exists
//! today: `merge-pr.sh`'s `_reset_partial_increment_labels`,
//! `_close_branch_derived_issue` and `_strip_closed_issue_building_labels`
//! never run, and Champion's "Step 4: Verify Issue Auto-Close" — the
//! out-of-band acceptance-criteria gate (#6883), the negated-reference reopen
//! (#1057/#8942) and the belt-and-suspenders close — never runs either. The
//! partial-increment issue keeps its `loom:building` claim until a
//! ≥2h-stale sweep reclaims it, and a closed issue keeps stale queue labels.
//!
//! This pass runs inside the per-workspace reconciliation tick
//! ([`super::run_reconciliation_pass_over_roots`]), which already visits
//! every registered root on a cadence independent of who dispatched what —
//! so a merge is observed no matter who made it. Unlike the merge-queue hook
//! one call above it in the tick, it makes **no merge-mode check**: it runs
//! on queue-mode and direct-mode workspaces alike (#14902's guidance — the
//! queue-only hook returns before forge calls in direct mode, so discovery
//! must not).
//!
//! # Recorded identity (#14902)
//!
//! The first observation of a merged PR pins a [`MergeRecord`] under
//! `.loom/state/merge-bookkeeping/pr-<N>.json`: repository, PR, landed head
//! (`head.sha`), merge commit (`merge_commit_sha`), merged-at, branch and
//! the PR body, plus which of the passes have completed. Subsequent
//! actions and retries read the record, never the listing again — a retry
//! refers to the same merge identity even after the PR has fallen out of the
//! discovery window. A partial failure (a pass that could not establish its
//! facts) leaves its pass unmarked and is retried on a later tick; a
//! completed record is never revisited.
//!
//! # Ordering and hold semantics (AC: preserved from the shell/Champion)
//!
//! The three shell passes keep their load-bearing order — partial-reset (a
//! #4569 reopen must precede the strip reading the issue), branch-close
//! (must precede the strip so the strip sees the issue it closed), strip.
//! Champion's close check sits between the reset and the branch close: in
//! the retired flow Champion ran it after `merge-pr.sh` finished, and here
//! running it before the strip means an issue this pass closes gets its
//! stale labels stripped in the same tick — the end state one shell run
//! plus one Champion run already converge to.
//!
//! The `loom:remainder-hold` semantics (#9264) are preserved by reusing
//! [`partial_reset::plan`] and [`post_merge_bookkeeping::apply_partial_steps`]
//! untouched; the mutations themselves go through the same
//! `lib/forge-helpers.sh` writers the shell calls (`forge_gh_*_rl_safe`),
//! so the #9548 write-scope vet, the #6074 permission ladder and the #4856
//! REST fallback stay in exactly one place.
//!
//! # What a direct merge cannot supply, and what this pass does about it
//!
//! The shell's partial-reset pass consumes two PRE-merge facts
//! (`$PARTIAL_CONFLICT_ISSUES`, `$PARTIAL_OPEN_BEFORE_MERGE`) that a merge
//! made outside the script nobody recorded. This pass re-derives them
//! post-merge, strictly gated:
//!
//! - **Was the issue open right before the merge, and closed by it?**
//!   [`closed_by_merge::decide`] answers from the issue's close events and
//!   `mergedAt`: `ThisMerge` implies the issue was open immediately before
//!   the merge (it cannot be closed by the merge otherwise). Any other
//!   verdict — closed before the merge (an unrelated historical close),
//!   another PR's close, a direct close, unattributable, or `Unanswered`
//!   (the fact could not be established) — leaves the facts unset and the
//!   dependent reopen does not happen.
//! - **Did this PR carry a closing reference to it?** the same three signals
//!   the shell's pre-merge guard unioned: the body's own keywords
//!   ([`closing_refs`]), the commit messages (the squash message's source),
//!   and `closingIssuesReferences` (the Development sidebar).
//!
//! A **newer Builder claim survives replay**: observation is asynchronous,
//! so unlike the shell's synchronous pass this one must not reset a
//! `loom:building` claim that was acquired AFTER the merge. Before swapping
//! an open, still-building issue, the pass reads the issue's label timeline
//! and requires the newest `loom:building` `labeled` event to predate the
//! merge ([`claim_predates_merge`]); an unreadable timeline fails the same
//! way — the swap is skipped, never guessed.
//!
//! # Convergence during the overlap release
//!
//! The shell and Champion callers keep running (retirement is a later child
//! of #14900). Every pass here re-reads the issue it acts on, and the two
//! audit comments the partial-reset posts are deduplicated by content
//! ([`comment_already_posted`]) — a heading plus the PR number for the
//! partial-increment notices, the full marker line for the AC-hold notice —
//! so script-then-tick, tick-then-script and tick-after-tick all converge
//! on the same issue state, labels and one comment each, with no duplicate
//! comments and no lost eligible work.
//!
//! # The worktree/branch cleanup pass (#14904)
//!
//! The four passes above are forge bookkeeping; the fifth is the local half
//! the script's merge also performed: `merge-pr.sh`'s whole post-merge
//! worktree-cleanup block, replayed in-process through
//! [`post_merge_cleanup::run`] — the same orchestrator, the same gates, the
//! same order. A merge made on the forge ran no script anywhere, so the
//! issue worktree, a co-existing Judge `pr-<N>` review worktree (#6264) and
//! a branch-discovered worktree (#3334) would otherwise wait for the
//! periodic reaper's grace period, or forever on a host the reaper's
//! naming filters do not cover.
//!
//! **Eligibility is decided per workspace** ([`CleanupWorld`]): the tick
//! runs once per registered root, the record store is per root, and every
//! gate the orchestrator applies — worktree paths, sentinels, porcelain,
//! dirty state, branch tips — reads THIS checkout. A merge completed in one
//! checkout (or a record already complete in another host's store) is no
//! evidence at all about the contents or ownership of a second checkout, so
//! it neither suppresses nor forces cleanup there.
//!
//! What a forge observation cannot supply stays unsupplied: the
//! `--worktree-path` override and `--remove-ignored` are the shell's
//! CLI-only knobs, and an external merge conveys no explicit user
//! authorization to clean a user-supplied path — so the override path (the
//! only one that can remove an unmanaged directory, #13599) is unreachable
//! here. The workspace's own preserve setting (`LOOM_PRESERVE_WORKTREE=1`)
//! IS passed through. The three `LOOM-NEED` facts the orchestrator asks its
//! caller for arrive exactly as the shell fetched them: the close targets
//! the pass already read, the issue state re-read on demand (empty on a
//! failed read — which the #4186 gate reads as Preserve, never as
//! "closed"), and `branch_has_landed` answered from the #7812 ladder with
//! the record's pinned head as the known-merged-head hint, so a local
//! branch that advanced past the merged head is `not-landed` and its
//! worktree is preserved rather than guessed at.
//!
//! # Fail direction
//!
//! Every pass is best-effort, exactly as in the shell: nothing here can
//! fail a merge (it already happened). Reads that cannot be established
//! skip the dependent mutation and leave the pass unmarked for a retry —
//! the discovery listing, the close targets, or a state that is neither
//! `open` nor `closed` each defer the record wholesale rather than read
//! "no answer" as "no targets".

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::claim_reconciliation::gh_call;
use crate::merge_pr::closed_by_merge;
use crate::merge_pr::partial_comment::now_timestamp;
use crate::merge_pr::partial_reset::{self, IssueView, PreMerge};
use crate::merge_pr::post_merge_bookkeeping::{self as bookkeeping, Host};
use crate::merge_pr::post_merge_cleanup as cleanup;
use crate::merge_pr::refs::{
    closing_ref_negation_status, closing_refs, partial_increment_refs, ClosingRefNegationStatus,
};

/// How many recently-updated closed PRs the discovery listing reads. The
/// record store makes older merges self-sufficient once seen; this window
/// only bounds how far BACK a first observation reaches.
const MERGED_WINDOW: usize = 15;

/// Pass tokens, in run order. The relative order of `partial-reset`,
/// `branch-close` and `strip` is the shell's own and load-bearing;
/// `worktree-cleanup` runs last, as `_post_merge_cleanup` did after the
/// bookkeeping in `merge-pr.sh` (#14904).
const PARTIAL_RESET: &str = "partial-reset";
const CLOSE_CHECK: &str = "close-check";
const BRANCH_CLOSE: &str = "branch-close";
const STRIP: &str = "strip";
const WORKTREE_CLEANUP: &str = "worktree-cleanup";
const PASSES: [&str; 5] = [
    PARTIAL_RESET,
    CLOSE_CHECK,
    BRANCH_CLOSE,
    STRIP,
    WORKTREE_CLEANUP,
];

/// One row of the merged-PR discovery listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedPull {
    pub number: u64,
    /// The landed head (`head.sha`) — the tree an AC marker must name.
    pub head: String,
    /// The merge commit (`merge_commit_sha`).
    pub merge_commit: String,
    pub merged_at: String,
    /// The PR's head branch name (`head.ref`).
    pub branch: String,
    pub body: String,
}

/// The recorded identity of one observed merge (#14902): everything later
/// passes and retries need, pinned at first observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRecord {
    pub repo: String,
    pub pr: u64,
    pub head: String,
    pub merge_commit: String,
    pub merged_at: String,
    pub branch: String,
    /// The merged PR's body. The partial-increment declarations and closing
    /// keywords live here and the body is immutable in practice once merged;
    /// pinning it makes a retry independent of the discovery window.
    pub body: String,
    pub observed_at: String,
    /// Completed pass tokens (subset of [`PASSES`], run order).
    pub done: Vec<String>,
}

impl MergeRecord {
    #[must_use]
    pub fn complete(&self) -> bool {
        PASSES.iter().all(|p| self.done.iter().any(|d| d == p))
    }

    fn mark(&mut self, pass: &str) {
        if !self.done.iter().any(|d| d == pass) {
            self.done.push(pass.to_string());
        }
    }
}

/// Counts for one pass over a workspace.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Stats {
    /// Merged PRs the discovery listing returned.
    pub merges_seen: usize,
    /// Records this run carried to `complete()`.
    pub records_completed: usize,
}

/// Parse the discovery listing (REST `pulls?state=closed…`, `--paginate`
/// concatenated pages) into merged rows. Closed-unmerged rows and rows
/// without a readable head or merge commit are dropped — a row whose
/// identity cannot be pinned is not an observation.
#[must_use]
pub fn parse_merged_pulls(raw: &str) -> Vec<MergedPull> {
    let mut out = Vec::new();
    for page in serde_json::Deserializer::from_str(raw).into_iter::<Value>() {
        let Ok(Value::Array(items)) = page else { break };
        for v in items {
            let row = MergedPull {
                number: v.get("number").and_then(Value::as_u64).unwrap_or_default(),
                head: v
                    .pointer("/head/sha")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                merge_commit: v
                    .get("merge_commit_sha")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                merged_at: v
                    .get("merged_at")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                branch: v
                    .pointer("/head/ref")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                body: v
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            };
            let merged = !row.merged_at.is_empty();
            if merged && row.number > 0 && !row.head.is_empty() && !row.merge_commit.is_empty() {
                out.push(row);
            }
        }
    }
    out
}

/// Whether the newest `loom:building` `labeled` event in `events` (an issue
/// timeline, `--paginate` concatenated pages) predates `merged_at`.
///
/// `Some(false)` = a newer claim: the label was (re)applied after the merge,
/// so the merge's bookkeeping must not reset it. `None` = unknown — no
/// readable event, or a timestamp that does not parse — and the caller
/// skips the dependent mutation rather than guess (#14902).
#[must_use]
pub fn claim_predates_merge(events: &str, merged_at: &str) -> Option<bool> {
    let merged = chrono::DateTime::parse_from_rfc3339(merged_at.trim()).ok()?;
    let mut rows: Vec<Value> = Vec::new();
    for page in serde_json::Deserializer::from_str(events).into_iter::<Value>() {
        let Ok(Value::Array(items)) = page else {
            return None;
        };
        rows.extend(items);
    }
    let mut labeled: Vec<&Value> = rows
        .iter()
        .filter(|e| {
            e.get("event").and_then(Value::as_str) == Some("labeled")
                && e.pointer("/label/name").and_then(Value::as_str) == Some("loom:building")
        })
        .collect();
    labeled.sort_by_key(|e| {
        e.get("created_at")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    });
    let newest = labeled.last()?;
    let at = newest.get("created_at").and_then(Value::as_str)?;
    Some(chrono::DateTime::parse_from_rfc3339(at.trim()).ok()? <= merged)
}

/// Would posting `body` duplicate an audit comment already on the issue?
///
/// Three shapes are idempotence-guarded during the overlap release, each
/// matched the way Champion's own marker grep works (content, not trust —
/// a copied marker can only make the pass skip a comment, never post a
/// second one):
///
/// - the AC-hold notice: its full `<!-- champion:ac-hold pr=P sha=H -->`
///   marker line, byte-for-byte;
/// - `## Partial Increment Merged…` / `## Premature Auto-Close Reverted`:
///   the heading plus the `PR #N` token, so two different PRs merging
///   partial increments of the same issue each still get their comment.
#[must_use]
pub fn comment_already_posted(body: &str, existing: &[String]) -> bool {
    match dedupe_key(body) {
        Some(DedupeKey::Marker(m)) => existing.iter().any(|b| b.contains(&m)),
        Some(DedupeKey::Family(family, pr_tok)) => existing
            .iter()
            .any(|b| b.contains(family) && b.contains(&pr_tok)),
        None => false,
    }
}

/// What identifies a guarded comment across postings.
enum DedupeKey {
    /// The AC-hold notice: its full marker line, byte-for-byte.
    Marker(String),
    /// An audit notice: its heading family plus the `PR #N` token. The
    /// family covers the held variant's longer heading, and matching the
    /// posting body's OWN family (not "any audit comment naming this PR")
    /// is what keeps a premature-close notice from deduping the
    /// partial-increment notice that follows it in the same pass.
    Family(&'static str, String),
}

/// The dedupe key of `body`, if it is a shape the overlap release guards.
fn dedupe_key(body: &str) -> Option<DedupeKey> {
    if let Some(marker) = body
        .lines()
        .find(|l| l.trim_start().starts_with("<!-- champion:ac-hold "))
        .map(str::trim)
    {
        return Some(DedupeKey::Marker(marker.to_string()));
    }
    let heading = body.lines().next()?;
    let family = if heading.starts_with("## Partial Increment Merged") {
        "## Partial Increment Merged"
    } else if heading == "## Premature Auto-Close Reverted" {
        "## Premature Auto-Close Reverted"
    } else {
        return None;
    };
    Some(DedupeKey::Family(family, pr_token(body)?))
}

/// The first `PR #<digits>` token in `body`, as written.
fn pr_token(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let mut i = 0;
    while let Some(at) = bytes[i..].iter().position(|b| *b == b'P') {
        let start = i + at;
        let rest = &body[start..];
        if let Some(rest) = rest.strip_prefix("PR #") {
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                return Some(format!("PR #{digits}"));
            }
        }
        i = start + 1;
    }
    None
}

/// `champion.outOfBandAcGate` (#12703): only an explicit boolean `false`
/// turns the gate off; absent, unreadable, or any other value keeps it on
/// (fail closed). The pure half of [`ac_gate_enabled`].
#[must_use]
pub fn ac_gate_enabled_from(config: &Value) -> bool {
    get_path(config, "champion.outOfBandAcGate") != Some(&Value::Bool(false))
}

fn get_path<'a>(config: &'a Value, dotted: &str) -> Option<&'a Value> {
    let mut node = config;
    for part in dotted.split('.') {
        node = node.get(part)?;
    }
    Some(node)
}

/// `.body // ""` over the issue read, jq-style: the first document's string
/// body, `""` for null/absent.
fn issue_body_from_json(json: &str) -> String {
    serde_json::Deserializer::from_str(json)
        .into_iter::<Value>()
        .map_while(Result::ok)
        .find_map(|d| d.get("body").and_then(Value::as_str).map(str::to_string))
        .unwrap_or_default()
}

/// The forge and script touches the pass needs. Every method is `&self`:
/// implementations that mutate (caches, test fakes) hold their own interior
/// mutability, so the [`bookkeeping::Host`] adapter below can delegate
/// without borrowing gymnastics.
pub trait Forge {
    /// Recently merged PRs of the root's repository, newest first; `None`
    /// when the listing itself could not be read (nothing observed).
    fn merged_pulls(&self) -> Option<Vec<MergedPull>>;
    /// `owner/repo` of the root's repository.
    fn nwo(&self) -> Option<String>;
    /// `gh api repos/<nwo>/issues/<n>` with the shell's `{}` fallback on a
    /// failed read.
    fn read_issue(&self, repo: &str, issue: &str) -> String;
    /// Every commit message of the PR, concatenated; empty on failure.
    fn commit_messages(&self, repo: &str, pr: &str) -> String;
    /// `pr` / `open` / `closed` / empty for the branch-derived close.
    fn issue_kind(&self, repo: &str, issue: &str) -> String;
    /// `forge_pr_close_targets`'s output: one issue number per line, sorted.
    /// `None` when the read failed — the caller defers, it never reads that
    /// as "no targets".
    fn close_targets(&self, repo: &str, pr: &str) -> Option<String>;
    /// Every comment body on the issue, any author (the dedupe corpus).
    fn issue_comments(&self, repo: &str, issue: &str) -> Vec<String>;
    /// The issue's timeline JSON (label events); `None` when unreadable.
    fn label_events(&self, repo: &str, issue: &str) -> Option<String>;
    /// The merge commit's message; empty on failure.
    fn merge_commit_message(&self, repo: &str, sha: &str) -> String;
    /// The `closed-by-merge` GraphQL response for (issue, pr), raw JSON
    /// (`closed_by_merge::query`); empty on failure.
    fn closed_by_merge_json(&self, repo: &str, issue: u64, pr: u64) -> String;
    /// Trusted evidence bodies for the AC classifier: the trusted comments
    /// of `number` (issue or PR — the REST issues endpoint serves both),
    /// plus the item's own body when `with_body` and its author is trusted
    /// (#9548).
    fn trusted_evidence(&self, repo: &str, number: u64, with_body: bool) -> Vec<String>;
    /// `classify-ac-verification.sh` hermetically: exit code and stdout.
    /// `None` when the script could not run at all — the caller treats that
    /// as rc 1 (fail closed).
    fn classify_ac(&self, body: &str, evidence: &str, head: &str) -> Option<(i32, String)>;
    /// One `lib/forge-helpers.sh` writer; true on exit 0.
    fn helper(&self, func: &str, args: &[&str]) -> bool;
    /// `gh issue close <n> --reason completed --comment <body>`; true on ok.
    fn close_issue(&self, repo: &str, issue: &str, comment: &str) -> bool;
    /// `gh issue edit <n> --add-label <label>`; true on ok.
    fn add_label(&self, repo: &str, issue: &str, label: &str) -> bool;
}

/// Adapt a [`Forge`] into the [`bookkeeping::Host`] the retired passes take,
/// intercepting comment posts for the overlap-release dedupe
/// ([`comment_already_posted`]).
struct AsHost<'a, F: Forge>(&'a F);

impl<F: Forge> Host for AsHost<'_, F> {
    fn read_issue(&self, repo: &str, issue: &str) -> String {
        self.0.read_issue(repo, issue)
    }
    fn commit_messages(&self, repo: &str, pr: &str) -> String {
        self.0.commit_messages(repo, pr)
    }
    fn issue_kind(&self, repo: &str, issue: &str) -> String {
        self.0.issue_kind(repo, issue)
    }
    fn helper(&self, func: &str, args: &[&str]) -> bool {
        if func == "forge_gh_comment_rl_safe" && args.len() >= 3 && dedupe_key(args[2]).is_some() {
            let existing = self.0.issue_comments(args[0], args[1]);
            if comment_already_posted(args[2], &existing) {
                log::debug!(
                    "merge_observation: issue #{} already carries this audit comment for its \
                     merge — not re-posting",
                    args[1]
                );
                return true;
            }
        }
        self.0.helper(func, args)
    }
    fn close_issue(&self, repo: &str, issue: &str, comment: &str) -> bool {
        self.0.close_issue(repo, issue, comment)
    }
    fn timestamp(&self) -> String {
        now_timestamp()
    }
}

fn mk_rec(level: &str, text: impl Into<String>) -> bookkeeping::Record {
    (level.to_string(), text.into())
}

/// The local world the observation's worktree-cleanup pass cleans (#14904):
/// the registered root this tick is reconciling, the cleanup orchestrator's
/// host binding, and the two git-only facts that stay with the caller —
/// everything the orchestrator's own `LOOM-NEED` protocol leaves to whoever
/// invoked it.
///
/// A bundle of closures rather than more [`Forge`] methods: these are LOCAL
/// workspace facts, not forge reads, and the production bindings are
/// one-liners over `worktree_cli` that tests replace whole, in the injected
/// style `run_reconciliation_pass_over_roots` injects its breaker check.
pub struct CleanupWorld<'a> {
    /// The registered root this tick is reconciling. Eligibility is decided
    /// per workspace: the paths, sentinels, porcelain and branch tips the
    /// gates read are THIS checkout's, so a merge completed elsewhere (or a
    /// record already complete in another host's store) neither suppresses
    /// nor forces cleanup here.
    pub root: &'a Path,
    /// The cleanup orchestrator's host binding. Production is
    /// [`cleanup::SystemHost::MERGE_OBSERVATION`]; the ledger mechanism it
    /// records is the observation pass's, not the shell's.
    pub host: &'a dyn cleanup::Host,
    /// `LOOM_PRESERVE_WORKTREE=1` — the workspace's preserve setting, read
    /// once by the caller so tests need not mutate process environment.
    pub preserve: bool,
    /// The checkout's default branch, resolved lazily on the pass's first
    /// run for a record (the resolver's third rung is a network call; a
    /// tick with no record reaching this pass must not pay for it).
    pub default_branch: &'a dyn Fn() -> Option<String>,
    /// `branch_has_landed` for a record's branch: `(branch, default,
    /// merged-head hint)` → the #7812 ladder's answer. The hint is the
    /// record's pinned `head`, so rung 2 answers without a forge round-trip
    /// and a branch whose local tip moved past the merged head reads
    /// `not-landed` — preserved, never guessed.
    pub landed: &'a dyn Fn(&str, Option<&str>, &str) -> cleanup::Landed,
}

/// The worktree/branch cleanup a direct-forge merge skipped (#14904):
/// `merge-pr.sh`'s whole post-merge cleanup block, replayed in-process from
/// the pinned record against THIS workspace through [`cleanup::run`] — the
/// same orchestrator, gates and order the shell's verb runs.
///
/// This pass supplies only what a forge observation legitimately can: the
/// record's pinned identity, the close targets the tick already fetched,
/// the issue state re-read on demand, and the landed ladder with the pinned
/// head as its hint. It never supplies `--worktree-path` or
/// `--remove-ignored`: those are the shell's CLI-only knobs, and an
/// external merge conveys no explicit user authorization to clean a
/// user-supplied path — so the override path (the only one that can remove
/// an unmanaged directory, #13599) stays unreachable here.
///
/// Returns false only when the `LOOM-NEED` loop could not converge (a fact
/// stayed unanswered); the caller leaves the pass unmarked and a later tick
/// retries. Every failure INSIDE the run is a warning record — the merge is
/// already confirmed and nothing here can un-merge it or unpick the
/// bookkeeping passes that already ran.
fn worktree_cleanup(
    f: &impl Forge,
    world: &CleanupWorld<'_>,
    rec: &MergeRecord,
    targets: &str,
    out: &mut Vec<bookkeeping::Record>,
) -> bool {
    let default_branch = (world.default_branch)();
    let inputs = cleanup::Inputs {
        repo_root: world.root.to_string_lossy().into_owned(),
        pr_number: rec.pr.to_string(),
        pr_branch: rec.branch.clone(),
        pr_head_sha: rec.head.clone(),
        default_branch: default_branch.clone(),
        // No CLI exists to turn these off for an observed merge; the
        // workspace's own preserve setting is the one opt-out passed
        // through, exactly as the script's default arm would see it.
        cleanup_worktree: true,
        cleanup_primary: true,
        preserve_env: world.preserve,
        worktree_path_override: String::new(),
        remove_ignored: false,
    };
    let mut facts = cleanup::Facts {
        close_targets: Some(targets.to_string()),
        ..cleanup::Facts::default()
    };
    // The shell's `for _ in 1 2 3 4` fact loop: at most one issue-state and
    // one landed answer exist (one issue number ever reaches the gate), so
    // four calls bound the loop with the same room to spare the shell kept.
    for _ in 0..4 {
        let need = match cleanup::run(&inputs, &facts, world.host) {
            cleanup::Outcome::Done { records, .. } => {
                // The `CD` record is the shell's: an in-process caller has
                // already been moved out of a removed cwd by the host's own
                // chdir, so there is nothing left to act on.
                out.extend(records);
                return true;
            }
            cleanup::Outcome::Need(need) => need,
        };
        match need {
            cleanup::Need::CloseTargets => {
                facts.close_targets = Some(targets.to_string());
            }
            cleanup::Need::IssueState(issue) => {
                // `forge_get_issue_state` semantics: `OPEN`/`CLOSED`, empty
                // on a failed read — which the #4186 gate reads as Preserve,
                // never as "closed, clean up".
                let view = IssueView::from_json(&f.read_issue(&rec.repo, &issue));
                facts.issue_state = Some(view.state.to_uppercase());
            }
            cleanup::Need::Landed => {
                facts.landed =
                    Some((world.landed)(&rec.branch, default_branch.as_deref(), &rec.head));
            }
        }
    }
    out.push(mk_rec(
        "WARNING",
        format!(
            "Post-merge worktree cleanup for PR #{} did not finish — a fact stayed \
             unanswered; nothing was removed and the pass retries next tick (#14904)",
            rec.pr
        ),
    ));
    false
}

fn log_records(rec: &MergeRecord, records: &[bookkeeping::Record]) {
    for (level, text) in records {
        let line = format!("merge_observation: PR #{} in {}: {text}", rec.pr, rec.repo);
        if level == "WARNING" {
            log::warn!("{line}");
        } else {
            log::info!("{line}");
        }
    }
}

/// The per-root JSON records of observed merges.
pub struct RecordStore {
    dir: PathBuf,
}

impl RecordStore {
    #[must_use]
    pub fn for_root(root: &Path) -> Self {
        Self {
            dir: root.join(".loom/state/merge-bookkeeping"),
        }
    }

    /// The stored record, if one parses; corrupt or absent files read as
    /// unobserved (the passes are convergent, so re-observing is safe).
    #[must_use]
    pub fn load(&self, pr: u64) -> Option<MergeRecord> {
        let text = std::fs::read_to_string(self.dir.join(format!("pr-{pr}.json"))).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Every stored record that has not completed all passes.
    #[must_use]
    pub fn incomplete(&self) -> Vec<MergeRecord> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut out: Vec<MergeRecord> = rd
            .flatten()
            .filter(|e| {
                let n = e.file_name();
                let n = n.to_string_lossy();
                n.starts_with("pr-") && n.ends_with(".json")
            })
            .filter_map(|e| {
                serde_json::from_str::<MergeRecord>(&std::fs::read_to_string(e.path()).ok()?).ok()
            })
            .filter(|r| !r.complete())
            .collect();
        out.sort_by_key(|r| r.pr);
        out
    }

    /// Write the record atomically (temp file + rename).
    ///
    /// # Errors
    ///
    /// The record could not be persisted; the caller leaves the merge for a
    /// later tick rather than act on an unpinned identity.
    pub fn store(&self, r: &MergeRecord) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let text =
            serde_json::to_string_pretty(r).map_err(|e| std::io::Error::other(e.to_string()))?;
        std::fs::write(self.dir.join(format!(".pr-{}.json.tmp", r.pr)), text)?;
        std::fs::rename(
            self.dir.join(format!(".pr-{}.json.tmp", r.pr)),
            self.dir.join(format!("pr-{}.json", r.pr)),
        )
    }
}

/// One pass over a workspace: discover merged PRs, and for each without a
/// completed record run the bookkeeping it may have missed.
pub fn run_pass(
    f: &impl Forge,
    world: &CleanupWorld<'_>,
    store: &RecordStore,
    ac_gate_on: bool,
    is_suppressed: impl Fn() -> bool,
) -> Stats {
    let mut stats = Stats::default();
    // An unreadable listing no longer ends the pass: recorded merges still
    // retry below, since a record is self-sufficient.
    let pulls = f.merged_pulls().unwrap_or_default();
    stats.merges_seen = pulls.len();
    let listed: Vec<u64> = pulls.iter().map(|p| p.number).collect();
    for row in pulls {
        if is_suppressed() {
            break;
        }
        let mut rec = match store.load(row.number) {
            Some(r) => r,
            None => {
                let Some(repo) = f.nwo().filter(|r| !r.is_empty() && r.contains('/')) else {
                    log::debug!(
                        "merge_observation: no repository identity for PR #{} — skipping",
                        row.number
                    );
                    continue;
                };
                let r = MergeRecord {
                    repo,
                    pr: row.number,
                    head: row.head.clone(),
                    merge_commit: row.merge_commit.clone(),
                    merged_at: row.merged_at.clone(),
                    branch: row.branch.clone(),
                    body: row.body.clone(),
                    observed_at: now_timestamp(),
                    done: Vec::new(),
                };
                if let Err(e) = store.store(&r) {
                    log::warn!(
                        "merge_observation: could not pin the merge record for PR #{}: {e} — \
                         retrying next tick",
                        row.number
                    );
                    continue;
                }
                log::info!(
                    "merge_observation: observed merge of PR #{} in {} at head {} (merge commit \
                     {}, merged at {})",
                    r.pr,
                    r.repo,
                    r.head,
                    r.merge_commit,
                    r.merged_at
                );
                r
            }
        };
        if rec.complete() {
            continue;
        }
        process(f, world, store, &mut rec, ac_gate_on);
        if rec.complete() {
            stats.records_completed += 1;
        }
    }
    // Deferred records whose PR has since left the discovery window: retry
    // from the pinned record, never from the listing.
    for mut rec in store.incomplete() {
        if is_suppressed() {
            break;
        }
        if listed.contains(&rec.pr) {
            continue;
        }
        process(f, world, store, &mut rec, ac_gate_on);
        if rec.complete() {
            stats.records_completed += 1;
        }
    }
    stats
}

/// Run the passes over one pinned record, marking each as done — and
/// persisting the mark — only once it has run.
fn process(
    f: &impl Forge,
    world: &CleanupWorld<'_>,
    store: &RecordStore,
    rec: &mut MergeRecord,
    ac_gate_on: bool,
) {
    // The close targets gate every close-dependent pass (the close check,
    // the branch close's "already a target" skip and the strip's target
    // list), and the partial-reset's reconstruction consults them as one of
    // its three closing-reference signals. An unreadable set is a missing
    // fact: defer the whole record rather than read "no answer" as "no
    // targets" — the shell had one shot and could not afford this; a tick
    // retries.
    let pr_s = rec.pr.to_string();
    let Some(targets) = f.close_targets(&rec.repo, &pr_s) else {
        log::debug!(
            "merge_observation: close targets of PR #{} unreadable — deferring the record",
            rec.pr
        );
        return;
    };

    if !rec.done.iter().any(|d| d == PARTIAL_RESET) {
        let mut out = Vec::new();
        if partial_reset_observed(f, rec, &targets, &mut out) {
            log_records(rec, &out);
            rec.mark(PARTIAL_RESET);
            let _ = store.store(rec);
        } else {
            log_records(rec, &out);
        }
    }

    if !rec.done.iter().any(|d| d == CLOSE_CHECK) {
        let mut out = Vec::new();
        if close_check(f, rec, &targets, ac_gate_on, &mut out) {
            log_records(rec, &out);
            rec.mark(CLOSE_CHECK);
            let _ = store.store(rec);
        } else {
            log_records(rec, &out);
        }
    }

    for (pass, token) in [
        (bookkeeping::Pass::BranchClose, BRANCH_CLOSE),
        (bookkeeping::Pass::Strip, STRIP),
    ] {
        if rec.done.iter().any(|d| d == token) {
            continue;
        }
        let mut out = Vec::new();
        run_bookkeeping_pass(pass, f, rec, &targets, &mut out);
        log_records(rec, &out);
        rec.mark(token);
        let _ = store.store(rec);
    }

    // #14904: last, as `_post_merge_cleanup` ran last in the script — the
    // bookkeeping above must not act on an issue whose worktree state this
    // is about to change, and the cleanup's own issue gates read the state
    // the passes above converged to.
    if !rec.done.iter().any(|d| d == WORKTREE_CLEANUP) {
        let mut out = Vec::new();
        if worktree_cleanup(f, world, rec, &targets, &mut out) {
            log_records(rec, &out);
            rec.mark(WORKTREE_CLEANUP);
            let _ = store.store(rec);
        } else {
            log_records(rec, &out);
        }
    }
}

/// One of the retired shell passes, replayed through the same verb the
/// shell calls — only the facts arrive from the record instead of the
/// script's variables.
fn run_bookkeeping_pass(
    pass: bookkeeping::Pass,
    f: &impl Forge,
    rec: &MergeRecord,
    targets: &str,
    out: &mut Vec<bookkeeping::Record>,
) {
    let inputs = bookkeeping::Inputs {
        repo: rec.repo.clone(),
        pr_number: rec.pr.to_string(),
        pr_branch: rec.branch.clone(),
        pr_body: rec.body.clone(),
        conflicted: String::new(),
        open_before_merge: String::new(),
    };
    let facts = bookkeeping::Facts {
        close_targets: Some(targets.to_string()),
    };
    match bookkeeping::run(pass, &inputs, &facts, &AsHost(f)) {
        bookkeeping::Outcome::Done(records) => out.extend(records),
        bookkeeping::Outcome::Need(need) => out.push(mk_rec(
            "WARNING",
            format!(
                "Post-merge bookkeeping ({}) asked for {} after it was supplied — nothing done",
                need.token(),
                need.token()
            ),
        )),
    }
}

/// The partial-reset pass with post-merge-reconstructed facts (#3667/#4569,
/// #14902). See the module docs for what a direct merge cannot supply and
/// how each fact is re-derived or the mutation skipped.
///
/// Returns false when a fact this pass needs could not be established (an
/// unreadable claim timeline), so the caller leaves the pass unmarked and a
/// later tick retries — a transient read failure must not lose the eligible
/// swap. A verified newer claim is a decision, not a failure: it completes
/// the pass having deliberately reset nothing.
fn partial_reset_observed(
    f: &impl Forge,
    rec: &MergeRecord,
    targets: &str,
    out: &mut Vec<bookkeeping::Record>,
) -> bool {
    if rec.body.is_empty() {
        return true;
    }
    let mut complete = true;
    let pr_s = rec.pr.to_string();
    let commits = f.commit_messages(&rec.repo, &pr_s);
    let body_refs = closing_refs(&rec.body);
    let commit_refs = closing_refs(&commits);
    for n in partial_increment_refs(&rec.body) {
        let n_s = n.to_string();
        let issue_json = f.read_issue(&rec.repo, &n_s);
        let view = IssueView::from_json(&issue_json);
        if view.is_pr {
            continue;
        }
        let mut pre = PreMerge::default();
        if view.state != "open" {
            let conflicted = body_refs.contains(&n)
                || commit_refs.contains(&n)
                || targets.lines().any(|l| l.trim() == n_s);
            let facts =
                closed_by_merge::Facts::from_json(&f.closed_by_merge_json(&rec.repo, n, rec.pr));
            if let closed_by_merge::Verdict::ThisMerge(_) =
                closed_by_merge::decide(n, rec.pr, &facts)
            {
                // This merge closed it, so it was open right before the
                // merge — the shell's two pre-merge facts, re-derived.
                pre = PreMerge {
                    conflicted,
                    open_before_merge: true,
                };
            }
            // Any other verdict — an unrelated historical close, another
            // PR's close, a deliberate close, unattributable, or no answer
            // — leaves the facts unset: the dependent reopen is skipped.
        }
        // The newer-claim gate: only an OPEN, still-building issue is about
        // to have its claim reset, and only a claim that predates the merge
        // is this merge's to reset. A closed issue takes the reopen path
        // above, where the attribution already bounded the blast radius.
        if view.state == "open" && view.building {
            match claim_predates_merge(
                &f.label_events(&rec.repo, &n_s).unwrap_or_default(),
                &rec.merged_at,
            ) {
                Some(true) => {}
                Some(false) => {
                    out.push(mk_rec(
                        "WARNING",
                        format!(
                            "Partial-increment reset: issue #{n_s} was re-claimed by a newer \
                             Builder after PR #{pr_s} merged — leaving its live claim alone"
                        ),
                    ));
                    continue;
                }
                None => {
                    complete = false;
                    out.push(mk_rec(
                        "WARNING",
                        format!(
                            "Partial-increment reset: could not verify that issue #{n_s}'s \
                             loom:building claim predates PR #{pr_s}'s merge — leaving it \
                             untouched this pass"
                        ),
                    ));
                    continue;
                }
            }
        }
        let steps = partial_reset::plan(&n_s, &pr_s, &rec.repo, &view, pre);
        bookkeeping::apply_partial_steps(
            &n_s,
            &pr_s,
            &rec.repo,
            &issue_json,
            &steps,
            &AsHost(f),
            out,
        );
    }
    complete
}

/// Champion's "Step 4: Verify Issue Auto-Close" for one observed merge
/// (#6883, #1057/#8942, #12703): the out-of-band acceptance-criteria gate
/// (configured switch, current-head evidence marker), the negation-aware
/// reopen, and the belt-and-suspenders close.
///
/// Returns false when a target's state could not be read at all — the close
/// it would have needed is left for a later tick rather than lost.
fn close_check(
    f: &impl Forge,
    rec: &MergeRecord,
    targets: &str,
    ac_gate_on: bool,
    out: &mut Vec<bookkeeping::Record>,
) -> bool {
    let mut complete = true;
    let pr_s = rec.pr.to_string();
    // Champion's #1057 input: the PR body plus the merge commit's message.
    let neg_src =
        format!("{}\n{}\n", rec.body, f.merge_commit_message(&rec.repo, &rec.merge_commit));
    for raw in targets.lines().filter(|l| !l.trim().is_empty()) {
        let Ok(n) = raw.trim().parse::<u64>() else {
            continue;
        };
        let n_s = n.to_string();
        let issue_json = f.read_issue(&rec.repo, &n_s);
        let view = IssueView::from_json(&issue_json);
        if view.is_pr {
            continue;
        }

        if ac_gate_on {
            let mut evidence = String::new();
            for body in f.trusted_evidence(&rec.repo, n, false) {
                evidence.push_str(&body);
                evidence.push('\n');
            }
            for body in f.trusted_evidence(&rec.repo, rec.pr, true) {
                evidence.push_str(&body);
                evidence.push('\n');
            }
            let (rc, report) = f
                .classify_ac(&issue_body_from_json(&issue_json), &evidence, &rec.head)
                .unwrap_or((1, String::new()));
            if matches!(rc, 12 | 13 | 1) {
                hold_for_unverified_ac(f, rec, &n_s, &view, rc, &report, out);
                continue;
            }
        }

        if closing_ref_negation_status(&neg_src, n) == ClosingRefNegationStatus::NegatedOnly {
            match view.state.as_str() {
                "closed" => {
                    let facts = closed_by_merge::Facts::from_json(
                        &f.closed_by_merge_json(&rec.repo, n, rec.pr),
                    );
                    match closed_by_merge::decide(n, rec.pr, &facts) {
                        closed_by_merge::Verdict::ThisMerge(_) => {
                            if f.helper("forge_gh_reopen_issue_rl_safe", &[&rec.repo, &n_s]) {
                                let comment = format!(
                                    "Reopened: PR #{pr_s} only references this issue negated \
                                     (#1057)."
                                );
                                if !f.helper(
                                    "forge_gh_comment_rl_safe",
                                    &[&rec.repo, &n_s, &comment],
                                ) {
                                    out.push(mk_rec(
                                        "WARNING",
                                        format!(
                                            "Could not post the negated-reference reopen comment \
                                             on issue #{n_s} (reopen still applied)"
                                        ),
                                    ));
                                }
                                out.push(mk_rec(
                                    "SUCCESS",
                                    format!(
                                        "Issue #{n_s}: reopened (PR #{pr_s} references it negated \
                                         only, and this merge closed it — #1057/#8942)"
                                    ),
                                ));
                            } else {
                                out.push(mk_rec(
                                    "WARNING",
                                    format!(
                                        "Could not reopen issue #{n_s} after PR #{pr_s}'s \
                                         negated-only reference — reopen manually: gh issue \
                                         reopen {n_s} --repo {}",
                                        rec.repo
                                    ),
                                ));
                            }
                        }
                        other => out.push(mk_rec(
                            "INFO",
                            closed_by_merge::render(n, rec.pr, &other)
                                .trim()
                                .to_string(),
                        )),
                    }
                }
                "open" => out.push(mk_rec(
                    "INFO",
                    format!(
                        "Issue #{n_s}: referenced negated-only — left open, not closed (#1057)"
                    ),
                )),
                _ => out.push(mk_rec(
                    "WARNING",
                    format!(
                        "Issue #{n_s}: state unreadable — not reopened for a negated-only \
                         reference (#1057/#8942)"
                    ),
                )),
            }
            continue;
        }

        match view.state.as_str() {
            "closed" => out.push(mk_rec(
                "INFO",
                format!("Issue #{n_s} is closed (closed by PR #{pr_s}'s merge)"),
            )),
            "open" => {
                let comment = format!(
                    "Closed by PR #{pr_s} (merge observed by Loom post-merge bookkeeping, \
                     #14902)."
                );
                if f.close_issue(&rec.repo, &n_s, &comment) {
                    out.push(mk_rec(
                        "SUCCESS",
                        format!("Issue #{n_s}: closed (linked to merged PR #{pr_s})"),
                    ));
                } else {
                    out.push(mk_rec(
                        "WARNING",
                        format!("Could not close issue #{n_s} after observing PR #{pr_s}'s merge"),
                    ));
                }
            }
            _ => {
                complete = false;
                out.push(mk_rec(
                    "WARNING",
                    format!(
                        "Issue #{n_s}: state unreadable — not closed (required fact not \
                         established, #14902)"
                    ),
                ));
            }
        }
    }
    complete
}

/// The out-of-band AC hold (#6883): reopen if the merge already closed the
/// issue, post the marker-idempotent notice, and add `loom:operator`. The
/// marker is byte-identical to Champion's own
/// (`champion-pr-merge.md` → `hold_issue_on_unverified_ac`), so a hold
/// posted by either side is recognized by both.
fn hold_for_unverified_ac(
    f: &impl Forge,
    rec: &MergeRecord,
    issue: &str,
    view: &IssueView,
    rc: i32,
    report: &str,
    out: &mut Vec<bookkeeping::Record>,
) {
    if view.state == "closed" && !f.helper("forge_gh_reopen_issue_rl_safe", &[&rec.repo, issue]) {
        out.push(mk_rec(
            "WARNING",
            format!("Could not reopen issue #{issue} to hold it open for out-of-band verification"),
        ));
    }
    let reason = match rc {
        12 => format!("no `loom:ac-verified` marker was found on this issue or on PR #{}", rec.pr),
        13 => format!(
            "a `loom:ac-verified` marker exists, but it names a different tree than the merged \
             head `{}`",
            rec.head
        ),
        _ => "the acceptance-criteria classifier could not complete, so this gate fails closed"
            .to_string(),
    };
    let quoted = report
        .lines()
        .filter_map(|l| {
            let (phrase, text) = l.split_once('\t')?;
            Some(format!("> - [ ] {text}\n>\n>   _(matched: `{phrase}`)_\n"))
        })
        .collect::<String>();
    let marker = format!("<!-- champion:ac-hold pr={} sha={} -->", rec.pr, rec.head);
    let body = format!(
        "{marker}\n**Loom is holding this issue open.** PR #{pr} merged, but this issue's own \
         acceptance criteria include a step that CI structurally cannot perform, and {reason}.\n\
         \nUnmet criteria:\n\n{quoted}\nA green suite is not evidence for these — merging proves \
         the criteria CI can check, not a live-source, real-run, or over-time observation. See \
         `champion-pr-merge.md` → \"Out-of-Band Acceptance-Criteria Gate\" (#6883).\n\n**To clear \
         this**: perform the step, then post a comment on this issue (or on PR #{pr}) saying what \
         you ran and what you observed, ending with\n\n    <!-- loom:ac-verified sha=<head> \
         -->\n\nwhere `<head>` is the merged head `{head}` (the full 40-char SHA or a 7+-char \
         abbreviation of it both match), and close the issue. If the criterion was never actually \
         out-of-band, reword it so it no longer reads as live/scheduled/over-time and close \
         normally.\n\n*Automated by Loom post-merge bookkeeping (#14902)*",
        pr = rec.pr,
        head = rec.head
    );
    if !f.helper("forge_gh_comment_rl_safe", &[&rec.repo, issue, &body]) {
        out.push(mk_rec(
            "WARNING",
            format!("Could not post the AC-hold notice on issue #{issue}"),
        ));
    }
    if !f.add_label(&rec.repo, issue, "loom:operator") {
        out.push(mk_rec("WARNING", format!("Could not add loom:operator to held issue #{issue}")));
    }
    out.push(mk_rec(
        "SUCCESS",
        format!(
            "Issue #{issue}: held open — unverified out-of-band acceptance criterion (PR #{}, \
             classifier rc {rc})",
            rec.pr
        ),
    ));
}

// ---------------------------------------------------------------------------
// The live forge
// ---------------------------------------------------------------------------

/// `champion.outOfBandAcGate` for a workspace root.
fn ac_gate_enabled(root: &Path) -> bool {
    ac_gate_enabled_from(&crate::config_resolver::resolve_effective_config(root))
}

/// The production pass for one registered root. Best-effort and self-logging;
/// a mid-pass rate-limit trip stops between merges, leaving incomplete
/// records for the next tick.
pub(super) fn reconcile(gh_bin: &Path, root: &Path) {
    if crate::rate_limit_breaker::global_skip_pass("claim_reconciliation") {
        return;
    }
    let Some(forge) = GhForge::resolve(gh_bin, root, crate::forge_cmd::repo_nwo_in) else {
        log::debug!(
            "merge_observation: could not resolve the repository at {} — pass idle",
            root.display()
        );
        return;
    };
    let host = cleanup::SystemHost::MERGE_OBSERVATION;
    let world = CleanupWorld {
        root,
        host: &host,
        preserve: std::env::var("LOOM_PRESERVE_WORKTREE").as_deref() == Ok("1"),
        default_branch: &|| crate::worktree_cli::default_branch::resolve(root),
        landed: &|branch, default, hint| {
            landed_from_answer(crate::worktree_cli::branch_landed::probe(
                root, branch, default, hint,
            ))
        },
    };
    let stats =
        run_pass(&forge, &world, &RecordStore::for_root(root), ac_gate_enabled(root), || {
            crate::rate_limit_breaker::global_skip_pass("claim_reconciliation")
        });
    if stats.merges_seen > 0 {
        log::info!(
            "merge_observation: {} merged PR(s) in the window at {}, {} record(s) completed this \
             tick (#14902)",
            stats.merges_seen,
            root.display(),
            stats.records_completed
        );
    }
}

/// Map the #7812 ladder's [`Answer`] onto the orchestrator's [`cleanup::Landed`]
/// fact — the same three fields the shell captured from `branch_has_landed`'s
/// globals. `landed` is true only of a `landed` verdict: `not-landed` and the
/// fail-closed `unknown` both count as not landed, as the shell's rc did.
fn landed_from_answer(a: crate::worktree_cli::branch_landed::Answer) -> cleanup::Landed {
    use crate::worktree_cli::branch_landed::Verdict;
    cleanup::Landed {
        landed: a.verdict == Verdict::Landed,
        verdict: a.verdict.as_str().to_string(),
        evidence: a.evidence.as_str().to_string(),
    }
}

/// [`Forge`] over the facade, rooted at a workspace checkout. GitHub-only,
/// like the shell passes it replays (the bookkeeping wrapper is
/// `FORGE_TYPE == github`-gated there); on another forge the reads fail and
/// the pass stays idle.
pub struct GhForge<'a> {
    gh_bin: &'a Path,
    root: &'a Path,
    nwo: Option<String>,
    commits: std::cell::RefCell<std::collections::HashMap<String, String>>,
}

impl<'a> GhForge<'a> {
    #[must_use]
    pub fn new(gh_bin: &'a Path, root: &'a Path) -> Self {
        Self {
            gh_bin,
            root,
            nwo: None,
            commits: std::cell::RefCell::new(std::collections::HashMap::new()),
        }
    }

    /// Build the forge the way [`reconcile`] does: resolve the repository
    /// identity first (`resolve` is `forge_cmd::repo_nwo_in` in production)
    /// and return `None` without one. [`GhForge::new`] alone has no identity,
    /// so a forge that skipped this step would leave the pass idle forever.
    #[must_use]
    pub fn resolve(
        gh_bin: &'a Path,
        root: &'a Path,
        resolve: impl Fn(&str, Option<&Path>) -> Option<String>,
    ) -> Option<Self> {
        let nwo = resolve(&gh_bin.to_string_lossy(), Some(root))
            .filter(|n| !n.is_empty() && n.contains('/'))?;
        Some(Self::new(gh_bin, root).with_nwo(nwo))
    }

    /// Pin the repository identity (see [`GhForge::resolve`]).
    #[must_use]
    pub fn with_nwo(mut self, nwo: String) -> Self {
        self.nwo = Some(nwo);
        self
    }

    fn ok_json(&self, op: &'static str, args: &[&str]) -> Option<String> {
        gh_call::ok_stdout(gh_call::read(op, self.gh_bin, self.root).args(args))
            .map(|b| String::from_utf8_lossy(&b).into_owned())
    }
}

impl Forge for GhForge<'_> {
    fn merged_pulls(&self) -> Option<Vec<MergedPull>> {
        let path = format!(
            "repos/{{owner}}/{{repo}}/pulls?state=closed&sort=updated&direction=desc&per_page={MERGED_WINDOW}"
        );
        let raw = self.ok_json("merged_obs.prs", &["api", &path])?;
        Some(parse_merged_pulls(&raw))
    }

    fn nwo(&self) -> Option<String> {
        self.nwo.clone()
    }

    fn read_issue(&self, repo: &str, issue: &str) -> String {
        let path = format!("repos/{repo}/issues/{issue}");
        let o = gh_call::output(
            gh_call::read("merged_obs.issue_read", self.gh_bin, self.root).args(["api", &path]),
        );
        // `$(gh api … 2>/dev/null || echo '{}')`: on failure the error body
        // is followed by `{}` — the shell capture this pass replays.
        let mut s = match &o {
            Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
            Err(_) => String::new(),
        };
        if !matches!(&o, Ok(o) if o.status.success()) {
            s.push_str("{}\n");
        }
        s.trim_end_matches('\n').to_string()
    }

    fn commit_messages(&self, repo: &str, pr: &str) -> String {
        if let Some(hit) = self.commits.borrow().get(pr) {
            return hit.clone();
        }
        let path = format!("repos/{repo}/pulls/{pr}/commits");
        let msgs = self
            .ok_json("merged_obs.commits", &["api", &path, "--paginate"])
            .map(|raw| {
                let mut msgs = Vec::new();
                for doc in serde_json::Deserializer::from_str(&raw).into_iter::<Value>() {
                    let Ok(Value::Array(items)) = doc else { break };
                    for c in items {
                        if let Some(m) = c.pointer("/commit/message").and_then(Value::as_str) {
                            msgs.push(m.to_string());
                        }
                    }
                }
                msgs.join("\n")
            })
            .unwrap_or_default();
        self.commits
            .borrow_mut()
            .insert(pr.to_string(), msgs.clone());
        msgs
    }

    fn issue_kind(&self, repo: &str, issue: &str) -> String {
        let path = format!("repos/{repo}/issues/{issue}");
        let Some(raw) = self.ok_json("merged_obs.issue_kind", &["api", &path]) else {
            return String::new();
        };
        match serde_json::from_str::<Value>(&raw) {
            Ok(v) if v.get("pull_request").is_some() => "pr".to_string(),
            Ok(v) => v
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            Err(_) => String::new(),
        }
    }

    fn close_targets(&self, repo: &str, pr: &str) -> Option<String> {
        let out = gh_call::output(
            gh_call::read("merged_obs.close_targets", self.gh_bin, self.root).args([
                "pr",
                "view",
                pr,
                "--repo",
                repo,
                "--json",
                "closingIssuesReferences",
                "--jq",
                ".closingIssuesReferences[].number",
            ]),
        )
        .ok()?;
        if !out.status.success() {
            return None;
        }
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        // `forge_pr_close_targets`' `sort -un`.
        let mut nums: Vec<u64> = raw
            .lines()
            .filter_map(|l| l.trim().parse::<u64>().ok())
            .collect();
        nums.sort_unstable();
        nums.dedup();
        Some(
            nums.into_iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn issue_comments(&self, repo: &str, issue: &str) -> Vec<String> {
        let path = format!("repos/{repo}/issues/{issue}/comments?per_page=100");
        let Some(raw) = self.ok_json("merged_obs.issue_comments", &["api", &path, "--paginate"])
        else {
            return Vec::new();
        };
        let mut bodies = Vec::new();
        for doc in serde_json::Deserializer::from_str(&raw).into_iter::<Value>() {
            let Ok(Value::Array(items)) = doc else { break };
            for c in items {
                if let Some(b) = c.get("body").and_then(Value::as_str) {
                    bodies.push(b.to_string());
                }
            }
        }
        bodies
    }

    fn label_events(&self, repo: &str, issue: &str) -> Option<String> {
        let path = format!("repos/{repo}/issues/{issue}/timeline?per_page=100");
        self.ok_json("merged_obs.timeline", &["api", &path, "--paginate"])
    }

    fn merge_commit_message(&self, repo: &str, sha: &str) -> String {
        let path = format!("repos/{repo}/commits/{sha}");
        self.ok_json("merged_obs.commit_msg", &["api", &path, "--jq", ".commit.message"])
            .unwrap_or_default()
    }

    fn closed_by_merge_json(&self, repo: &str, issue: u64, pr: u64) -> String {
        let Some((owner, name)) = repo.split_once('/') else {
            return String::new();
        };
        let q = closed_by_merge::query(issue, pr);
        self.ok_json(
            "merged_obs.closed_by_merge",
            &[
                "api",
                "graphql",
                "-F",
                &format!("o={owner}"),
                "-F",
                &format!("r={name}"),
                "-f",
                &format!("query={q}"),
            ],
        )
        .unwrap_or_default()
    }

    fn trusted_evidence(&self, repo: &str, number: u64, with_body: bool) -> Vec<String> {
        let comments_path = format!("repos/{repo}/issues/{number}/comments");
        let Some(comments) =
            self.ok_json("merged_obs.comments_trusted", &["api", &comments_path, "--paginate"])
        else {
            return Vec::new();
        };
        let policy = crate::comment_trust::TrustPolicy::for_root(self.root);
        let mut items: Vec<Value> =
            crate::comment_trust::parse_listing(comments.as_bytes()).unwrap_or_default();
        if with_body {
            let item_path = format!("repos/{repo}/issues/{number}");
            if let Some(item) = self.ok_json("merged_obs.item_read", &["api", &item_path]) {
                if let Ok(v) = serde_json::from_str::<Value>(&item) {
                    items.insert(0, v);
                }
            }
        }
        let Ok(doc) = serde_json::to_vec(&Value::Array(items)) else {
            return Vec::new();
        };
        let Some(Value::Array(kept)) = crate::comment_trust::filter_document(&policy, &doc) else {
            return Vec::new();
        };
        kept.iter()
            .filter_map(|c| c.get("body").and_then(Value::as_str).map(String::from))
            .collect()
    }

    fn classify_ac(&self, body: &str, evidence: &str, head: &str) -> Option<(i32, String)> {
        let script = self.root.join(".loom/scripts/classify-ac-verification.sh");
        if !script.is_file() {
            return None;
        }
        let dir = tempfile::tempdir().ok()?;
        let body_path = dir.path().join("ac-body.md");
        std::fs::write(&body_path, body).ok()?;
        let evidence_path = dir.path().join("ac-evidence.md");
        std::fs::write(&evidence_path, evidence).ok()?;
        let (body_s, evidence_s) = (
            body_path.to_string_lossy().into_owned(),
            evidence_path.to_string_lossy().into_owned(),
        );
        let out = std::process::Command::new("bash")
            .arg(&script)
            .args([
                "--body-file",
                &body_s,
                "--evidence-file",
                &evidence_s,
                "--head-sha",
                head,
            ])
            .current_dir(self.root)
            .stdin(std::process::Stdio::null())
            .output();
        match out {
            Ok(o) => Some((
                o.status.code().unwrap_or(1),
                String::from_utf8_lossy(&o.stdout).into_owned(),
            )),
            Err(_) => None,
        }
    }

    fn helper(&self, func: &str, args: &[&str]) -> bool {
        let lib = self.root.join(".loom/scripts/lib/forge-helpers.sh");
        std::process::Command::new("bash")
            .arg("-c")
            .arg(r#"source "$1" || exit 127; shift; "$@""#)
            .arg("_")
            .arg(&lib)
            .arg(func)
            .args(args)
            .current_dir(self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn close_issue(&self, repo: &str, issue: &str, comment: &str) -> bool {
        gh_call::output(gh_call::write("merged_obs.issue_close", self.gh_bin, self.root).args([
            "issue",
            "close",
            issue,
            "--repo",
            repo,
            "--reason",
            "completed",
            "--comment",
            comment,
        ]))
        .is_ok_and(|o| o.status.success())
    }

    fn add_label(&self, repo: &str, issue: &str, label: &str) -> bool {
        gh_call::output(
            gh_call::write("merged_obs.issue_add_label", self.gh_bin, self.root).args([
                "issue",
                "edit",
                issue,
                "--repo",
                repo,
                "--add-label",
                label,
            ]),
        )
        .is_ok_and(|o| o.status.success())
    }
}

#[cfg(test)]
#[path = "merge_observation_tests.rs"]
mod tests;
