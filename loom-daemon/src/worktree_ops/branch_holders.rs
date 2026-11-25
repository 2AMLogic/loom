//! Branch -> holding-worktree map shared by every branch-deletion site (#10766).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Branch -> the worktree that holds it (issue #10766). One shared helper for
/// every branch-deletion site, so "a kept worktree still has this branch" is
/// decided in one place.
///
/// A worktree holds a branch when it has it checked out (`branch refs/heads/x`
/// in `git worktree list --porcelain`) **or** when it is mid-rebase / mid-bisect:
/// git reports such a worktree as `detached`, yet `git branch -D` still refuses
/// the branch ("used by worktree at ..."), because the rebase will move it back
/// on completion. That detached-but-held case is what failed `loom-fleet-clean`
/// on robb-studio (gf180-trng `pr-161`, loom `pr-7904` / `pr-10252`). A
/// `git rebase --update-refs` also holds every other branch of the stack it
/// will rewrite (`rebase-merge/update-refs`, #10851).
pub(crate) fn branch_holders(repo_root: &Path) -> HashMap<String, PathBuf> {
    branch_holders_checked(repo_root).unwrap_or_default()
}

/// [`branch_holders`], but `None` when holder state cannot be established
/// (git failed, or a worktree admin directory is unreadable). Deletion sites
/// that bypass git's own "used by worktree" refusal (`update-ref -d`) must
/// keep the branch on `None` rather than treat it as "no holders".
///
/// Rebase / bisect state is read from the common git directory's
/// `worktrees/<id>/` admin dirs (plus the main git dir), not by running git
/// inside each checkout, so a locked or offline linked worktree still holds
/// its branch (it is the same state git's own refusal consults).
pub(crate) fn branch_holders_checked(repo_root: &Path) -> Option<HashMap<String, PathBuf>> {
    let mut holders = HashMap::new();
    let out = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut current: Option<PathBuf> = None;
    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            let path = PathBuf::from(p.trim());
            for b in in_progress_branches(&path) {
                holders.entry(b).or_insert_with(|| path.clone());
            }
            current = Some(path);
        } else if let Some(name) = line.strip_prefix("branch refs/heads/") {
            if let Some(path) = &current {
                holders.insert(name.trim().to_string(), path.clone());
            }
        }
    }
    let common = common_git_dir(repo_root)?;
    for (dir, owner) in admin_dirs(&common)? {
        let owner = owner.unwrap_or_else(|| dir.clone());
        for b in in_progress_branches_in(&dir) {
            holders.entry(b).or_insert_with(|| owner.clone());
        }
    }
    Some(holders)
}

/// Absolute common git directory of the repository at `repo_root`.
fn common_git_dir(repo_root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    Some(if Path::new(&raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        repo_root.join(raw)
    })
}

/// Every git-state directory of the repository: the common dir itself and each
/// `<common>/worktrees/<id>/`, paired with the checkout path recorded in its
/// `gitdir` file when readable. `None` when the `worktrees` dir exists but
/// cannot be listed.
fn admin_dirs(common: &Path) -> Option<Vec<(PathBuf, Option<PathBuf>)>> {
    let mut dirs = vec![(common.to_path_buf(), None)];
    match std::fs::read_dir(common.join("worktrees")) {
        Ok(rd) => {
            for entry in rd {
                let dir = entry.ok()?.path();
                if !dir.is_dir() {
                    continue;
                }
                let owner = std::fs::read_to_string(dir.join("gitdir"))
                    .ok()
                    .and_then(|c| Path::new(c.trim()).parent().map(Path::to_path_buf));
                dirs.push((dir, owner));
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return None,
    }
    Some(dirs)
}

/// Branches a worktree is holding while "detached": the branch being rebased
/// (`rebase-merge/head-name`, `rebase-apply/head-name`), the other branches a
/// `--update-refs` rebase will move (`rebase-merge/update-refs`; the apply
/// backend does not support `--update-refs`), and a bisect's start branch
/// (`BISECT_START`). Mirrors git's `branch.c:prepare_checked_out_branches`.
fn in_progress_branches(worktree: &Path) -> Vec<String> {
    let Ok(out) = Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(worktree)
        .output()
    else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let git_dir = if Path::new(&raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        worktree.join(raw)
    };
    in_progress_branches_in(&git_dir)
}

/// [`in_progress_branches`] for an already-resolved git state directory.
fn in_progress_branches_in(git_dir: &Path) -> Vec<String> {
    let mut names = Vec::new();
    for rel in ["rebase-merge/head-name", "rebase-apply/head-name"] {
        if let Ok(c) = std::fs::read_to_string(git_dir.join(rel)) {
            if let Some(b) = c.trim().strip_prefix("refs/heads/") {
                names.push(b.to_string());
            }
        }
    }
    if let Ok(c) = std::fs::read_to_string(git_dir.join("rebase-merge/update-refs")) {
        names.extend(update_refs_branches(&c));
    }
    if let Ok(c) = std::fs::read_to_string(git_dir.join("BISECT_START")) {
        let c = c.trim();
        if !c.is_empty() {
            names.push(c.strip_prefix("refs/heads/").unwrap_or(c).to_string());
        }
    }
    names
}

/// Local branch names in a `rebase-merge/update-refs` file. Each record is
/// three lines (refname, before-OID, after-OID), as written by git's
/// `sequencer.c:write_update_refs_state`; like git's own reader, take every
/// third line rather than guessing which lines look like OIDs.
fn update_refs_branches(contents: &str) -> Vec<String> {
    contents
        .lines()
        .step_by(3)
        .filter_map(|l| l.trim().strip_prefix("refs/heads/"))
        .map(str::to_string)
        .collect()
}

/// The `kept:` log line for a branch skipped because a worktree holds it.
#[must_use]
pub(crate) fn kept_line(worktree: &Path, branch: &str) -> String {
    format!("kept: worktree {} holds {branch}", worktree.display())
}

/// If `branch` is held by a worktree, log the `kept:` line and return `true`
/// (the caller must skip the deletion). Probes the worktrees afresh; a caller
/// deciding many branches should build [`branch_holders`] once and use
/// [`skip_if_held_in`].
pub(crate) fn skip_if_held(repo_root: &Path, branch: &str) -> bool {
    skip_if_held_in(&branch_holders(repo_root), branch)
}

/// [`skip_if_held`] against an already-computed holder map.
pub(crate) fn skip_if_held_in(holders: &HashMap<String, PathBuf>, branch: &str) -> bool {
    match holders.get(branch) {
        Some(path) => {
            println!("  {}", kept_line(path, branch));
            true
        }
        None => false,
    }
}

/// Worktree path from git's "cannot delete branch 'x' used by worktree at
/// '<path>'" refusal, if that is what `cause` is. Backstop for holders the
/// porcelain/state-file probe did not see.
pub(super) fn held_by_worktree(cause: &str) -> Option<PathBuf> {
    let rest = cause.split("used by worktree at '").nth(1)?;
    Some(PathBuf::from(rest.split('\'').next()?))
}

#[cfg(test)]
#[path = "branch_holders_tests.rs"]
mod tests;
