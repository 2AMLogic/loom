//! Tests for the merged-parent stacked-children pass (#14903).
//!
//! The pure predicates are tested on values. The pass itself runs against
//! real repositories (a bare "origin" plus a clone, under `tempfile`) with an
//! in-memory forge, so the replay, the pin and the push are the real git
//! operations and only the forge is simulated.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::*;

// --- pure predicates ---------------------------------------------------------

fn rec(
    pr: u64,
    branch: &str,
    head: &str,
    merge_commit: &str,
    merged_at: &str,
) -> merge_observation_record::Record {
    merge_observation_record::Record {
        pr,
        branch: branch.to_string(),
        head: head.to_string(),
        merge_commit: merge_commit.to_string(),
        merged_at: merged_at.to_string(),
    }
}

#[test]
fn parents_from_records_keeps_stackable_pinnable_records_newest_first() {
    let records = vec![
        rec(11, "feature/issue-11", &"b".repeat(40), &"c".repeat(40), "2026-11-24T09:00:00Z"),
        rec(9, "feature/issue-9", &"d".repeat(40), &"e".repeat(40), "2026-11-24T10:00:00Z"),
        // Not a stackable parent branch.
        rec(12, "fix/thing", &"f".repeat(40), &"0".repeat(40), "2026-11-24T11:00:00Z"),
        // No pinnable identity.
        rec(13, "feature/issue-13", "", &"1".repeat(40), "2026-11-24T12:00:00Z"),
        rec(14, "feature/issue-14", &"2".repeat(40), "", "2026-11-24T12:00:00Z"),
    ];
    let parents = parents_from_records(&records);
    assert_eq!(
        parents.iter().map(|p| p.pr).collect::<Vec<_>>(),
        vec![9, 11],
        "newest merged_at first, non-stackable and unpinnable dropped"
    );
    assert_eq!(parents[0].head, "d".repeat(40));
}

#[test]
fn defer_dedupe_matches_both_this_pass_and_the_shell_body() {
    // The shell's byte-frozen deferral (stacked_reconcile's defer_comment).
    let shell = crate::merge_pr::reconcile::defer_comment("905", "7", "feature/issue-904", "T");
    assert!(defer_already_posted(std::slice::from_ref(&shell), "feature/issue-904", 7));
    // This pass's own body.
    let mine = defer_comment_body("905", 7, 904, "feature/issue-904");
    assert!(defer_already_posted(std::slice::from_ref(&mine), "feature/issue-904", 7));
    // A different parent branch or a different child PR does not dedupe.
    assert!(!defer_already_posted(std::slice::from_ref(&shell), "feature/issue-903", 7));
    assert!(!defer_already_posted(std::slice::from_ref(&shell), "feature/issue-904", 8));
    assert!(mine.starts_with(&deferred_marker(904, "feature/issue-904")));
}

#[test]
fn pushed_earlier_reads_only_a_marker_naming_the_current_head() {
    let note = format!("{}\nbody", pushed_marker(904, "aaa", "bbb"));
    let bodies = vec!["unrelated".to_string(), note];
    assert_eq!(pushed_earlier(&bodies, 904, "bbb").as_deref(), Some("aaa"));
    assert_eq!(pushed_earlier(&bodies, 904, "ccc"), None, "the head moved on");
    assert_eq!(pushed_earlier(&bodies, 905, "bbb"), None, "another parent");
}

#[test]
fn flag_marker_is_keyed_by_parent_and_head() {
    let head = "f".repeat(40);
    let ctx = FlagContext {
        pr: 5,
        branch: "feature/issue-5",
        head: &head,
        parent_pr: 4,
        parent_branch: "feature/issue-4",
        upstream: "abc",
        default_branch: "main",
        retarget_to: Some("main"),
        cascade_onto: None,
    };
    let body = flag_comment_body(&ctx, &FlagReason::Conflict(vec!["a.txt".to_string()]));
    assert!(body.starts_with(&flagged_marker(4, &head)));
    assert!(body.contains("`a.txt`"));
    assert!(!body.contains("merge-conflict"), "no Doctor label from this pass");
    assert!(body.contains("git rebase --onto origin/main refs/loom/parent/feature/issue-4"));
    assert!(body.contains("gh pr edit 5 --base main"));
    let bodies = vec![body];
    assert!(already_flagged(&bodies, 4, &head));
    assert!(!already_flagged(&bodies, 4, &"0".repeat(40)));
    assert!(!already_flagged(&bodies, 3, &head));
    // The rewritten-parent refusal must not name the parent head as the
    // upstream of the recipe, since that is exactly the wrong range.
    let rewritten = flag_comment_body(&ctx, &FlagReason::ParentRewritten);
    assert!(rewritten.contains("is not an ancestor"), "{rewritten}");
    assert!(!rewritten.contains("git update-ref refs/loom/parent/feature/issue-4 abc"));
}

// --- the pass, on real repositories -----------------------------------------

use crate::test_git_helpers::{commit_file, configure, run};

const A: &str = "feature/issue-901";
const B: &str = "feature/issue-902";
const C: &str = "feature/issue-903";

/// origin + an "author" clone that builds the stack + the clone the pass
/// runs in (which never checked out any of the stack's branches).
struct Fixture {
    _tmp: tempfile::TempDir,
    origin: PathBuf,
    author: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let author = tmp.path().join("author");
        let repo = tmp.path().join("repo");
        run(
            tmp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "--initial-branch=main",
                origin.to_str().unwrap(),
            ],
        );
        std::fs::create_dir_all(&author).unwrap();
        run(&author, &["init", "-q", "--initial-branch=main"]);
        configure(&author);
        commit_file(&author, "base.txt", "base\n", "base");
        run(&author, &["remote", "add", "origin", origin.to_str().unwrap()]);
        run(&author, &["push", "-q", "origin", "main"]);
        Self {
            _tmp: tmp,
            origin,
            author,
            repo,
        }
    }

    /// Commit `file` on a new branch `name` off `from` in the author clone,
    /// push it, and record it as PR `pr`'s head (`refs/pull/<pr>/head`).
    fn branch(&self, name: &str, from: &str, pr: u32, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "-B", name, from]);
        let sha = commit_file(&self.author, file, body, &format!("{name}: {file}"));
        run(&self.author, &["push", "-q", "-f", "origin", &format!("{name}:{name}")]);
        run(
            &self.author,
            &[
                "push",
                "-q",
                "-f",
                "origin",
                &format!("{name}:refs/pull/{pr}/head"),
            ],
        );
        sha
    }

    /// Move origin's main forward with `file` — the squash merge, whose SHA
    /// differs from the parent's pre-merge tip by construction.
    fn advance_main(&self, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "main"]);
        let sha = commit_file(&self.author, file, body, &format!("main: {file}"));
        run(&self.author, &["push", "-q", "origin", "main"]);
        sha
    }

    /// The clone the pass runs in, made after the stack exists.
    fn clone_repo(&self) {
        run(
            self.origin.parent().unwrap(),
            &[
                "clone",
                "-q",
                self.origin.to_str().unwrap(),
                self.repo.to_str().unwrap(),
            ],
        );
        configure(&self.repo);
    }

    fn origin_rev(&self, rev: &str) -> String {
        run(&self.origin, &["rev-parse", rev])
    }

    fn origin_files(&self, rev: &str) -> Vec<String> {
        run(&self.origin, &["ls-tree", "--name-only", rev])
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn origin_count(&self, range: &str) -> usize {
        run(&self.origin, &["rev-list", "--count", range])
            .parse()
            .unwrap()
    }
}

/// The repository the fake forge says it is; [`open_pr`] heads live in it.
const REPO: &str = "acme/widgets";

fn open_pr(number: u32, head: &str, sha: &str, base: &str) -> RestPull {
    RestPull {
        number,
        state: "open".to_string(),
        head_ref: Some(head.to_string()),
        head_sha: Some(sha.to_string()),
        base_ref: Some(base.to_string()),
        head_repo: Some(REPO.to_string()),
        ..RestPull::default()
    }
}

#[derive(Default)]
struct FakeForge {
    /// Newest closed PR per head branch (shape A's parent lookup).
    parents: BTreeMap<String, orphan::ParentPr>,
    /// `loom:building` per child issue number.
    building_labels: BTreeMap<u32, bool>,
    lease: Option<LeaseState>,
    strategy_merge: bool,
    repo_unknown: bool,
    retarget_failures: std::cell::Cell<u32>,
    comments: RefCell<BTreeMap<u32, Vec<String>>>,
    retargets: RefCell<Vec<(u32, String)>>,
}

impl FakeForge {
    fn merged_parent(number: u32, sha: &str) -> orphan::ParentPr {
        orphan::ParentPr {
            number,
            merged: true,
            head_sha: Some(sha.to_string()),
        }
    }
}

impl MergedForge for FakeForge {
    fn newest_closed_pr_for_head(&self, branch: &str) -> Result<Option<orphan::ParentPr>, String> {
        Ok(self.parents.get(branch).cloned())
    }
    fn trusted_comments(&self, pr: u32) -> Option<Vec<String>> {
        Some(self.comments.borrow().get(&pr).cloned().unwrap_or_default())
    }
    fn issue_labels(&self, issue: u32) -> String {
        if self.building_labels.get(&issue).copied().unwrap_or(false) {
            "loom:building".to_string()
        } else {
            String::new()
        }
    }
    fn lease(&self, _issue: u32) -> LeaseState {
        self.lease.unwrap_or(LeaseState::NoneFresh)
    }
    fn strategy(&self) -> Strategy {
        if self.strategy_merge {
            Strategy::Merge
        } else {
            Strategy::Rebase
        }
    }
    fn repository(&self) -> Option<String> {
        (!self.repo_unknown).then(|| REPO.to_string())
    }
    fn retarget(&self, pr: u32, base: &str) -> Result<(), String> {
        self.retargets.borrow_mut().push((pr, base.to_string()));
        if self.retarget_failures.get() > 0 {
            self.retarget_failures.set(self.retarget_failures.get() - 1);
            return Err("HTTP 502".to_string());
        }
        Ok(())
    }
    fn comment(&self, pr: u32, body: &str) -> Result<(), String> {
        self.comments
            .borrow_mut()
            .entry(pr)
            .or_default()
            .push(body.to_string());
        Ok(())
    }
}

fn worktree_count(repo: &Path) -> usize {
    run(repo, &["worktree", "list", "--porcelain"])
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

/// Shape A: the child is still based on the merged parent's branch, the
/// parent head ref is gone, and the squash merge has a different SHA. One
/// tick must leave the child open on `main` with exactly its own commits.
#[test]
fn forge_merged_child_is_replayed_onto_main_with_only_its_own_commits() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    // main moves after the stack was built: the squash merge (a NEW commit,
    // different SHA from the parent tip) plus the destination the rebase
    // must use is the fetched tip.
    fx.clone_repo();
    let main_tip = fx.advance_main("m.txt", "squash-merged parent\n");
    // delete_branch_on_merge: the parent's branch is gone; only its PR ref
    // (refs/pull/1/head) still names the pre-merge tip.
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open, &[]);

    assert_eq!(stats.children, 1);
    assert_eq!((stats.restacked, stats.retargeted), (1, 1), "{stats:?}");
    assert_eq!(*forge.retargets.borrow(), vec![(2, "main".to_string())]);
    let new_b = fx.origin_rev(B);
    assert_ne!(new_b, b);
    assert_eq!(fx.origin_rev(&format!("{B}^")), main_tip, "B sits on the fetched main tip");
    assert_eq!(fx.origin_count(&format!("main..{B}")), 1, "only B's own commit");
    let files = fx.origin_files(B);
    assert!(files.contains(&"b.txt".to_string()) && files.contains(&"m.txt".to_string()));
    assert!(!files.contains(&"a.txt".to_string()), "the merged parent's change is gone");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(RESTACKED_MARKER_PREFIX), "{posted:?}");
    // The parent tip is positively recovered AND preserved as the pin.
    assert_eq!(
        run(&fx.repo, &["rev-parse", &pin_ref(A)]),
        a,
        "refs/loom/parent pin written for reconcile-stack.sh"
    );
    assert_eq!(worktree_count(&fx.repo), 1, "the scratch worktree is removed");
    assert_eq!(
        run(&fx.repo, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main",
        "the clone's own checkout is never switched"
    );

    // Idempotent: the retargeted child's base is main, so shape A no longer
    // matches; and its new head sits on the post-merge main, so no record
    // matches either. No second push, comment, or retarget.
    let after = vec![open_pr(2, B, &new_b, "main")];
    let again = reconcile_with(&forge, &fx.repo, "main", &after, &[]);
    assert_eq!(again, MergedStats::default());
    assert_eq!(fx.origin_rev(B), new_b, "nothing pushed again");
    assert_eq!(forge.retargets.borrow().len(), 1);
    assert_eq!(
        forge
            .comments
            .borrow()
            .get(&2)
            .cloned()
            .unwrap_or_default()
            .len(),
        1
    );
}

/// Shape B: GitHub already retargeted the child onto the default branch when
/// it deleted the parent's; the relationship survives only as git ancestry
/// against the recorded merge, and only the rebase is left to do.
#[test]
fn github_retargeted_child_is_found_by_ancestry_and_restacked_without_a_retarget() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    let squash = fx.advance_main("a.txt", "parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let records = vec![rec(1, A, &a, &squash, "2026-11-24T09:00:00Z")];
    let parents = parents_from_records(&records);
    let forge = FakeForge::default();
    // The child's base is already main (GitHub's retarget).
    let open = vec![open_pr(2, B, &b, "main")];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open, &parents);

    assert_eq!((stats.restacked, stats.retargeted), (1, 0), "{stats:?}");
    assert!(forge.retargets.borrow().is_empty(), "the base is already correct");
    let new_b = fx.origin_rev(B);
    assert_eq!(fx.origin_rev(&format!("{B}^")), squash, "B sits on the post-merge main");
    assert_eq!(fx.origin_count(&format!("main..{B}")), 1, "only B's own commit");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(RESTACKED_MARKER_PREFIX));
    assert!(posted[0].contains("GitHub retargeted it"), "{}", posted[0].as_str());
    assert_eq!(run(&fx.repo, &["rev-parse", &pin_ref(A)]), a);

    // Idempotent: the restacked head contains the merge commit, so the same
    // record never matches it again.
    let after = vec![open_pr(2, B, &new_b, "main")];
    let again = reconcile_with(&forge, &fx.repo, "main", &after, &parents);
    assert_eq!(again, MergedStats::default());
    assert_eq!(fx.origin_rev(B), new_b, "nothing pushed again");
}

/// A child GitHub retargeted that carries several merged parents reconciles
/// against the NEWEST one only.
#[test]
fn a_retargeted_child_with_two_merged_ancestors_uses_the_newest_parent() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent1\n");
    let b = fx.branch(B, A, 2, "b.txt", "parent2\n");
    let c = fx.branch(C, B, 3, "c.txt", "child\n");
    fx.clone_repo();
    let squash1 = fx.advance_main("a.txt", "parent1\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    let squash2 = fx.advance_main("b.txt", "parent2\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", B]);

    let records = vec![
        rec(1, A, &a, &squash1, "2026-11-24T09:00:00Z"),
        rec(2, B, &b, &squash2, "2026-11-24T10:00:00Z"),
    ];
    let parents = parents_from_records(&records);
    let forge = FakeForge::default();
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(3, C, &c, "main")], &parents);

    assert_eq!((stats.restacked, stats.retargeted), (1, 0), "{stats:?}");
    // Replaying from the OLDER parent would have carried b.txt's commit
    // again; the newest boundary drops both parents' commits.
    assert_eq!(fx.origin_count("main..feature/issue-903"), 1, "only C's own commit");
    let files = fx.origin_files(C);
    assert!(files.contains(&"c.txt".to_string()));
    assert!(!files.contains(&"a.txt".to_string()) || files.contains(&"b.txt".to_string()));
    let posted = forge.comments.borrow().get(&3).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].contains("parent=2"), "reported against the newest parent");
}

/// A `--no-ff`-merged parent's landed head is ON the default branch (the
/// merge commit's second parent), so the shape-B ancestry match runs through
/// the best-common-ancestors set, not the child's own commits — pinned here
/// beside the squash shape above (#15209).
#[test]
fn a_no_ff_merged_parent_is_matched_through_the_merge_base() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    // GitHub's merge button with --no-ff: main gains a real merge commit
    // whose second parent is the parent branch's head.
    run(&fx.author, &["checkout", "-q", "main"]);
    run(&fx.author, &["merge", "-q", "--no-ff", "-m", "Merge A", A]);
    let merged = run(&fx.author, &["rev-parse", "HEAD"]);
    run(&fx.author, &["push", "-q", "origin", "main"]);
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    fx.clone_repo();
    assert_eq!(
        run(&fx.repo, &["rev-parse", &format!("{merged}^2")]),
        a,
        "the --no-ff merge carries the parent head on the default branch"
    );

    let records = vec![rec(1, A, &a, &merged, "2026-11-24T09:00:00Z")];
    let parents = parents_from_records(&records);
    let forge = FakeForge::default();
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, "main")], &parents);

    assert_eq!((stats.restacked, stats.retargeted), (1, 0), "{stats:?}");
    let new_b = fx.origin_rev(B);
    assert_eq!(fx.origin_rev(&format!("{B}^")), merged, "B sits on the post-merge main");
    assert_eq!(fx.origin_count(&format!("main..{B}")), 1, "only B's own commit");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(RESTACKED_MARKER_PREFIX));
    assert!(posted[0].contains("parent=1"), "{}", posted[0].as_str());

    // Idempotent: the restacked head contains the merge commit, so the same
    // record never matches it again.
    let after = vec![open_pr(2, B, &new_b, "main")];
    let again = reconcile_with(&forge, &fx.repo, "main", &after, &parents);
    assert_eq!(again, MergedStats::default());
    assert_eq!(fx.origin_rev(B), new_b, "nothing pushed again");
}

/// A child that merged an older `main` into itself (the #13593 Doctor shape)
/// has SEVERAL best common ancestors with the merged default branch: the
/// `--no-ff` parent's head and the older `main` point it merged. git's
/// single-answer `merge-base` picks between them by commit date, so the
/// match must run against ALL of them — with the parent head committed
/// earlier, the single answer names the `main` side and the parent would be
/// missed (#15209).
#[test]
#[serial_test::serial]
fn a_child_that_merged_main_before_a_no_ff_parent_merge_still_matches() {
    let fx = Fixture::new();
    // Date-skew the fixture so the parent head is older than the `main`
    // point the child merges: the single-answer merge-base then names the
    // (newer) `main` point, not the parent head.
    std::env::set_var("GIT_AUTHOR_DATE", "2026-01-01T10:00:00");
    std::env::set_var("GIT_COMMITTER_DATE", "2026-01-01T10:00:00");
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    std::env::set_var("GIT_AUTHOR_DATE", "2026-01-01T10:05:00");
    std::env::set_var("GIT_COMMITTER_DATE", "2026-01-01T10:05:00");
    fx.branch(B, A, 2, "b.txt", "child\n");
    std::env::remove_var("GIT_AUTHOR_DATE");
    std::env::remove_var("GIT_COMMITTER_DATE");
    // `main` moves, and the child merges that point into itself.
    let p = fx.advance_main("m1.txt", "main move\n");
    run(&fx.author, &["checkout", "-q", B]);
    run(
        &fx.author,
        &[
            "merge",
            "-q",
            "--no-ff",
            "-m",
            "Merge main into child",
            "main",
        ],
    );
    let b2 = run(&fx.author, &["rev-parse", "HEAD"]);
    run(&fx.author, &["push", "-q", "-f", "origin", &format!("{B}:{B}")]);
    run(
        &fx.author,
        &[
            "push",
            "-q",
            "-f",
            "origin",
            &format!("{B}:refs/pull/2/head"),
        ],
    );
    // The parent merges --no-ff afterwards.
    run(&fx.author, &["checkout", "-q", "main"]);
    run(&fx.author, &["merge", "-q", "--no-ff", "-m", "Merge A", A]);
    let merged = run(&fx.author, &["rev-parse", "HEAD"]);
    run(&fx.author, &["push", "-q", "origin", "main"]);
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    fx.clone_repo();

    // The premise this test exists for: two best common ancestors, and the
    // single-answer merge-base names the `main` side — the shape a
    // singular-merge-base matcher would miss.
    let all: Vec<String> = run(&fx.repo, &["merge-base", "--all", &b2, "origin/main"])
        .lines()
        .map(str::to_string)
        .collect();
    assert!(all.contains(&a), "the parent head is a best common ancestor: {all:?}");
    assert!(all.contains(&p), "so is the merged-in main point: {all:?}");
    assert_ne!(run(&fx.repo, &["merge-base", &b2, "origin/main"]), a);

    let records = vec![rec(1, A, &a, &merged, "2026-11-24T09:00:00Z")];
    let parents = parents_from_records(&records);
    let forge = FakeForge::default();
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b2, "main")], &parents);

    assert_eq!((stats.restacked, stats.retargeted), (1, 0), "{stats:?}");
    // Only the child's own commit survives the replay: the merged-in `main`
    // side is already upstream.
    assert_eq!(fx.origin_count(&format!("main..{B}")), 1, "only B's own commit");
    assert_eq!(fx.origin_rev(&format!("{B}^")), merged, "B sits on the post-merge main");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(RESTACKED_MARKER_PREFIX));
}

/// A child cut from a MID-branch commit of the parent does not carry the
/// recorded head (the pre-merge TIP), so no record matches and nothing is
/// written — the fail direction of the shape-B filter, under both merge
/// shapes (#15209).
#[test]
fn a_child_cut_from_a_mid_branch_parent_commit_does_not_match() {
    let fx = Fixture::new();
    let a1 = fx.branch(A, "main", 1, "a.txt", "parent\n");
    // The parent advances past the point the child will be cut from; the
    // record pins the two-commit tip.
    run(&fx.author, &["checkout", "-q", A]);
    let a2 = commit_file(&fx.author, "a2.txt", "parent2\n", "parent work 2");
    run(&fx.author, &["push", "-q", "-f", "origin", &format!("{A}:{A}")]);
    run(
        &fx.author,
        &[
            "push",
            "-q",
            "-f",
            "origin",
            &format!("{A}:refs/pull/1/head"),
        ],
    );
    let b = fx.branch(B, &a1, 2, "b.txt", "child\n");
    fx.clone_repo();
    let squash = fx.advance_main("a.txt", "parent\nparent2\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let records = vec![rec(1, A, &a2, &squash, "2026-11-24T09:00:00Z")];
    let parents = parents_from_records(&records);
    let forge = FakeForge::default();
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, "main")], &parents);

    assert_eq!(stats, MergedStats::default(), "{stats:?}");
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
    assert!(forge.comments.borrow().is_empty());
    // The record WAS evaluated (its merge commit is in `behind`) and the
    // ancestry leg is what refused it: the mid-branch cut's own commit is
    // not the recorded tip.
    assert_ne!(a1, a2);
}

/// A Builder-active child (loom:building + a live lease) gets its rebase
/// DEFERRED with one comment and nothing pushed — the stacked-reconcile
/// decision — and neither this pass's nor the shell's earlier deferral is
/// duplicated on a later tick.
#[test]
fn builder_active_child_defers_once_and_pushes_nothing() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        building_labels: [(902u32, true)].into_iter().collect(),
        lease: Some(LeaseState::Held),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open, &[]);
    assert_eq!((stats.deferred, stats.restacked), (1, 0), "{stats:?}");
    assert!(forge.retargets.borrow().is_empty());
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].contains("## Stacked parent merged — reconciliation deferred"));
    assert!(posted[0].contains("./.loom/scripts/reconcile-stack.sh 2 feature/issue-901"));

    // Second tick while the Builder is still active: the existing deferral
    // (this pass's shape) suppresses a second comment.
    let again = reconcile_with(&forge, &fx.repo, "main", &open, &[]);
    assert_eq!(
        again,
        MergedStats {
            children: 1,
            ..MergedStats::default()
        },
        "{again:?}"
    );
    assert_eq!(
        forge
            .comments
            .borrow()
            .get(&2)
            .cloned()
            .unwrap_or_default()
            .len(),
        1
    );
    assert_eq!(fx.origin_rev(B), b);
}

/// The deferral the SHELL's own post-merge pass posted suppresses this pass's
/// deferral too — script-then-tick converges on one comment (overlap).
#[test]
fn a_shell_deferral_suppresses_the_daemon_deferral() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let shell_body =
        crate::merge_pr::reconcile::defer_comment("902", "2", A, "2026-11-24T09:00:00Z");
    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        building_labels: [(902u32, true)].into_iter().collect(),
        lease: Some(LeaseState::Held),
        comments: RefCell::new([(2u32, vec![shell_body])].into_iter().collect()),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!(
        stats,
        MergedStats {
            children: 1,
            ..MergedStats::default()
        },
        "{stats:?}"
    );
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
}

/// The deferred child is reconciled by a LATER tick once the Builder is done
/// — the "subsequent pass" the deferral comment promises.
#[test]
fn a_deferred_child_is_reconciled_by_a_later_tick_once_the_builder_finishes() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    let main_tip = fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let mut forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        building_labels: [(902u32, true)].into_iter().collect(),
        lease: Some(LeaseState::Held),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A)];
    reconcile_with(&forge, &fx.repo, "main", &open, &[]);
    assert_eq!(fx.origin_rev(B), b);

    // The Builder finished: loom:building stays (the issue keeps it until
    // merge) but the lease is gone.
    forge.lease = Some(LeaseState::NoneFresh);
    let stats = reconcile_with(&forge, &fx.repo, "main", &open, &[]);
    assert_eq!((stats.restacked, stats.retargeted), (1, 1), "{stats:?}");
    assert_eq!(fx.origin_rev(&format!("{B}^")), main_tip);
    assert_eq!(fx.origin_count(&format!("main..{B}")), 1);
}

/// Missing parent-tip evidence (the merged PR record names no head) flags
/// with the manual remedy and rewrites nothing.
#[test]
fn missing_parent_tip_evidence_flags_without_rewrite() {
    let fx = Fixture::new();
    fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();

    let forge = FakeForge {
        parents: [(
            A.to_string(),
            orphan::ParentPr {
                number: 1,
                merged: true,
                head_sha: None,
            },
        )]
        .into_iter()
        .collect(),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!((stats.flagged, stats.restacked), (1, 0), "{stats:?}");
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(&flagged_marker(1, &b)));
    assert!(posted[0].contains("names no head commit"), "{}", posted[0].as_str());

    // The flag marker keys on the head: the same state posts nothing again.
    let again = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!(
        again,
        MergedStats {
            children: 1,
            ..MergedStats::default()
        }
    );
}

/// Mismatched parent-tip evidence: the recorded (or pinned) parent head is
/// not an ancestor of the child — the parent was rewritten after the child
/// was cut — so the replay range cannot be established positively and the
/// branch is never rewritten, even when a pin disagrees with the record.
#[test]
fn mismatched_parent_tip_evidence_is_not_replayed() {
    let fx = Fixture::new();
    fx.branch(A, "main", 1, "a.txt", "parent v1\n");
    let b = fx.branch(B, A, 2, "b.txt", "child cut at v1\n");
    // The parent was rewritten after the child was cut, then merged at v2.
    let a2 = fx.branch(A, "main", 1, "a.txt", "parent v2\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent v2\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    // A stale pin names the merged v2 tip — wrong boundary for this child.
    run(&fx.repo, &["update-ref", &pin_ref(A), &a2]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a2))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!((stats.flagged, stats.restacked), (1, 0), "{stats:?}");
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].contains("is not an ancestor"), "{}", posted[0].as_str());
}

/// An independently advanced child (a newer push arrived between observation
/// and action) is left for the next tick, not replayed onto a stale view.
#[test]
fn an_independently_advanced_child_is_left_for_the_next_tick() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child v1\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    // The Builder pushes a newer child head after the listing was taken.
    let newer = fx.branch(B, A, 2, "b.txt", "child v2\n");

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    // The listing still reports the stale head.
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!(stats.skipped, 1, "{stats:?}");
    assert_eq!(fx.origin_rev(B), newer, "the newer head is untouched");
    assert!(forge.retargets.borrow().is_empty());
    assert!(forge.comments.borrow().is_empty());
}

/// A push whose retarget failed leaves a marker; the next tick retries the
/// retarget alone instead of replaying (and misreading) the rebased branch.
#[test]
fn a_failed_retarget_after_the_push_is_retried_alone() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        retarget_failures: std::cell::Cell::new(1),
        ..FakeForge::default()
    };
    let first = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!((first.restacked, first.skipped), (1, 1), "{first:?}");
    let pushed = fx.origin_rev(B);
    assert_ne!(pushed, b, "the replay was pushed before the retarget failed");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert!(posted[0].starts_with(&pushed_marker(1, &b, &pushed)), "{posted:?}");

    // Next tick: the listing reports the pushed head; only the retarget runs.
    let second = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &pushed, A)], &[]);
    assert_eq!((second.restacked, second.retargeted, second.flagged), (0, 1, 0), "{second:?}");
    assert_eq!(fx.origin_rev(B), pushed, "nothing pushed again");
    // Both retarget attempts are recorded (the first 502'd); only one push
    // ever happened, and only the marker + report comments exist.
    assert_eq!(
        *forge.retargets.borrow(),
        vec![(2, "main".to_string()), (2, "main".to_string())]
    );
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 2, "{posted:?}");
    assert!(posted[1].starts_with(RESTACKED_MARKER_PREFIX));
    assert!(posted[1].contains(&format!("old={b} new={pushed}")), "{}", posted[1]);
}

/// A merge-mode repository never rewrites the pushed branch (#10771): the
/// child is retargeted bare, with one comment, and nothing is pushed.
#[test]
fn merge_mode_retargets_the_base_without_rewriting() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        strategy_merge: true,
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!((stats.retargeted, stats.restacked, stats.flagged), (1, 0, 0), "{stats:?}");
    assert_eq!(*forge.retargets.borrow(), vec![(2, "main".to_string())]);
    assert_eq!(fx.origin_rev(B), b, "the branch is never rewritten");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(RETARGETED_MARKER_PREFIX));
    assert!(posted[0].contains("#10771"), "{}", posted[0].as_str());
}

/// The grandchild stacked on a restacked child is replayed onto its new head
/// (base unchanged), and a conflicting replay is flagged without a push.
#[test]
fn a_childs_own_children_are_replayed_onto_its_new_head() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    let c = fx.branch(C, B, 3, "c.txt", "grandchild\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let open = vec![open_pr(2, B, &b, A), open_pr(3, C, &c, B)];
    let stats = reconcile_with(&forge, &fx.repo, "main", &open, &[]);

    assert_eq!((stats.restacked, stats.cascaded), (1, 1), "{stats:?}");
    assert_eq!(*forge.retargets.borrow(), vec![(2, "main".to_string())]);
    assert_eq!(fx.origin_rev(&format!("{C}^")), fx.origin_rev(B));
    assert_eq!(fx.origin_count(&format!("{B}..{C}")), 1, "only C's own commit");
    assert!(!fx.origin_files(C).contains(&"a.txt".to_string()));
    assert!(fx.origin_files(C).contains(&"b.txt".to_string()));
    // The grandchild's note says its still-open parent was rewritten, not merged.
    let posted = forge.comments.borrow().get(&3).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1, "{posted:?}");
    assert!(posted[0].contains("Stacked parent rewritten"), "{}", posted[0]);
    assert!(!posted[0].contains("merged while"), "{}", posted[0]);
    assert!(!posted[0].contains("refs/loom/parent"), "{}", posted[0]);
}

/// A cascade flag is keyed apart from the parent-merged flag, so it cannot
/// suppress reconciliation once that parent itself merges, and its recipe
/// replays onto the parent's new head rather than the default branch.
#[test]
fn a_cascade_flag_is_distinct_from_the_parent_merged_flag() {
    let head = "f".repeat(40);
    let ctx = FlagContext {
        pr: 3,
        branch: "feature/issue-903",
        head: &head,
        parent_pr: 2,
        parent_branch: "feature/issue-902",
        upstream: "oldparenthead",
        default_branch: "main",
        retarget_to: None,
        cascade_onto: Some("newparenthead"),
    };
    let body = flag_comment_body(&ctx, &FlagReason::Refused("x".to_string()));
    assert!(!body.contains(&flagged_marker(2, &head)), "{body}");
    assert!(!already_flagged(std::slice::from_ref(&body), 2, &head));
    assert!(body.contains("git rebase --onto newparenthead oldparenthead"), "{body}");
    assert!(!body.contains("origin/main"), "{body}");
}

/// Merge mode, shape A, Builder lease held: the base-only retarget still
/// happens (it races no Builder) and no rebase deferral is promised.
#[test]
fn merge_mode_retargets_even_when_a_builder_is_active() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        building_labels: [(902u32, true)].into_iter().collect(),
        lease: Some(LeaseState::Held),
        strategy_merge: true,
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!((stats.retargeted, stats.deferred), (1, 0), "{stats:?}");
    assert_eq!(*forge.retargets.borrow(), vec![(2, "main".to_string())]);
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(RETARGETED_MARKER_PREFIX), "{}", posted[0]);
}

/// Merge mode, shape B: nothing the policy may do, so no comments, no
/// retargets and an all-zero pass.
#[test]
fn merge_mode_shape_b_does_nothing() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    let merged = fx.advance_main("a.txt", "parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    let records = parents_from_records(&[rec(1, A, &a, &merged, "2026-11-24T09:00:00Z")]);
    let forge = FakeForge {
        strategy_merge: true,
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, "main")], &records);
    assert_eq!(stats, MergedStats::default());
    assert!(forge.comments.borrow().is_empty());
    assert!(forge.retargets.borrow().is_empty());
}

#[test]
fn a_conflicting_replay_is_flagged_once_and_nothing_is_pushed() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "base.txt", "child edit\n");
    fx.clone_repo();
    fx.advance_main("base.txt", "main edit\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);

    assert_eq!((stats.flagged, stats.restacked), (1, 0), "{stats:?}");
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].contains("`base.txt`"), "{}", posted[0].as_str());

    // Flagged once per head: the same state posts nothing again.
    let again = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);
    assert_eq!(
        again,
        MergedStats {
            children: 1,
            ..MergedStats::default()
        }
    );
}

/// The rebase guard's `EMPTIED` (#8298, #9001): a replay that leaves the child
/// no change of its own — here a `merge=ours` path that the default branch
/// changed too — is flagged with the guard's reason, and nothing is pushed.
#[test]
fn a_replay_the_rebase_guard_empties_is_flagged_and_nothing_is_pushed() {
    let fx = Fixture::new();
    std::fs::write(fx.author.join(".gitattributes"), "install-metadata.json merge=ours\n").unwrap();
    let metadata = |commit: &str| {
        format!("{{\n  \"loom_version\": \"1\",\n  \"loom_commit\": \"{commit}\"\n}}\n")
    };
    commit_file(&fx.author, "install-metadata.json", &metadata("aaaa"), "base: metadata");
    run(&fx.author, &["push", "-q", "origin", "main"]);
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "install-metadata.json", &metadata("bbbb"));
    fx.advance_main(
        "install-metadata.json",
        "{\n  \"loom_version\": \"2\",\n  \"loom_commit\": \"aaaa\"\n}\n",
    );
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    fx.clone_repo();
    // The driver the attribute names, as a Loom repository configures it.
    run(&fx.repo, &["config", "merge.ours.driver", "true"]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let stats = reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &b, A)], &[]);

    assert_eq!(
        (stats.children, stats.flagged, stats.restacked, stats.retargeted),
        (1, 1, 0, 0),
        "{stats:?}"
    );
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
    assert!(forge.retargets.borrow().is_empty());
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].starts_with(&flagged_marker(1, &b)));
    assert!(
        posted[0].contains(
            "replaying it would leave no change of its own (the rebase guard reported \
             EMPTIED, #9282)"
        ),
        "{}",
        posted[0]
    );
    assert_eq!(worktree_count(&fx.repo), 1, "the scratch worktree is removed");
}

/// An already-reconciled child (rebased by the shell or by hand after the
/// merge) contains the merge commit and is left entirely alone.
#[test]
fn an_already_reconciled_child_is_untouched() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    let squash = fx.advance_main("a.txt", "parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);
    // The shell's reconcile-stack.sh already replayed the child onto main.
    run(&fx.author, &["checkout", "-q", "-B", B, "main"]);
    commit_file(&fx.author, "b.txt", "child\n", "child rebased");
    run(&fx.author, &["push", "-q", "-f", "origin", &format!("{B}:{B}")]);
    let rebased = fx.origin_rev(B);

    let records = vec![rec(1, A, &a, &squash, "2026-11-24T09:00:00Z")];
    let parents = parents_from_records(&records);
    let forge = FakeForge::default();
    let stats =
        reconcile_with(&forge, &fx.repo, "main", &[open_pr(2, B, &rebased, "main")], &parents);
    assert_eq!(stats, MergedStats::default(), "{stats:?}");
    assert_eq!(fx.origin_rev(B), rebased, "nothing pushed");
    assert!(forge.comments.borrow().is_empty());
}

/// A foreign head (a human's branch or a fork's) is flagged once and never
/// rewritten — same disposition as the orphan pass.
#[test]
fn a_foreign_head_is_flagged_once_and_never_rewritten() {
    let fx = Fixture::new();
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "human child\n");
    fx.clone_repo();
    fx.advance_main("m.txt", "squash-merged parent\n");
    run(&fx.author, &["push", "-q", "origin", "--delete", A]);

    let forge = FakeForge {
        parents: [(A.to_string(), FakeForge::merged_parent(1, &a))]
            .into_iter()
            .collect(),
        ..FakeForge::default()
    };
    let mut human = open_pr(2, B, &b, A);
    human.head_repo = Some("someone/widgets".to_string());
    let stats = reconcile_with(&forge, &fx.repo, "main", &[human], &[]);
    assert_eq!((stats.flagged, stats.restacked), (1, 0), "{stats:?}");
    assert_eq!(fx.origin_rev(B), b, "never rewritten");
    let posted = forge.comments.borrow().get(&2).cloned().unwrap_or_default();
    assert_eq!(posted.len(), 1);
    assert!(posted[0].contains("not a Loom branch"), "{}", posted[0].as_str());

    let mut human_again = open_pr(2, B, &b, A);
    human_again.head_repo = Some("someone/widgets".to_string());
    let again = reconcile_with(&forge, &fx.repo, "main", &[human_again], &[]);
    assert_eq!(
        again,
        MergedStats {
            children: 1,
            ..MergedStats::default()
        }
    );
}

/// The store behind shape B's parent identities: `all()` reads complete and
/// incomplete records alike and skips corrupt files.
#[test]
fn record_store_all_reads_every_parseable_record() {
    let tmp = tempfile::tempdir().unwrap();
    let store = super::super::pass_loop::merge_observation::RecordStore::for_root(tmp.path());
    let one = merge_observation_record::Record {
        pr: 7,
        branch: "feature/issue-7".into(),
        head: "a".repeat(40),
        merge_commit: "b".repeat(40),
        merged_at: "2026-11-24T09:00:00Z".into(),
    };
    let mut done = store_record(tmp.path(), 7, &one.head, &one.merge_commit, &one.merged_at);
    done.done = vec!["partial-reset".into()];
    store.store(&done).unwrap();
    let two = store_record(tmp.path(), 9, &"c".repeat(40), &"d".repeat(40), "2026-11-24T10:00:00Z");
    store.store(&two).unwrap();
    std::fs::write(tmp.path().join(".loom/state/merge-bookkeeping/pr-8.json"), "not json").unwrap();

    let all = store.all();
    assert_eq!(all.iter().map(|r| r.pr).collect::<Vec<_>>(), vec![7, 9]);
    assert!(all.iter().all(|r| !r.branch.is_empty()));

    let parents = parents_from_records(
        &all.iter()
            .map(|r| merge_observation_record::Record {
                pr: r.pr,
                branch: r.branch.clone(),
                head: r.head.clone(),
                merge_commit: r.merge_commit.clone(),
                merged_at: r.merged_at.clone(),
            })
            .collect::<Vec<_>>(),
    );
    assert_eq!(parents.iter().map(|p| p.pr).collect::<Vec<_>>(), vec![9, 7]);
}

fn store_record(
    _root: &Path,
    pr: u64,
    head: &str,
    merge_commit: &str,
    merged_at: &str,
) -> super::super::pass_loop::merge_observation::MergeRecord {
    super::super::pass_loop::merge_observation::MergeRecord {
        repo: "acme/widgets".into(),
        pr,
        head: head.to_string(),
        merge_commit: merge_commit.to_string(),
        merged_at: merged_at.to_string(),
        branch: format!("feature/issue-{pr}"),
        body: String::new(),
        observed_at: merged_at.to_string(),
        done: Vec::new(),
    }
}
