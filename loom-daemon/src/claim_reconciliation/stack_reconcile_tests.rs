//! Unit tests for the shared cascade walk, lease check, and marker helpers
//! ([`super`], #15597).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::*;
use crate::comment_trust::records::TEST_FLEET_AUTHOR;
use crate::forge_call_stats;
use crate::gh_invocation::test_support::{calls, stub};
use crate::merge_pr::reconcile::LeaseState;
use crate::test_git_helpers::{run, StackFixture as GitFixture};
use crate::types::{ForgeCallCounts, ForgeOperationCounts};

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

// --- live forge adapters -----------------------------------------------------

/// A stub `gh` that appends its argv to `log` and answers by endpoint: one
/// closed PR whose head is `feature/issue-7`, `lease_reply` for the lease
/// read of issue 7's comments, an empty comment listing for every other read
/// of them, one label on issue 7, and `[]` for anything else.
fn logging_stub(dir: &Path, log: &Path, lease_reply: &str) -> PathBuf {
    const CLOSED: &str = r#"[{"number":12,"state":"closed","head":{"ref":"feature/issue-7","sha":"abc123"},"merged_at":null}]"#;
    const LABELED: &str = r#"{"labels":[{"name":"loom:issue"}]}"#;
    let script = format!(
        "printf '%s\\n' \"$*\" >> '{log}'\n\
         case \"$2\" in\n\
         \t*pulls?head=*) printf '%s' '{CLOSED}' ;;\n\
         \t*issues/7/comments*)\n\
         \t\tif [ \"$4\" = --jq ]; then printf '%s\\n' '{lease_reply}'\n\
         \t\telse printf '%s' '[]'; fi ;;\n\
         \t*issues/7) printf '%s' '{LABELED}' ;;\n\
         \t*) printf '%s' '[]' ;;\n\
         esac",
        log = log.display()
    );
    stub(dir, "gh-log", &script)
}

/// Run `body` with this thread's call-stats sink at a fresh dir: the
/// per-call-site rows and the per-operation rows it produced.
fn report_after(body: impl FnOnce()) -> (Vec<ForgeCallCounts>, Vec<ForgeOperationCounts>) {
    let sink = tempfile::tempdir().unwrap();
    forge_call_stats::set_test_sink_dir(Some(sink.path().to_path_buf()));
    body();
    let report = forge_call_stats::status_report(chrono::Utc::now(), None);
    forge_call_stats::set_test_sink_dir(None);
    (report.host_window.unwrap_or_default(), report.operations.unwrap_or_default())
}

fn op_calls(rows: &[ForgeOperationCounts], operation: &str) -> u64 {
    rows.iter()
        .filter(|r| r.operation == operation)
        .map(|r| r.ok + r.error)
        .sum()
}

fn logged_argv(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A trusted author's lease-record comment, as the lease read's `--jq`
/// projection hands it to the policy.
fn lease_record(updated_at: &str) -> String {
    format!("{{\"updated_at\":\"{updated_at}\",{TEST_FLEET_AUTHOR}}}")
}

const PARENT_LOOKUP: &str =
    "api repos/{owner}/{repo}/pulls?head={owner}:feature/issue-7&state=closed&per_page=100";
const TRUSTED_COMMENTS: &str = "api repos/{owner}/{repo}/issues/7/comments?per_page=100 --paginate";
const LEASE_READ: &str = "api repos/{owner}/{repo}/issues/7/comments?per_page=100 --paginate --jq ";

fn closed_parent() -> Option<orphan::ParentPr> {
    Some(orphan::ParentPr {
        number: 12,
        merged: false,
        head_sha: Some("abc123".to_string()),
    })
}

#[test]
#[serial_test::serial]
fn orphan_forge_issues_its_calls_under_the_orphan_stack_op_names() {
    use orphan::{GhOrphanForge, OrphanForge};

    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let gh = logging_stub(tmp.path(), &log, &lease_record(&chrono::Utc::now().to_rfc3339()));
    let forge = GhOrphanForge {
        gh_bin: &gh,
        root: tmp.path(),
    };
    let (rows, operations) = report_after(|| {
        assert_eq!(forge.newest_closed_pr_for_head("feature/issue-7").unwrap(), closed_parent());
        assert_eq!(forge.trusted_comments(7), Some(vec![]));
        assert_eq!(forge.issue_lease_fresh(7), Some(true));
        forge.retarget(12, "main").unwrap();
        forge.add_label(12, "loom:merge-conflict").unwrap();
        forge.remove_label(12, "loom:merge-conflict").unwrap();
        forge.comment(12, "flagged").unwrap();
    });

    let argv = logged_argv(&log);
    assert_eq!(argv.len(), 7, "{argv:?}");
    assert_eq!(argv[0], PARENT_LOOKUP);
    assert_eq!(argv[1], TRUSTED_COMMENTS);
    assert!(argv[2].starts_with(LEASE_READ), "{}", argv[2]);
    assert_eq!(
        argv[3..],
        [
            "api -X PATCH repos/{owner}/{repo}/pulls/12 -f base=main",
            "api -X POST repos/{owner}/{repo}/issues/12/labels -f labels[]=loom:merge-conflict",
            "api -X DELETE repos/{owner}/{repo}/issues/12/labels/loom%3Amerge-conflict",
            "api -X POST repos/{owner}/{repo}/issues/12/comments -f body=flagged",
        ]
    );

    for name in [
        "orphan_stack.parent_lookup",
        "orphan_stack.comments",
        "orphan_stack.retarget",
        "orphan_stack.comment",
    ] {
        assert_eq!(calls(&rows, name), 1, "{name}: {rows:?}");
    }
    assert_eq!(calls(&rows, "orphan_stack.label"), 2, "{rows:?}");
    assert!(rows.iter().all(|r| !r.caller.starts_with("merged_stack.")), "{rows:?}");
    // The lease read (`claim.lease_comments`) books `comment.list` too.
    for (operation, n) in [
        ("pr.list-by-head", 1),
        ("comment.list", 2),
        ("issue.edit-labels", 2),
        ("pr.edit-base", 1),
        ("comment.create", 1),
    ] {
        assert_eq!(op_calls(&operations, operation), n, "{operation}: {operations:?}");
    }
}

#[test]
#[serial_test::serial]
fn merged_forge_issues_its_calls_under_the_merged_stack_op_names() {
    use super::super::merged_stack::{GhMergedForge, MergedForge};

    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let gh = logging_stub(tmp.path(), &log, &lease_record(&chrono::Utc::now().to_rfc3339()));
    let forge = GhMergedForge {
        gh_bin: &gh,
        root: tmp.path(),
    };
    let (rows, _) = report_after(|| {
        assert_eq!(forge.newest_closed_pr_for_head("feature/issue-7").unwrap(), closed_parent());
        assert_eq!(forge.trusted_comments(7), Some(vec![]));
        assert_eq!(forge.issue_labels(7), "loom:issue");
        assert_eq!(forge.lease(7), LeaseState::Held);
        forge.retarget(12, "main").unwrap();
        forge.comment(12, "restacked").unwrap();
    });

    let argv = logged_argv(&log);
    assert_eq!(argv.len(), 6, "{argv:?}");
    assert_eq!(argv[0], PARENT_LOOKUP);
    assert_eq!(argv[1], TRUSTED_COMMENTS);
    assert_eq!(argv[2], "api repos/{owner}/{repo}/issues/7");
    assert!(argv[3].starts_with(LEASE_READ), "{}", argv[3]);
    assert_eq!(
        argv[4..],
        [
            "api -X PATCH repos/{owner}/{repo}/pulls/12 -f base=main",
            "api -X POST repos/{owner}/{repo}/issues/12/comments -f body=restacked",
        ]
    );

    for name in [
        "merged_stack.parent_lookup",
        "merged_stack.comments",
        "merged_stack.issue_labels",
        "merged_stack.retarget",
        "merged_stack.comment",
    ] {
        assert_eq!(calls(&rows, name), 1, "{name}: {rows:?}");
    }
    assert!(rows.iter().all(|r| !r.caller.starts_with("orphan_stack.")), "{rows:?}");
}

#[test]
#[serial_test::serial]
fn both_forges_read_the_lease_through_one_probe_and_project_it_their_own_way() {
    use super::super::merged_stack::{GhMergedForge, MergedForge};
    use orphan::{GhOrphanForge, OrphanForge};

    let tmp = tempfile::tempdir().unwrap();
    let stale = logging_stub(
        tmp.path(),
        &tmp.path().join("stale.log"),
        &lease_record("2020-01-01T00:00:00Z"),
    );
    let absent = logging_stub(tmp.path(), &tmp.path().join("absent.log"), "[]");
    let failing = stub(tmp.path(), "gh-fail", "exit 1");
    for (gh, fresh, state) in [
        (&stale, Some(false), LeaseState::NoneFresh),
        (&absent, Some(false), LeaseState::NoneFresh),
        (&failing, None, LeaseState::Unknown),
    ] {
        let root = tmp.path();
        assert_eq!(GhOrphanForge { gh_bin: gh, root }.issue_lease_fresh(7), fresh, "{gh:?}");
        assert_eq!(GhMergedForge { gh_bin: gh, root }.lease(7), state, "{gh:?}");
    }
}

#[test]
#[serial_test::serial]
fn prepare_reconcile_reuses_a_handed_down_listing_and_skips_an_unknown_branch() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("argv.log");
    let gh = logging_stub(tmp.path(), &log, "[]");
    let rows = vec![open_pr(7, "feature/issue-7", &"a".repeat(40), "main")];

    let saved = std::env::var("LOOM_DEFAULT_BRANCH").ok();
    std::env::set_var("LOOM_DEFAULT_BRANCH", "main");
    let handed_down = prepare_reconcile("test stack", &gh, tmp.path(), Some(&rows));
    std::env::remove_var("LOOM_DEFAULT_BRANCH");
    let unknown_branch = prepare_reconcile("test stack", &gh, tmp.path(), Some(&rows));
    if let Some(v) = saved {
        std::env::set_var("LOOM_DEFAULT_BRANCH", v);
    }

    match handed_down {
        Some((branch, open)) => {
            assert_eq!(branch, "main");
            assert!(
                matches!(open, Cow::Borrowed(r) if std::ptr::eq(r, rows.as_slice())),
                "the handed-down rows must be reused, not copied: {open:?}"
            );
        }
        None => panic!("a known default branch and a handed-down listing must proceed"),
    }
    assert!(
        unknown_branch.is_none(),
        "an unknown default branch skips the pass: {unknown_branch:?}"
    );
    assert!(!log.exists(), "neither path may call gh");
}
