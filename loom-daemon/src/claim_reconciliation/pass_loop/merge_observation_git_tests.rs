#![allow(clippy::unwrap_used)]
//! #15172: the observed-merge worktree cleanup (#14904), against REAL
//! disposable git checkouts.
//!
//! `merge_observation_tests.rs` pins the pass's wiring against an in-memory
//! `FakeHost`. Nothing there touches `git worktree`, a local branch tip, a
//! redirected cargo target or the removal ledger, so the production binding
//! ([`cleanup::SystemHost::MERGE_OBSERVATION`], `branch_landed::probe`,
//! `worktree_teardown`, `cargo_target::reclaim`, `removal_log`) was never
//! exercised end to end. These tests drive [`reconcile`] — the production
//! entry point — over `tempfile` checkouts of a bare local `origin.git`, with a
//! local executable `gh` stub standing in for the forge (no network, no live
//! forge, no credentials).
//!
//! Every test is `#[serial]` and clears the process-env inputs the code under
//! test reads (`LOOM_WORKTREE_ROOT`, `LOOM_PRESERVE_WORKTREE`,
//! `CARGO_TARGET_DIR`), so a fleet host's exported overrides cannot decide the
//! suite — a worker exports `CARGO_TARGET_DIR`, which would otherwise make the
//! cargo-target reclaim refuse as "machine-global".

use super::*;
use crate::env_test_support::EnvGuard;
use crate::merge_pr::post_merge_cleanup as cleanup;
use crate::worktree_ops::clean;
use crate::worktree_reaper::{self, WorktreeReaperConfig};
use serial_test::serial;
use std::collections::BTreeSet;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

const BRANCH: &str = "feature/issue-42";
const ISSUE_WT: &str = ".loom/worktrees/issue-42";
const JUDGE_WT: &str = ".loom/worktrees/pr-9";
const MERGED_AT: &str = "2026-11-21T10:00:00Z";

/// The env the code under test reads, cleared for the test's lifetime.
fn clean_env() -> EnvGuard {
    EnvGuard::clear(&[
        "CARGO_TARGET_DIR",
        "LOOM_WORKTREE_ROOT",
        "LOOM_PRESERVE_WORKTREE",
    ])
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=Loom Test",
            "-c",
            "user.email=loom@example.com",
            "-c",
            "commit.gpgsign=false",
            "-C",
        ])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {} failed: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare `origin.git`, the merged PR's head and merge commit, and the `gh`
/// stub that serves exactly that one merge.
struct Fixture {
    tmp: tempfile::TempDir,
    origin: PathBuf,
    head: String,
    gh: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().canonicalize().unwrap();
        let origin = base.join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "-q", "--bare", "-b", "main"]);

        // The seed clone plays the forge: it pushes `main`, the PR branch,
        // and the merge of one into the other (a merge made directly on the
        // forge, never through `merge-pr.sh`).
        let seed = base.join("seed");
        git(
            &base,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                seed.to_str().unwrap(),
            ],
        );
        git(&seed, &["checkout", "-q", "-b", "main"]);
        std::fs::write(seed.join("README.md"), "base\n").unwrap();
        std::fs::write(seed.join(".gitignore"), ".loom/\n.loom-managed\n.cargo/\n").unwrap();
        std::fs::write(
            seed.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir_all(seed.join("src")).unwrap();
        std::fs::write(seed.join("src/lib.rs"), "").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "base"]);
        git(&seed, &["push", "-q", "origin", "main"]);
        git(&seed, &["checkout", "-q", "-b", BRANCH]);
        std::fs::write(seed.join("feature.txt"), "the change\n").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-q", "-m", "the change"]);
        let head = git(&seed, &["rev-parse", "HEAD"]);
        git(&seed, &["push", "-q", "origin", BRANCH]);
        git(&seed, &["checkout", "-q", "main"]);
        git(
            &seed,
            &[
                "merge",
                "-q",
                "--no-ff",
                "-m",
                "Merge pull request #9",
                BRANCH,
            ],
        );
        let merge_commit = git(&seed, &["rev-parse", "HEAD"]);
        git(&seed, &["push", "-q", "origin", "main"]);

        let gh = base.join("fake-gh.sh");
        let pulls = serde_json::json!([{
            "number": 9,
            "head": {"sha": head, "ref": BRANCH},
            "merge_commit_sha": merge_commit,
            "merged_at": MERGED_AT,
            "body": "Closes #42",
        }]);
        let pulls_file = base.join("pulls.json");
        std::fs::write(&pulls_file, pulls.to_string()).unwrap();
        // Issue #42 is the PR's close target and already closed, so every
        // bookkeeping pass converges without a write and the pass reaches
        // `worktree-cleanup`. Anything unexpected fails loudly (exit 1).
        let script = format!(
            r#"#!/usr/bin/env bash
printf '%s\n' "$*" >> '{log}'
case "$1 $2" in
  "repo view") echo '{{"nameWithOwner":"o/r"}}' ;;
  "pr view") echo 42 ;;
  "api repos/{{owner}}/{{repo}}/pulls?state=closed"*) cat '{pulls}' ;;
  "api repos/o/r/issues/42") echo '{{"state":"closed","labels":[],"body":""}}' ;;
  "api repos/o/r/commits/"*) echo "Merge pull request #9" ;;
  "api repos/o/r/"*) echo '[]' ;;
  "api graphql") echo '{{}}' ;;
  *) echo "fake-gh: unexpected call: $*" >&2; exit 1 ;;
esac
"#,
            log = base.join("gh.log").display(),
            pulls = pulls_file.display(),
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

        Self {
            tmp,
            origin,
            head,
            gh,
        }
    }

    fn base(&self) -> PathBuf {
        self.tmp.path().canonicalize().unwrap()
    }

    /// A disposable workspace: a clone of `origin.git` with the merged PR's
    /// branch as a local ref at the merged head, the primary checkout on
    /// `main`, and the out-of-band AC gate off (this workspace has no
    /// classifier script to run).
    fn workspace(&self, label: &str) -> PathBuf {
        let root = self.base().join(label);
        git(
            &self.base(),
            &[
                "clone",
                "-q",
                self.origin.to_str().unwrap(),
                root.to_str().unwrap(),
            ],
        );
        git(&root, &["branch", BRANCH, &self.head]);
        std::fs::create_dir_all(root.join(".loom/worktrees")).unwrap();
        std::fs::write(root.join(".loom/config.json"), r#"{"champion":{"outOfBandAcGate":false}}"#)
            .unwrap();
        root
    }

    /// The Builder's managed `issue-42` worktree on the merged branch.
    fn issue_worktree(&self, root: &Path) -> PathBuf {
        let wt = root.join(ISSUE_WT);
        git(root, &["worktree", "add", "-q", wt.to_str().unwrap(), BRANCH]);
        std::fs::write(wt.join(".loom-managed"), "test").unwrap();
        wt
    }

    /// The Judge's managed `pr-9` review worktree (#6264), detached at head.
    fn judge_worktree(&self, root: &Path) -> PathBuf {
        let wt = root.join(JUDGE_WT);
        git(
            root,
            &[
                "worktree",
                "add",
                "-q",
                "--detach",
                wt.to_str().unwrap(),
                &self.head,
            ],
        );
        std::fs::write(wt.join(".loom-managed"), "test").unwrap();
        wt
    }

    /// Redirect the worktree's cargo output to an out-of-tree directory the
    /// worktree's own `.cargo/config.toml` names (the per-worktree shape the
    /// reclaim exists for) and fill it. Returns the target directory.
    fn redirect_cargo_target(&self, label: &str, wt: &Path) -> PathBuf {
        let target = self.base().join(format!("ct-{label}")).join("issue-42");
        std::fs::create_dir_all(target.join("debug")).unwrap();
        std::fs::write(target.join("debug/artifact.bin"), vec![0u8; 4096]).unwrap();
        std::fs::create_dir_all(wt.join(".cargo")).unwrap();
        std::fs::write(
            wt.join(".cargo/config.toml"),
            format!("[build]\ntarget-dir = \"{}\"\n", target.display()),
        )
        .unwrap();
        // Prove the redirect is live before relying on its reclaim, so a
        // resolver that quietly fell back to `<wt>/target` cannot make the
        // assertions below vacuous.
        let resolved = crate::worktree_ops::cargo_target::resolve_for_worktree(wt);
        assert_eq!(
            resolved.canonicalize().unwrap(),
            target.canonicalize().unwrap(),
            "cargo did not resolve the redirect"
        );
        target
    }

    /// The eligible workspace: managed `issue-42` + Judge `pr-9` worktrees,
    /// the first with a redirected cargo target.
    fn eligible(&self, label: &str) -> (PathBuf, PathBuf) {
        let root = self.workspace(label);
        let issue = self.issue_worktree(&root);
        let target = self.redirect_cargo_target(label, &issue);
        self.judge_worktree(&root);
        (root, target)
    }

    fn gh_log(&self) -> String {
        std::fs::read_to_string(self.base().join("gh.log")).unwrap_or_default()
    }
}

/// One tick of the production pass.
fn tick(fx: &Fixture, root: &Path) {
    reconcile(&fx.gh, root);
}

/// `merge-pr.sh post-merge-cleanup`'s side: the same orchestrator with the
/// script's own mechanism and facts, over the same real checkout.
fn script_cleanup(fx: &Fixture, root: &Path) {
    let inputs = cleanup::Inputs {
        repo_root: root.to_string_lossy().into_owned(),
        pr_number: "9".into(),
        pr_branch: BRANCH.into(),
        pr_head_sha: fx.head.clone(),
        default_branch: crate::worktree_cli::default_branch::resolve(root),
        cleanup_worktree: true,
        cleanup_primary: true,
        preserve_env: false,
        worktree_path_override: String::new(),
        remove_ignored: false,
    };
    let facts = cleanup::Facts {
        close_targets: Some("42\n".into()),
        ..cleanup::Facts::default()
    };
    match cleanup::run(&inputs, &facts, &cleanup::SystemHost::MERGE_PR) {
        cleanup::Outcome::Done { .. } => {}
        cleanup::Outcome::Need(n) => panic!("the script side unexpectedly asked for {n:?}"),
    }
}

/// What a workspace looks like on disk after a cleanup, with the workspace
/// root stripped so two roots compare.
#[derive(Debug, PartialEq, Eq)]
struct Disposition {
    /// `(relative path, still on disk)`.
    dirs: Vec<(String, bool)>,
    /// `git worktree list --porcelain`'s registrations.
    registered: BTreeSet<String>,
    /// Local branch refs.
    branches: BTreeSet<String>,
    /// Whether each redirected cargo target still exists.
    targets: Vec<bool>,
    /// Ledger removals, by relative worktree path.
    ledgered: BTreeSet<String>,
}

fn rel(root: &Path, p: &str) -> String {
    p.strip_prefix(root.to_str().unwrap())
        .unwrap_or(p)
        .to_string()
}

fn disposition(root: &Path, watched: &[&str], targets: &[&Path]) -> Disposition {
    Disposition {
        dirs: watched
            .iter()
            .map(|w| ((*w).to_string(), root.join(w).exists()))
            .collect(),
        registered: git(root, &["worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("worktree "))
            .map(|p| rel(root, p))
            .collect(),
        branches: git(root, &["for-each-ref", "--format=%(refname)", "refs/heads"])
            .lines()
            .map(str::to_string)
            .collect(),
        targets: targets.iter().map(|t| t.exists()).collect(),
        ledgered: ledger(root)
            .iter()
            .map(|e| rel(root, e["worktree"].as_str().unwrap()))
            .collect(),
    }
}

/// The removal ledger's lines, parsed.
fn ledger(root: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(crate::worktree_ops::removal_log::ledger_path(root))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn mechanisms(root: &Path) -> Vec<String> {
    ledger(root)
        .iter()
        .map(|e| e["mechanism"].as_str().unwrap().to_string())
        .collect()
}

fn record(root: &Path) -> MergeRecord {
    RecordStore::for_root(root)
        .load(9)
        .expect("no merge record")
}

/// Run one reaper worktree pass over `root` with a forge that says issue #42
/// is closed and its PR merged long ago, removing through the real remover.
fn reap(root: &Path) -> worktree_reaper::ReapReport {
    let active = std::collections::HashSet::new();
    let registered = clean::registered_worktree_paths(root);
    let is_registered = clean::is_registered_worktree_probe(&registered);
    let probes = clean::production_probes(
        &active,
        &|_| "CLOSED".to_string(),
        &|_| Some("2020-01-01T00:00:00Z".to_string()),
        &|_| clean::PrStatus::Merged {
            merged_at: "2020-01-01T00:00:00Z".to_string(),
        },
        &|_| true,
        &is_registered,
        chrono::Utc::now(),
    );
    let opts = worktree_reaper::reaper_clean_options_for(&WorktreeReaperConfig::default());
    worktree_reaper::reap_worktrees(
        root,
        &opts,
        &probes,
        &|_, _| None,
        &|_, _| Err("no backup route in this test".to_string()),
        &|path, issue| {
            clean::cleanup_worktree(
                root,
                path,
                issue,
                &clean::CleanOptions::default(),
                "worktree_reaper",
            )
            .is_ok()
        },
    )
}

#[test]
#[serial]
fn an_observed_merge_leaves_the_same_disposition_a_script_merge_does() {
    let _env = clean_env();
    let fx = Fixture::new();
    let (observed, observed_target) = fx.eligible("observed");
    let (scripted, scripted_target) = fx.eligible("scripted");
    let watched = [ISSUE_WT, JUDGE_WT];

    // Both eligible workspaces start with the full local state.
    assert!(observed_target.exists() && scripted_target.exists());
    assert!(observed.join(ISSUE_WT).is_dir() && observed.join(JUDGE_WT).is_dir());

    tick(&fx, &observed);
    script_cleanup(&fx, &scripted);

    let got = disposition(&observed, &watched, &[&observed_target]);
    let want = disposition(&scripted, &watched, &[&scripted_target]);
    assert_eq!(got, want, "an observed merge disposed of the workspace differently");

    // …and that disposition is the cleaned one, not two matching no-ops.
    assert_eq!(got.dirs, vec![(ISSUE_WT.to_string(), false), (JUDGE_WT.to_string(), false)]);
    assert_eq!(got.targets, vec![false], "the redirected cargo target was not reclaimed");
    assert!(!got.branches.contains(&format!("refs/heads/{BRANCH}")), "{:?}", got.branches);
    assert!(got.branches.contains("refs/heads/main"));
    assert_eq!(got.registered.len(), 1, "only the primary checkout stays registered");
    assert_eq!(got.ledgered, [format!("/{ISSUE_WT}"), format!("/{JUDGE_WT}")].into());

    // The ledger names the daemon's pass, and the script's names the script:
    // one removal each, never misattributed.
    assert_eq!(mechanisms(&observed), ["merge_observation", "merge_observation"]);
    assert_eq!(mechanisms(&scripted), ["merge-pr.sh", "merge-pr.sh"]);

    // The pass ran to completion over real forge reads.
    assert!(record(&observed).complete());
    assert_eq!(record(&observed).done.last().unwrap(), WORKTREE_CLEANUP);
    assert!(fx.gh_log().contains("pulls?state=closed"));
}

#[test]
#[serial]
fn a_dirty_worktree_with_a_newer_tip_survives_the_observed_merge() {
    let _env = clean_env();
    let fx = Fixture::new();
    let root = fx.workspace("dirty");
    let wt = fx.issue_worktree(&root);
    // A clean Judge review worktree beside it: the pass really runs here, so
    // "the dirty one survived" is a decision, not a pass that did nothing.
    let judge = fx.judge_worktree(&root);
    // A local commit past the merged head, then uncommitted edits on top.
    std::fs::write(wt.join("local.txt"), "work after the merge\n").unwrap();
    git(&wt, &["add", "local.txt"]);
    git(&wt, &["commit", "-q", "-m", "work after the merge"]);
    let newer_tip = git(&wt, &["rev-parse", "HEAD"]);
    assert_ne!(newer_tip, fx.head);
    std::fs::write(wt.join("README.md"), "uncommitted edit\n").unwrap();
    std::fs::write(wt.join("scratch.txt"), "untracked\n").unwrap();

    tick(&fx, &root);

    assert!(wt.is_dir(), "the dirty worktree was removed");
    assert_eq!(std::fs::read_to_string(wt.join("README.md")).unwrap(), "uncommitted edit\n");
    assert_eq!(std::fs::read_to_string(wt.join("scratch.txt")).unwrap(), "untracked\n");
    assert_eq!(git(&root, &["rev-parse", BRANCH]), newer_tip, "the branch moved");
    assert!(git(&root, &["worktree", "list", "--porcelain"]).contains("issue-42"));
    assert!(!judge.exists(), "the pass never ran: the clean review worktree survived");
    assert_eq!(
        ledger(&root)
            .iter()
            .map(|e| rel(&root, e["worktree"].as_str().unwrap()))
            .collect::<Vec<_>>(),
        [format!("/{JUDGE_WT}")],
        "a preserved worktree wrote a removal"
    );
    // Preserving is a finished decision, not a retry loop.
    assert!(record(&root).complete());
}

#[test]
#[serial]
fn a_clean_worktree_with_a_newer_tip_is_removed_but_its_branch_survives() {
    // The #7812 ladder reads the tip: a branch that moved past the merged
    // head is `not-landed`, so the ref (and the commit only it holds) is kept
    // even though the clean worktree directory is safe to remove.
    let _env = clean_env();
    let fx = Fixture::new();
    let root = fx.workspace("advanced");
    let wt = fx.issue_worktree(&root);
    std::fs::write(wt.join("local.txt"), "work after the merge\n").unwrap();
    git(&wt, &["add", "local.txt"]);
    git(&wt, &["commit", "-q", "-m", "work after the merge"]);
    let newer_tip = git(&wt, &["rev-parse", "HEAD"]);

    tick(&fx, &root);

    assert!(!wt.exists());
    assert_eq!(git(&root, &["rev-parse", BRANCH]), newer_tip, "the advanced commit was lost");
}

#[test]
#[serial]
fn a_branch_discovered_worktree_gets_the_same_disposition() {
    let _env = clean_env();
    let fx = Fixture::new();
    // The convention path is missing; a managed worktree tracking the branch
    // lives elsewhere (#3334 discovery).
    let place = |label: &str| {
        let root = fx.workspace(label);
        let found = fx.base().join(format!("{label}-elsewhere"));
        git(&root, &["worktree", "add", "-q", found.to_str().unwrap(), BRANCH]);
        std::fs::write(found.join(".loom-managed"), "test").unwrap();
        (root, found)
    };
    let (observed, observed_wt) = place("observed-found");
    let (scripted, scripted_wt) = place("scripted-found");

    tick(&fx, &observed);
    script_cleanup(&fx, &scripted);

    assert!(!observed_wt.exists(), "the discovered worktree survived");
    assert!(!scripted_wt.exists());
    let branches = |r: &Path| disposition(r, &[], &[]).branches;
    assert_eq!(branches(&observed), branches(&scripted));
    assert!(!branches(&observed).contains(&format!("refs/heads/{BRANCH}")));
    assert_eq!(mechanisms(&observed), ["merge_observation"]);
    assert_eq!(mechanisms(&scripted), ["merge-pr.sh"]);
    assert_eq!(
        git(&observed, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[test]
#[serial]
fn a_script_cleanup_followed_by_the_observation_writes_nothing_twice() {
    let _env = clean_env();
    let fx = Fixture::new();
    let (root, target) = fx.eligible("script-first");

    script_cleanup(&fx, &root);
    let after_script = ledger(&root);
    assert_eq!(after_script.len(), 2);
    assert!(!target.exists());

    // The tick finds the worktrees, the ref and the target already gone.
    tick(&fx, &root);

    assert_eq!(ledger(&root), after_script, "the replay duplicated a ledger line");
    assert!(record(&root).complete());
    assert_eq!(
        disposition(&root, &[ISSUE_WT, JUDGE_WT], &[&target])
            .registered
            .len(),
        1
    );
}

#[test]
#[serial]
fn a_replayed_observation_and_the_reaper_write_nothing_twice() {
    let _env = clean_env();
    let fx = Fixture::new();
    let (root, target) = fx.eligible("observe-first");

    tick(&fx, &root);
    let after_first = ledger(&root);
    assert_eq!(after_first.len(), 2);
    assert!(!target.exists());

    // A second tick over the same pinned record is a no-op …
    tick(&fx, &root);
    assert_eq!(ledger(&root), after_first);

    // … and so is one that re-runs every pass, as when the record was lost
    // (the passes are convergent; the removals are already done).
    std::fs::remove_file(root.join(".loom/state/merge-bookkeeping/pr-9.json")).unwrap();
    tick(&fx, &root);
    assert_eq!(ledger(&root), after_first, "a re-observed merge duplicated a ledger line");
    assert!(record(&root).complete());

    // The periodic reaper finds nothing left to reap.
    let report = reap(&root);
    assert!(report.removed.is_empty() && report.failed.is_empty(), "{report:?}");
    assert_eq!(ledger(&root), after_first, "the reaper wrote for a removed worktree");
}

#[test]
#[serial]
fn the_reaper_and_the_observation_each_ledger_only_what_they_removed() {
    let _env = clean_env();
    let fx = Fixture::new();
    let (root, _target) = fx.eligible("reap-first");

    // The reaper gets to the Builder's issue worktree first (a merge made on
    // another host, the periodic pass catching it).
    let report = reap(&root);
    assert_eq!(report.removed, vec![42], "{report:?}");
    assert!(!root.join(ISSUE_WT).exists());

    // The observation then finds the issue worktree gone and the Judge's
    // `pr-9` still there: it removes that one and nothing else.
    tick(&fx, &root);
    assert!(!root.join(JUDGE_WT).exists());

    let entries = ledger(&root);
    let by_path: Vec<(String, String)> = entries
        .iter()
        .map(|e| {
            (
                rel(&root, e["worktree"].as_str().unwrap()),
                e["mechanism"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(by_path.len(), 2, "a worktree was ledgered twice or not at all: {by_path:?}");
    assert!(
        by_path.contains(&(format!("/{ISSUE_WT}"), "worktree_reaper".to_string())),
        "{by_path:?}"
    );
    assert!(
        by_path.contains(&(format!("/{JUDGE_WT}"), "merge_observation".to_string())),
        "{by_path:?}"
    );
    assert!(record(&root).complete());
}

#[test]
#[serial]
fn a_failed_removal_keeps_the_merge_record_and_writes_no_false_ledger_line() {
    let _env = clean_env();
    let fx = Fixture::new();
    let root = fx.workspace("failing");
    let issue = fx.issue_worktree(&root);
    let judge = fx.judge_worktree(&root);
    // A locked worktree: `git worktree remove --force` refuses it, and the
    // prune-and-retry cannot clear a lock either — a real teardown failure.
    git(
        &root,
        &[
            "worktree",
            "lock",
            "--reason",
            "test",
            issue.to_str().unwrap(),
        ],
    );

    // The four bookkeeping passes are already recorded for this merge.
    let store = RecordStore::for_root(&root);
    let pinned = MergeRecord {
        repo: "o/r".into(),
        pr: 9,
        head: fx.head.clone(),
        merge_commit: "m1".into(),
        merged_at: MERGED_AT.into(),
        branch: BRANCH.into(),
        body: "Closes #42".into(),
        observed_at: MERGED_AT.into(),
        done: [PARTIAL_RESET, CLOSE_CHECK, BRANCH_CLOSE, STRIP]
            .iter()
            .map(ToString::to_string)
            .collect(),
    };
    store.store(&pinned).unwrap();

    tick(&fx, &root);

    // The failure is isolated: the pass ran, and the Judge's worktree beside
    // the locked one was still removed and ledgered once.
    assert!(!judge.exists(), "the pass never ran");
    assert_eq!(mechanisms(&root), ["merge_observation"]);
    assert!(issue.is_dir(), "a locked worktree was removed");
    assert!(
        git(&root, &["worktree", "list", "--porcelain"]).contains("issue-42"),
        "the registration was dropped"
    );
    assert!(
        ledger(&root)
            .iter()
            .all(|e| !e["worktree"].as_str().unwrap().contains("issue-42")),
        "a removal that failed was ledgered: {:?}",
        ledger(&root)
    );
    // The record is intact: same pinned identity, every earlier pass still
    // marked, and the pass finished (a failed best-effort removal is the
    // reaper's or `loom-clean`'s to recover, not a reason to re-run the merge).
    let after = record(&root);
    assert_eq!(
        (
            &after.repo,
            after.pr,
            &after.head,
            &after.merge_commit,
            &after.merged_at,
            &after.branch
        ),
        (
            &pinned.repo,
            pinned.pr,
            &pinned.head,
            &pinned.merge_commit,
            &pinned.merged_at,
            &pinned.branch
        )
    );
    for pass in &pinned.done {
        assert!(after.done.contains(pass), "{pass} was lost from {:?}", after.done);
    }
    assert!(after.complete());
    let stray: Vec<_> = std::fs::read_dir(root.join(".loom/state/merge-bookkeeping"))
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
        .filter(|name| crate::fs_util::is_staging_name(name))
        .collect();
    assert!(stray.is_empty(), "a temp record was left behind: {stray:?}");
    // The branch is still checked out in the locked worktree, so it stays.
    assert_eq!(git(&root, &["rev-parse", BRANCH]), fx.head);
}
