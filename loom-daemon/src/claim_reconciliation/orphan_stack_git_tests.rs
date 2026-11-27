//! Tests for the guarded replay both stack reconcilers share (#15529): what
//! [`super::guarded_replay`] publishes and when, on real repositories (a bare
//! "origin" plus a clone, under `tempfile`), so the replay, the #9282 rebase
//! guard and the lease are the real git operations.

use std::path::{Path, PathBuf};

use super::*;
use crate::test_git_helpers::{commit_file, configure, run};

const A: &str = "feature/issue-901";
const B: &str = "feature/issue-902";

/// origin + an "author" clone that builds the stack + the clone the replay
/// runs in, named `repo_dir`.
struct Fixture {
    tmp: tempfile::TempDir,
    origin: PathBuf,
    author: PathBuf,
    repo: PathBuf,
}

impl Fixture {
    fn new(repo_dir: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let author = tmp.path().join("author");
        let repo = tmp.path().join(repo_dir);
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
            tmp,
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

    /// Move origin's main forward with `file`.
    fn advance_main(&self, file: &str, body: &str) -> String {
        run(&self.author, &["checkout", "-q", "main"]);
        let sha = commit_file(&self.author, file, body, &format!("main: {file}"));
        run(&self.author, &["push", "-q", "origin", "main"]);
        sha
    }

    /// The clone the replay runs in, made after the stack exists.
    fn clone_repo(&self) {
        run(
            self.tmp.path(),
            &[
                "clone",
                "-q",
                self.origin.to_str().unwrap(),
                self.repo.to_str().unwrap(),
            ],
        );
        configure(&self.repo);
    }

    /// As the reconcilers do before a replay: the default branch's new tip
    /// is fetched into the clone.
    fn fetch_main(&self) {
        run(&self.repo, &["fetch", "-q", "origin", "main"]);
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
}

/// The stack every outcome is read against: PR 1 at `a`, PR 2 at `b` cut
/// from it with one commit of its own, and a default branch that moved to
/// `onto` after PR 2 was cut. Returns `(a, b, onto)`.
fn stack(fx: &Fixture) -> (String, String, String) {
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "b.txt", "child\n");
    fx.clone_repo();
    let onto = fx.advance_main("m.txt", "main moved\n");
    fx.fetch_main();
    (a, b, onto)
}

/// PR 2's replay of `a..b` onto `onto`, in `scratch`, as [`guarded_replay`]
/// takes it.
fn request<'a>(scratch: &'a Path, a: &'a str, b: &'a str, onto: &'a str) -> ReplayRequest<'a> {
    ReplayRequest {
        pr: 2,
        branch: B,
        head: b,
        onto,
        upstream: a,
        scratch,
        pass: "test stack",
    }
}

#[track_caller]
fn pushed(out: ReplayOutcome) -> String {
    let ReplayOutcome::Pushed(new) = out else {
        panic!("expected a push, got {out:?}");
    };
    new
}

/// The control: a clean replay is published, carrying only the branch's own
/// commit, on the new base.
#[test]
fn a_clean_replay_is_pushed_with_only_the_branchs_own_commits() {
    let fx = Fixture::new("repo");
    let (a, b, onto) = stack(&fx);
    let scratch = scratch_path(&fx.repo, 2);

    let new = pushed(guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto)));

    assert_ne!(new, b, "the replay made a new head");
    assert_eq!(fx.origin_rev(&format!("{B}^")), onto, "B sits on the new base");
    assert_eq!(fx.origin_files(B), ["b.txt", "base.txt", "m.txt"]);
}

/// A remote that moved since its head was listed is never clobbered: the
/// attempt is transient, and the newer head stays.
#[test]
fn a_remote_that_moved_since_its_head_was_listed_is_left_alone() {
    let fx = Fixture::new("repo");
    let (a, b, onto) = stack(&fx);
    // A sibling pushes to B after its head was listed.
    let newer = fx.branch(B, A, 2, "b2.txt", "more\n");
    let scratch = scratch_path(&fx.repo, 2);

    let out = guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto));
    assert_eq!(
        out,
        ReplayOutcome::Transient(format!(
            "the remote branch is at {newer}, not the listed head {b}"
        ))
    );
    assert_eq!(fx.origin_rev(B), newer);

    // A branch the remote no longer has is equally transient.
    run(&fx.author, &["push", "-q", "origin", "--delete", B]);
    let out = guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto));
    assert_eq!(out, ReplayOutcome::Transient(format!("the remote has no branch '{B}'")));
}

/// `install-metadata.json` as a Loom repository's base commit holds it.
fn metadata(commit: &str) -> String {
    format!("{{\n  \"loom_version\": \"1\",\n  \"loom_commit\": \"{commit}\"\n}}\n")
}

/// The rebase guard's `EMPTIED` (#8298, #9001): the child's only change is to
/// a `merge=ours` path that the default branch changed too, so the replay
/// discards it and the replayed head has the default branch's exact tree.
/// The guard refuses it, and nothing is pushed.
#[test]
fn a_replay_the_rebase_guard_empties_is_refused_and_nothing_is_pushed() {
    let fx = Fixture::new("repo");
    std::fs::write(fx.author.join(".gitattributes"), "install-metadata.json merge=ours\n").unwrap();
    commit_file(&fx.author, "install-metadata.json", &metadata("aaaa"), "base: metadata");
    run(&fx.author, &["push", "-q", "origin", "main"]);
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    let b = fx.branch(B, A, 2, "install-metadata.json", &metadata("bbbb"));
    let onto = fx.advance_main(
        "install-metadata.json",
        "{\n  \"loom_version\": \"2\",\n  \"loom_commit\": \"aaaa\"\n}\n",
    );
    fx.clone_repo();
    // The driver the attribute names, as a Loom repository configures it.
    run(&fx.repo, &["config", "merge.ours.driver", "true"]);
    let scratch = scratch_path(&fx.repo, 2);

    let out = guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto));

    assert_eq!(out, ReplayOutcome::Refused(ReplayRefusal::Guard(GuardRefusal::Emptied)));
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
}

/// The rebase guard's `RESOLUTION_DISCARDED` (#9282): the child merged the
/// default branch and, in that merge commit, moved its own fixture off a
/// colliding coordinate by hand. The replay flattens the merge, so the hand
/// resolution is gone: refused, naming the merge, and nothing is pushed.
#[test]
fn a_replay_that_drops_a_hand_resolution_is_refused_and_nothing_is_pushed() {
    let fx = Fixture::new("repo");
    let a = fx.branch(A, "main", 1, "a.txt", "parent\n");
    fx.branch(B, A, 2, "fix_b.txt", "coord=5\n");
    let onto = fx.advance_main("fix_a.txt", "coord=5\n");
    run(&fx.author, &["checkout", "-q", B]);
    run(&fx.author, &["merge", "--no-commit", "main"]);
    let b =
        commit_file(&fx.author, "fix_b.txt", "coord=6\n", "B: merge main, keep the fixtures apart");
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
    fx.clone_repo();
    let scratch = scratch_path(&fx.repo, 2);

    let out = guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto));

    assert_eq!(
        out,
        ReplayOutcome::Refused(ReplayRefusal::Guard(GuardRefusal::ResolutionDiscarded {
            merge: b.clone()
        }))
    );
    assert_eq!(fx.origin_rev(B), b, "nothing pushed");
}

/// A rebase guard that cannot answer does not block the push, as
/// `reconcile-stack.sh` does. Here it cannot quarantine its object writes,
/// because the repository's path contains a `:`.
#[test]
fn an_unanswered_rebase_guard_does_not_block_the_push() {
    let fx = Fixture::new("re:po");
    let (a, b, onto) = stack(&fx);
    let scratch = scratch_path(&fx.repo, 2);

    let new = pushed(guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto)));

    // The premise this test exists for: assessed directly, the same replay
    // gets no answer from the guard.
    match crate::rebase_guard::assess(&fx.repo, &onto, &b, &new) {
        GuardVerdict::Unanswered(reason) => {
            assert!(reason.contains("cannot quarantine"), "{reason}");
        }
        other => panic!("expected no answer from the rebase guard, got {other:?}"),
    }
    assert_eq!(fx.origin_rev(B), new, "pushed unchecked");
    assert_eq!(fx.origin_rev(&format!("{B}^")), onto);
}

/// A push the remote refuses publishes nothing: the attempt is transient, so
/// a later tick retries it from the same head.
#[test]
fn a_failed_push_is_transient_and_publishes_nothing() {
    let fx = Fixture::new("repo");
    let (a, b, onto) = stack(&fx);
    // Reads still reach the remote; only the push does not.
    let unreachable = fx.tmp.path().join("no-such-remote");
    run(
        &fx.repo,
        &[
            "remote",
            "set-url",
            "--push",
            "origin",
            unreachable.to_str().unwrap(),
        ],
    );
    let scratch = scratch_path(&fx.repo, 2);

    let out = guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto));

    let ReplayOutcome::Transient(why) = out else {
        panic!("expected a transient failure, got {out:?}");
    };
    assert!(why.contains("`git push --force-with-lease` exited"), "{why}");
    assert_eq!(fx.origin_rev(B), b, "nothing published");
}

/// The scratch worktree is the one the caller names: a leftover there is
/// cleared for it, and nothing is left behind.
#[test]
fn the_scratch_worktree_is_the_one_the_caller_names() {
    let fx = Fixture::new("repo");
    let (a, b, onto) = stack(&fx);
    let scratch = fx.repo.join(".loom/worktrees/caller-named-pr-2");
    std::fs::create_dir_all(&scratch).unwrap();
    std::fs::write(scratch.join("leftover"), "from a crashed run\n").unwrap();

    let _new = pushed(guarded_replay(&fx.repo, &request(&scratch, &a, &b, &onto)));

    assert!(!scratch.exists(), "used for the replay, then removed");
    assert_eq!(fx.origin_rev(&format!("{B}^")), onto);
}
