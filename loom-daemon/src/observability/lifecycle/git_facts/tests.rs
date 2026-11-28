#![allow(clippy::unwrap_used)]

use super::*;
pub(in crate::observability) use crate::test_git_helpers::run as git;

/// A repo whose `origin/main` and `feature/issue-42` tips differ.
pub(in crate::observability) fn repo_with_branch() -> (tempfile::TempDir, String, String) {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q", "-b", "main"]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "base"]);
    let base = git(dir.path(), &["rev-parse", "HEAD"]);
    git(dir.path(), &["update-ref", "refs/remotes/origin/main", &base]);
    git(dir.path(), &["checkout", "-q", "-b", "feature/issue-42"]);
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "work"]);
    let head = git(dir.path(), &["rev-parse", "HEAD"]);
    (dir, base, head)
}

#[test]
fn base_and_head_are_full_oids_from_local_refs() {
    let (dir, base, head) = repo_with_branch();
    assert_ne!(base, head);
    assert_eq!(base_sha(dir.path()), Some(base));
    assert_eq!(head_sha(dir.path(), "42"), Some(head));
}

#[test]
fn an_unresolvable_ref_omits_the_fact() {
    let (dir, _, _) = repo_with_branch();
    assert_eq!(head_sha(dir.path(), "43"), None);
    assert_eq!(head_sha(dir.path(), "../x"), None);
    assert_eq!(head_sha(dir.path(), ""), None);
    let empty = tempfile::tempdir().unwrap();
    assert_eq!(base_sha(empty.path()), None);
}
