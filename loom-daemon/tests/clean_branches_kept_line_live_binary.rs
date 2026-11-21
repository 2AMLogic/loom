//! Live-binary check of the `kept:` line and summary for a worktree-held
//! branch (#10851, following #10766/#10848).
//!
//! `clean_branches` reports with `println!`, which a unit test cannot capture,
//! so this runs the real `loom-daemon clean --branches-only --force` against a
//! temp repo and asserts on what it actually printed. The repo has no `origin`
//! remote: a held branch is skipped before any remote or forge probe, so no
//! network call is made.

mod common;

use common::git;
use std::process::{Command, Stdio};

#[test]
fn clean_prints_the_kept_line_and_counts_held_branches_as_in_use() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".loom")).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "init"]);

    // A dirty worktree holding a stale (no remote) branch.
    let wt = root.join("wt-pr-1");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/issue-1",
            wt.to_str().unwrap(),
        ],
    );
    std::fs::write(wt.join("dirty.txt"), "uncommitted").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args([
            "clean",
            "--branches-only",
            "--force",
            "--workspace",
            repo.to_str().unwrap(),
        ])
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .stdin(Stdio::null())
        .output()
        .expect("loom-daemon clean must spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "clean failed\nstdout:\n{stdout}\nstderr:\n{stderr}");

    let kept = format!("kept: worktree {} holds feature/issue-1", wt.display());
    assert!(stdout.contains(&kept), "missing `{kept}` in:\n{stdout}");
    assert!(
        stdout.contains("In use (held by a worktree): 1 branch(es)"),
        "missing in-use summary in:\n{stdout}"
    );
    assert!(stdout.contains("Kept: 0 branch(es)"), "held branch counted as Kept:\n{stdout}");
    assert!(wt.join("dirty.txt").exists());
}

/// A historical squash merge proves the PR head shipped. It says nothing about
/// a later local commit, whether or not origin still has the old topic ref.
#[test]
fn safe_clean_preserves_local_work_after_an_issue_pr_merged() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let repo = root.join("repo");
    let branch = "feature/issue-42";
    let wt = repo.join(".loom/worktrees/issue-42");
    std::fs::create_dir_all(wt.parent().unwrap()).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "base"]);
    git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/project.git",
        ],
    );
    git(&repo, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
    git(&repo, &["worktree", "add", "-qb", branch, wt.to_str().unwrap()]);
    git(&wt, &["commit", "-q", "--allow-empty", "-m", "merged PR head"]);
    let tip = || {
        let out = Command::new("git")
            .current_dir(&repo)
            .args(["rev-parse", "--verify", branch])
            .output()
            .unwrap();
        out.status
            .success()
            .then(|| String::from_utf8(out.stdout).unwrap().trim().to_owned())
    };
    let merged = tip().unwrap();
    let topic = "refs/remotes/origin/feature/issue-42";
    let rows = root.join("pulls.json");
    let payload = serde_json::json!([{
        "number": 43, "state": "closed", "merged_at": "2020-01-01T00:00:00Z",
        "base": {"repo":{"full_name":"example/project"}},
        "head":{"ref":branch, "sha":merged, "repo":{"full_name":"example/project","owner":{"login":"example"}}}
    }]);
    std::fs::write(&rows, payload.to_string()).unwrap();
    let stub = root.join("gh");
    std::fs::write(
        &stub,
        r#"#!/bin/sh
case "$*" in
  *"issue view"*) echo CLOSED ;;
  *"pr list"*) echo '[{"number":43,"state":"MERGED","mergedAt":"2020-01-01T00:00:00Z"}]' ;;
  *"/pulls?"*) cat "$PULL_ROWS" ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let clean = |args: &[&str]| {
        let mut cmd = Command::new(common::daemon_bin());
        common::isolate_daemon_state(&mut cmd, &root);
        let out = cmd
            .current_dir(&repo)
            .args(["clean", "--safe", "--force", "--workspace"])
            .arg(&repo)
            .args(args)
            .env("LOOM_GH_BIN", &stub)
            .env("PULL_ROWS", &rows)
            .env("LOOM_HYGIENE_MEMO", "0")
            .env("LOOM_CONFIG_DEFAULTS_FILE", "")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(out.status.success(), "{stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        stdout
    };
    git(&wt, &["commit", "-q", "--allow-empty", "-m", "unpushed work"]);
    let unpushed = tip().unwrap();
    let dry = clean(&["--worktrees-only", "--dry-run"]);
    assert!(
        dry.contains(&format!("No remote or merged PR covers tip {unpushed} - keeping {branch}")),
        "{dry}"
    );
    assert!(wt.exists());
    assert_eq!(tip().as_deref(), Some(unpushed.as_str()));

    let actual = clean(&["--worktrees-only"]);
    assert!(actual.contains(&format!("keeping {branch}")), "{actual}");
    assert!(!wt.exists(), "{actual}");
    assert_eq!(tip().as_deref(), Some(unpushed.as_str()), "{actual}");
    for has_remote_topic in [true, false] {
        if has_remote_topic {
            git(&repo, &["update-ref", topic, &merged]);
        } else {
            git(&repo, &["update-ref", "-d", topic]);
        }
        let out = clean(&["--branches-only"]);
        assert!(out.contains("keeping feature/issue-42"), "{out}");
        assert_eq!(tip().as_deref(), Some(unpushed.as_str()), "{out}");
    }
    // If the forge is unavailable, even an exact historical head stays.
    git(&repo, &["branch", "-f", branch, &merged]);
    std::fs::write(&rows, "unreadable").unwrap();
    clean(&["--branches-only"]);
    assert_eq!(tip().as_deref(), Some(merged.as_str()));
    std::fs::write(&rows, payload.to_string()).unwrap();
    clean(&["--branches-only"]);
    assert!(tip().is_none(), "the squash-merged PR head can be deleted");

    git(&repo, &["branch", branch, &unpushed]);
    git(&repo, &["update-ref", topic, &unpushed]);
    clean(&["--branches-only"]);
    assert!(tip().is_none(), "a remote contains the exact tip");
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
