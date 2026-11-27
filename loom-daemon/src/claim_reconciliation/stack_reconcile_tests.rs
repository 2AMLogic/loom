//! Unit tests for the shared cascade walk, lease check, and marker helpers
//! ([`super`], #15597).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::*;
use crate::test_git_helpers::{commit_file, configure, run};

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

#[test]
fn pushed_marker_and_pushed_earlier_match_prefix_parent_and_current_head() {
    const ORPHAN: &str = "<!-- loom:orphan-stack pushed";
    const MERGED: &str = "<!-- loom:merged-stack pushed";

    let m = pushed_marker(ORPHAN, 7, "old111", "new222");
    assert_eq!(m, "<!-- loom:orphan-stack pushed parent=7 old=old111 new=new222 -->");

    let bodies = vec![
        "unrelated comment".to_string(),
        format!("  {m}  \nbody after marker"),
    ];
    assert_eq!(pushed_earlier(ORPHAN, &bodies, 7, "new222").as_deref(), Some("old111"));
    assert_eq!(pushed_earlier(MERGED, &bodies, 7, "new222"), None, "other pass prefix");
    assert_eq!(pushed_earlier(ORPHAN, &bodies, 8, "new222"), None, "other parent");
    assert_eq!(pushed_earlier(ORPHAN, &bodies, 7, "new333"), None, "head moved on");

    // Malformed `old=` values (empty or containing whitespace) never match.
    let malformed = vec![
        format!("{ORPHAN} parent=7 old= new=new222 -->"),
        format!("{ORPHAN} parent=7 old=two words new=new222 -->"),
    ];
    assert_eq!(pushed_earlier(ORPHAN, &malformed, 7, "new222"), None);
}

#[test]
fn flagged_marker_and_already_flagged_distinguish_prefixes_and_cascade_onto_suffix() {
    const PREFIX: &str = "<!-- loom:merged-stack flagged";
    let head = "f".repeat(40);
    let marker = flagged_marker(PREFIX, 12, &head);
    assert_eq!(marker, format!("<!-- loom:merged-stack flagged parent=12 head={head} -->"));

    let cascade_only = vec![format!(
        "{PREFIX} parent=12 head={head} onto=abc123 -->\nbody"
    )];
    assert!(
        !already_flagged(PREFIX, &cascade_only, 12, &head),
        "a cascade marker with onto=... must not satisfy already_flagged"
    );

    let flagged = vec![format!("{marker}\nbody")];
    assert!(already_flagged(PREFIX, &flagged, 12, &head));
    assert!(!already_flagged("<!-- loom:orphan-stack flagged", &flagged, 12, &head));
    assert!(!already_flagged(PREFIX, &flagged, 13, &head));
    assert!(!already_flagged(PREFIX, &flagged, 12, &"0".repeat(40)));
}

#[test]
fn branch_lease_blocks_checks_only_loom_issue_branches() {
    assert!(branch_lease_blocks("feature/issue-42", |n| {
        assert_eq!(n, 42);
        true
    }));
    assert!(!branch_lease_blocks("feature/issue-42", |_| false));
    assert!(!branch_lease_blocks("jdoe/fix", |_| {
        panic!("non-issue branches must not query issue leases")
    }));
    assert!(!branch_lease_blocks("main", |_| {
        panic!("default branch must not query issue leases")
    }));
}

struct GitFixture {
    _tmp: tempfile::TempDir,
    origin: PathBuf,
    author: PathBuf,
    repo: PathBuf,
}

impl GitFixture {
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
}

#[test]
fn cascade_replays_up_to_max_depth_and_flags_the_level_beyond_it() {
    let fx = GitFixture::new();
    // Root child PR #100 on `feature/issue-100`, rewritten from `old_root` to `new_root`.
    let old_root = fx.branch("feature/issue-100", "main", 100, "r.txt", "root v1\n");
    let mut open = Vec::new();
    let mut prev_branch = "feature/issue-100".to_string();
    // Build 9 stacked descendants: levels 1..=8 (PRs 101..=108) must cascade;
    // level 9 (PR 109) exceeds MAX_CASCADE_DEPTH (8) and must be flagged.
    for depth in 1..=(MAX_CASCADE_DEPTH + 1) {
        let pr_num = 100 + u32::try_from(depth).unwrap();
        let branch = format!("feature/issue-{pr_num}");
        let file = format!("d{depth}.txt");
        let sha = fx.branch(&branch, &prev_branch, pr_num, &file, "level\n");
        open.push(open_pr(pr_num, &branch, &sha, &prev_branch));
        prev_branch = branch;
    }
    let deepest_before = fx.origin_rev("feature/issue-109");
    let new_root = fx.branch("feature/issue-100", "main", 100, "r.txt", "root v2\n");
    fx.clone_repo();
    run(&fx.repo, &["fetch", "-q", "origin", "refs/pull/100/head"]);

    let root_pr = open_pr(100, "feature/issue-100", &new_root, "main");
    let mut cascaded_prs = Vec::new();
    let mut flagged_prs = Vec::new();
    let mut scratch_prs = Vec::new();

    let counts = cascade(
        &root_pr,
        &old_root,
        &new_root,
        &mut Cascade {
            repo: &fx.repo,
            open: &open,
            this_repo: Some(REPO),
            default_branch: "main",
            pass: "test stack",
            issue_tag: "#15597",
            scratch_path: |repo: &Path, pr: u32| {
                repo.join(format!(".loom/worktrees/test-cascade-pr-{pr}"))
            },
            issue_lease_blocks: &|_| false,
            on_cascaded: &mut |step, g_new| {
                scratch_prs.push(step.pr);
                cascaded_prs.push((step.pr, step.parent_pr, g_new.to_string()));
            },
            on_flag: &mut |step, reason| {
                flagged_prs.push((step.pr, step.parent_pr, reason));
            },
        },
    );

    assert_eq!(
        counts,
        CascadeCounts {
            cascaded: 8,
            skipped: 0
        }
    );
    assert_eq!(scratch_prs, (101..=108).collect::<Vec<_>>());
    assert_eq!(
        flagged_prs,
        vec![(
            109,
            108,
            CascadeFlag::Refused("the stack above #108 is deeper than 8 levels".to_string()),
        )]
    );
    assert_eq!(
        fx.origin_rev("feature/issue-109"),
        deepest_before,
        "level beyond MAX_CASCADE_DEPTH is never pushed"
    );
    assert_eq!(
        fx.origin_rev("feature/issue-101^"),
        new_root,
        "level 1 sits on the rewritten root"
    );
}

#[test]
fn cascade_enforces_hold_inflight_nohead_default_branch_lease_and_transient_rules() {
    let fx = GitFixture::new();
    let old_root = fx.branch("feature/issue-200", "main", 200, "r.txt", "root v1\n");

    let held_sha = fx.branch("feature/issue-201", "feature/issue-200", 201, "h.txt", "held\n");
    let inflight_sha =
        fx.branch("feature/issue-202", "feature/issue-200", 202, "i.txt", "inflight\n");
    let leased_sha = fx.branch("feature/issue-203", "feature/issue-200", 203, "l.txt", "leased\n");
    let moved_listed = fx.branch("feature/issue-204", "feature/issue-200", 204, "m.txt", "v1\n");
    let moved_live = fx.branch("feature/issue-204", "feature/issue-200", 204, "m2.txt", "v2\n");
    let main_sha = fx.origin_rev("main");

    let new_root = fx.branch("feature/issue-200", "main", 200, "r.txt", "root v2\n");
    fx.clone_repo();
    run(&fx.repo, &["fetch", "-q", "origin", "refs/pull/200/head"]);

    let mut held = open_pr(201, "feature/issue-201", &held_sha, "feature/issue-200");
    held.labels = vec!["loom:blocked".to_string()];

    let mut inflight = open_pr(202, "feature/issue-202", &inflight_sha, "feature/issue-200");
    inflight.labels = vec!["loom:reviewing".to_string()];

    let mut no_head = open_pr(205, "feature/issue-205", "not-a-sha", "feature/issue-200");
    no_head.head_sha = Some("short".to_string());

    let leased = open_pr(203, "feature/issue-203", &leased_sha, "feature/issue-200");
    let moved = open_pr(204, "feature/issue-204", &moved_listed, "feature/issue-200");
    let sync_main = open_pr(206, "main", &main_sha, "feature/issue-200");

    let open = vec![held, inflight, no_head, leased, moved, sync_main];
    let root_pr = open_pr(200, "feature/issue-200", &new_root, "main");
    let leased_issues: BTreeSet<u32> = [203].into_iter().collect();
    let mut flagged = Vec::new();

    let counts = cascade(
        &root_pr,
        &old_root,
        &new_root,
        &mut Cascade {
            repo: &fx.repo,
            open: &open,
            this_repo: Some(REPO),
            default_branch: "main",
            pass: "test stack",
            issue_tag: "#15597",
            scratch_path: git::scratch_path,
            issue_lease_blocks: &|issue| leased_issues.contains(&issue),
            on_cascaded: &mut |step, _| {
                panic!("none of these gated/moved descendants may push (got #{})", step.pr)
            },
            on_flag: &mut |step, reason| {
                flagged.push((step.pr, reason));
            },
        },
    );

    // Held (#201) increments `skipped`; DefaultBranch (#206) skips silently.
    assert_eq!(
        counts,
        CascadeCounts {
            cascaded: 0,
            skipped: 1
        }
    );
    assert_eq!(fx.origin_rev("feature/issue-201"), held_sha);
    assert_eq!(fx.origin_rev("feature/issue-204"), moved_live);

    assert_eq!(flagged.len(), 4, "{flagged:?}");
    assert_eq!(
        flagged[0],
        (
            202,
            CascadeFlag::Refused(
                "an agent is working on it (loom:reviewing / loom:treating)".to_string()
            )
        )
    );
    assert_eq!(
        flagged[1],
        (
            205,
            CascadeFlag::Refused("its head branch or head commit is not usable".to_string())
        )
    );
    assert_eq!(
        flagged[2],
        (
            203,
            CascadeFlag::Refused(
                "its issue lease is held, so a Builder may be pushing to it".to_string()
            )
        )
    );
    assert!(
        matches!(&flagged[3], (204, CascadeFlag::Refused(msg)) if msg.contains("not the listed head")),
        "{:?}",
        flagged[3]
    );

    // Foreign head (human branch or fork) and unknown repository identity all
    // map to `CascadeFlag::NotLoomBranch` without touching git.
    let mut forked = open_pr(207, "feature/issue-207", &held_sha, "feature/issue-200");
    forked.head_repo = Some("someone/fork".to_string());
    let human = open_pr(208, "jdoe/top", &held_sha, "feature/issue-200");

    let mut owner_flags = Vec::new();
    let _ = cascade(
        &root_pr,
        &old_root,
        &new_root,
        &mut Cascade {
            repo: &fx.repo,
            open: &[forked, human],
            this_repo: Some(REPO),
            default_branch: "main",
            pass: "test stack",
            issue_tag: "#15597",
            scratch_path: git::scratch_path,
            issue_lease_blocks: &|_| false,
            on_cascaded: &mut |_, _| panic!("foreign heads must not push"),
            on_flag: &mut |step, reason| owner_flags.push((step.pr, reason)),
        },
    );
    assert_eq!(
        owner_flags,
        vec![
            (207, CascadeFlag::NotLoomBranch),
            (208, CascadeFlag::NotLoomBranch),
        ]
    );

    let mut unknown_flags = Vec::new();
    let loom_shaped = open_pr(209, "feature/issue-209", &held_sha, "feature/issue-200");
    let _ = cascade(
        &root_pr,
        &old_root,
        &new_root,
        &mut Cascade {
            repo: &fx.repo,
            open: &[loom_shaped],
            this_repo: None,
            default_branch: "main",
            pass: "test stack",
            issue_tag: "#15597",
            scratch_path: git::scratch_path,
            issue_lease_blocks: &|_| false,
            on_cascaded: &mut |_, _| panic!("unknown repo identity must not push"),
            on_flag: &mut |step, reason| unknown_flags.push((step.pr, reason)),
        },
    );
    assert_eq!(unknown_flags, vec![(209, CascadeFlag::NotLoomBranch)]);
}
