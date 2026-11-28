//! Git utilities: thin local `git` subprocess helpers and branch diff
//! statistics.
//!
//! Shared runners (`git_run`, `get_current_commit`, `is_git_repo`) plus the
//! `git diff --numstat` probes that size a worktree's own changes against its
//! mainline — the aggregate lines-added/lines-removed pair and the per-path
//! rows sweep-outcome telemetry records. Every probe is a local `git`
//! invocation inside the worktree; none calls the forge.

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Run `git -C <dir> <args>` and return the raw [`Output`].
///
/// Shared runner for the private `git(dir, args)` wrappers: stdin is null,
/// stdout and stderr are piped, and there is no timeout or env tweaking.
/// Callers keep a thin adapter for their own return shape (decode/trim).
pub fn git_run(dir: &Path, args: &[&str]) -> std::io::Result<Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
}

/// Capture the current HEAD commit hash
pub fn get_current_commit(working_dir: &Path) -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(working_dir)
        .output()
        .ok()?;

    if output.status.success() {
        let commit = String::from_utf8(output.stdout).ok()?.trim().to_string();
        if commit.is_empty() {
            None
        } else {
            Some(commit)
        }
    } else {
        None
    }
}

/// Check if a directory is a git repository
pub fn is_git_repo(working_dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .current_dir(working_dir)
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Local lines-added/lines-removed for `working_dir`'s current branch against
/// `base_ref`'s common ancestor with `HEAD` (Issue #5357), via `git
/// merge-base` and `git diff --numstat` — never a forge API call. This is
/// the same numstat probe [`diff_stat_against_mainline`] runs, for a caller
/// that already knows which ref to diff against.
///
/// `None` when `working_dir` is not a git repo, `base_ref` cannot be
/// resolved to a common ancestor with `HEAD` (unknown ref, shallow clone,
/// no shared history), or the diff itself fails — never a fabricated zero
/// for an unprobeable worktree.
#[must_use]
pub fn diff_stat_since_merge_base(working_dir: &Path, base_ref: &str) -> Option<(i64, i64)> {
    if !is_git_repo(working_dir) {
        return None;
    }
    let merge_base_output = Command::new("git")
        .args(["merge-base", "HEAD", base_ref])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !merge_base_output.status.success() {
        return None;
    }
    let merge_base = String::from_utf8(merge_base_output.stdout)
        .ok()?
        .trim()
        .to_string();
    if merge_base.is_empty() {
        return None;
    }
    Some(sum_diff_rows(&diff_rows_between(working_dir, &merge_base, "HEAD")?))
}

/// Refs likely to name `repo`'s mainline, in priority order (Issue #5357):
/// `origin/<default_branch>` when `origin/HEAD` resolves (the actual base a
/// Loom worktree branched from — see `worktree.sh`'s `loom_default_branch`),
/// then the common hardcoded names as a fallback for a repo with no
/// configured remote (a bare local clone, or this module's own tests) so the
/// LOC probe degrades gracefully rather than requiring `git remote
/// set-head`. Order matters: the first candidate `git merge-base` can
/// resolve against `HEAD` wins.
fn mainline_candidates(repo: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(branch) = crate::worktree_ops::clean::default_branch(repo) {
        out.push(format!("origin/{branch}"));
    }
    for candidate in ["origin/main", "origin/master", "main", "master"] {
        if !out.iter().any(|c| c == candidate) {
            out.push(candidate.to_string());
        }
    }
    out
}

/// Local lines-added/lines-removed for `working_dir`'s current branch
/// against its mainline's merge base (Issue #5357) — never a forge API call.
/// Tries [`mainline_candidates`] in order and uses the first ref `git
/// merge-base` can resolve against `HEAD`.
///
/// `None` when `working_dir` is not a git repo or no candidate ref resolves
/// (e.g. a shallow clone with no shared history) — never a fabricated zero
/// for an unprobeable worktree. This is the entry point sweep-outcome
/// telemetry capture calls; [`diff_stat_since_merge_base`] is exposed
/// separately for a caller that already knows which ref to diff against.
#[must_use]
pub fn diff_stat_against_mainline(working_dir: &Path) -> Option<(i64, i64)> {
    if !is_git_repo(working_dir) {
        return None;
    }
    let merge_base = merge_base_with_mainline(working_dir)?;
    Some(sum_diff_rows(&diff_rows_between(working_dir, &merge_base, "HEAD")?))
}

/// The merge base of `working_dir`'s `HEAD` with its mainline — the first
/// candidate ref from [`mainline_candidates`] that `git merge-base` resolves
/// against `HEAD`. `None` when no candidate resolves (shallow clone, no
/// shared history).
fn merge_base_with_mainline(working_dir: &Path) -> Option<String> {
    for base_ref in mainline_candidates(working_dir) {
        let merge_base_output = Command::new("git")
            .args(["merge-base", "HEAD", &base_ref])
            .current_dir(working_dir)
            .output()
            .ok()?;
        if merge_base_output.status.success() {
            let merge_base = String::from_utf8(merge_base_output.stdout)
                .ok()?
                .trim()
                .to_string();
            if !merge_base.is_empty() {
                return Some(merge_base);
            }
        }
    }
    None
}

/// One `git diff --numstat` row: per-path add/delete counts, the unit every
/// numstat probe in this module parses and the shape
/// [`diff_rows_against_mainline`] returns so a caller can classify paths
/// (Issue #9466's generated vs hand-written split). Binary rows
/// (`-\t-\tpath`) are skipped: they carry no line counts to classify.
#[derive(Debug, Clone, PartialEq)]
pub struct GitDiffRow {
    pub added: i64,
    pub deleted: i64,
    pub path: String,
}

/// Local per-path numstat for `working_dir`'s current branch against its
/// mainline merge base (Issue #9466) — the per-path sibling of
/// [`diff_stat_against_mainline`], same mainline resolution and same
/// never-a-fabricated-zero contract (`None` when the worktree is absent,
/// not a git repo, or no base resolves). Never a forge API call.
///
/// # Panics
/// Never: a malformed numstat row is skipped, not unwrapped.
#[must_use]
pub fn diff_rows_against_mainline(working_dir: &Path) -> Option<Vec<GitDiffRow>> {
    if !is_git_repo(working_dir) {
        return None;
    }
    let merge_base = merge_base_with_mainline(working_dir)?;
    diff_rows_between(working_dir, &merge_base, "HEAD")
}

/// Parse `git diff --numstat` stdout into one [`GitDiffRow`] per row.
///
/// A row is skipped, never unwrapped, when it carries no usable counts:
/// binary rows (`-\t-\tpath`), rows with fewer than three tab-separated
/// columns, and rows whose count columns are not numbers.
#[must_use]
fn parse_numstat_rows(stdout: &str) -> Vec<GitDiffRow> {
    stdout
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() < 3 {
                return None;
            }
            // Renames render as `added\tdeleted\told => new` (or with a
            // `{prefix => suffix}` brace form); classify the post-image
            // path, which is what a landing diff reports.
            let added = parts[0].parse::<i64>().ok()?;
            let deleted = parts[1].parse::<i64>().ok()?;
            Some(GitDiffRow {
                added,
                deleted,
                path: normalize_diff_path(parts[2]),
            })
        })
        .collect()
}

/// Local `git diff --numstat <before> <after>` for `working_dir`, parsed
/// through [`parse_numstat_rows`].
///
/// `None` when `git` cannot be run, exits non-zero (an unknown ref, or a
/// directory that is not a git repo), or prints output that is not UTF-8.
#[must_use]
fn diff_rows_between(working_dir: &Path, before: &str, after: &str) -> Option<Vec<GitDiffRow>> {
    let output = Command::new("git")
        .args(["diff", "--numstat", before, after])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    Some(parse_numstat_rows(&stdout))
}

/// Sum a numstat row set into the `(lines_added, lines_removed)` pair the
/// `diff_stat_*` probes return. An empty row set is `(0, 0)`: a real zero,
/// the same as a diff of `HEAD` against itself.
#[must_use]
fn sum_diff_rows(rows: &[GitDiffRow]) -> (i64, i64) {
    rows.iter()
        .fold((0, 0), |(added, deleted), row| (added + row.added, deleted + row.deleted))
}

/// The post-image path of a numstat path column: plain paths pass through;
/// `old => new` renames keep `new`; `{a => b}/rest` brace forms reassemble
/// to `b/rest`.
#[must_use]
fn normalize_diff_path(raw: &str) -> String {
    if let Some((head, new)) = raw.rsplit_once(" => ") {
        if let Some(path) = brace_form_post_image(head, new) {
            return path;
        }
        return new.to_string();
    }
    raw.to_string()
}

/// The post-image path of the brace form git writes for a rename whose old
/// and new path share a leading and/or trailing part, `pfx{a => b}sfx`, given
/// the row's two halves around ` => `: `head` is `pfx{a` and `new` is `b}sfx`.
///
/// `None` when the pair is not that form, which a plain rename resembles when
/// a file name itself holds a brace (`a{b.txt => c}d.txt`): git's braces sit
/// on path-component boundaries, so the prefix is empty or ends in `/` and
/// the suffix is empty or starts with `/`.
fn brace_form_post_image(head: &str, new: &str) -> Option<String> {
    let open = head.rfind('{')?;
    let close = new.find('}')?;
    let prefix = &head[..open];
    let new_mid = &new[..close];
    let suffix = &new[close + 1..];
    if !(prefix.is_empty() || prefix.ends_with('/')) {
        return None;
    }
    if !(suffix.is_empty() || suffix.starts_with('/')) {
        return None;
    }
    if new_mid.is_empty() {
        // `pfx{a => }sfx`: the prefix and the suffix share one slash.
        let suffix = suffix.strip_prefix('/').unwrap_or(suffix);
        return Some(format!("{prefix}{suffix}"));
    }
    Some(format!("{prefix}{new_mid}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::parallel;

    // --- diff_stat_since_merge_base / diff_stat_against_mainline (#5357) ---
    //
    // Issue #6177: these tests were observed flaking under `cargo test`'s
    // default parallel-thread execution (2 then 3 failures on the *same*
    // commit across two consecutive full-suite runs), always passing in
    // isolation. The shared resource is not anything in this module — every
    // `git` invocation below already pins its own `current_dir` per-`Command`
    // — it is the **process-global environment** (`std::env::set_var` /
    // `remove_var`), which dozens of tests elsewhere in this crate's single
    // test binary mutate concurrently (most heavily `LOOM_CONFIG_DEFAULTS_FILE`,
    // serialized crate-wide under the `loom_config_env` `#[serial]` key —
    // see `config_resolver.rs`). Concurrently forking a subprocess
    // (`Command::spawn`, which every test here does, some several times) while
    // another thread calls `std::env::set_var`/`remove_var` is a documented
    // Rust unsoundness (std's own `env::set_var` docs, rust-lang/rust#27970):
    // `environ` can be read mid-mutation by the fork, corrupting what the
    // child process inherits. `#[parallel(loom_config_env)]` below is
    // `serial_test`'s documented mechanism for exactly this: it lets these
    // tests keep running in parallel with each other and with ordinary tests,
    // while guaranteeing none of them ever overlaps a `loom_config_env`
    // `#[serial]` test (the single largest env-mutation lock domain in this
    // crate) — closing the race window instead of just hiding the symptom by
    // serializing this module against itself.
    use crate::test_git_helpers::run as git;

    /// A repo with `main` at one commit and a `feature` branch (checked out)
    /// carrying one additional commit that adds `new.txt` (3 lines) and
    /// appends one line to `README.md` — no `origin` remote configured, the
    /// common shape for a bare local test fixture (and for a Loom worktree
    /// whose remote fetch failed).
    fn feature_branch_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        std::fs::write(dir.path().join("README.md"), "line1\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "seed"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.path().join("README.md"), "line1\nline2\n").unwrap();
        std::fs::write(dir.path().join("new.txt"), "a\nb\nc\n").unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "feature work"]);
        dir
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_rows_against_mainline_reports_the_post_image_path_of_a_rename() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        // The production call runs a plain `git diff`, which reads the host
        // config: pin rename detection so the row cannot turn into a
        // delete/add pair on a host that disabled it.
        git(dir.path(), &["config", "diff.renames", "true"]);
        std::fs::create_dir_all(dir.path().join("src/loom_ui_alerts")).unwrap();
        std::fs::write(dir.path().join("src/loom_ui_alerts/ledger.rs"), "pub fn ledger() {}\n")
            .unwrap();
        git(dir.path(), &["add", "-A"]);
        git(dir.path(), &["commit", "-q", "-m", "seed"]);
        git(dir.path(), &["checkout", "-q", "-b", "feature"]);
        std::fs::create_dir_all(dir.path().join("src/alerts")).unwrap();
        git(dir.path(), &["mv", "src/loom_ui_alerts/ledger.rs", "src/alerts/ledger.rs"]);
        git(dir.path(), &["commit", "-q", "-m", "move the ledger"]);
        // git prints the pure rename as `src/{loom_ui_alerts => alerts}/ledger.rs`.
        let rows =
            diff_rows_against_mainline(dir.path()).expect("bare `main` fallback must resolve");
        assert_eq!(
            rows,
            vec![GitDiffRow {
                added: 0,
                deleted: 0,
                path: "src/alerts/ledger.rs".to_string(),
            }]
        );
    }

    // --- normalize_diff_path ---

    #[test]
    fn normalize_diff_path_passes_a_plain_path_through() {
        assert_eq!(normalize_diff_path("src/lib.rs"), "src/lib.rs");
    }

    #[test]
    fn normalize_diff_path_keeps_the_new_side_of_a_plain_rename() {
        assert_eq!(normalize_diff_path("old/name.rs => new/name.rs"), "new/name.rs");
    }

    #[test]
    fn normalize_diff_path_reassembles_a_brace_form_rename() {
        assert_eq!(normalize_diff_path("src/{a => b}/mod.rs"), "src/b/mod.rs");
        assert_eq!(normalize_diff_path("{a => b}"), "b");
        assert_eq!(normalize_diff_path("src/{top.txt => moved.txt}"), "src/moved.txt");
    }

    #[test]
    fn normalize_diff_path_reassembles_this_repositorys_ledger_rename() {
        // `git diff --numstat -M bd0efeb6f^ bd0efeb6f` in this repository.
        assert_eq!(
            normalize_diff_path("loom-daemon/src/{loom_ui_alerts => alerts}/ledger.rs"),
            "loom-daemon/src/alerts/ledger.rs"
        );
    }

    #[test]
    fn normalize_diff_path_handles_an_empty_brace_side() {
        assert_eq!(normalize_diff_path("src/{ => b}/mod.rs"), "src/b/mod.rs");
        // The prefix and the suffix share the slash before `mod.rs`.
        assert_eq!(normalize_diff_path("src/{a => }/mod.rs"), "src/mod.rs");
    }

    #[test]
    fn normalize_diff_path_keeps_a_stray_brace_in_a_plain_rename() {
        assert_eq!(normalize_diff_path("a{b}c.txt => d.txt"), "d.txt");
        assert_eq!(normalize_diff_path("a{b.txt => c}d.txt"), "c}d.txt");
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_since_merge_base_counts_added_and_removed_lines() {
        let dir = feature_branch_repo();
        let stats = diff_stat_since_merge_base(dir.path(), "main")
            .expect("merge-base against a real sibling branch must resolve");
        // new.txt: +3, README.md: +1 (append-only, nothing removed).
        assert_eq!(stats, (4, 0));
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_since_merge_base_is_none_for_an_unresolvable_ref() {
        let dir = feature_branch_repo();
        assert_eq!(diff_stat_since_merge_base(dir.path(), "origin/main"), None);
        assert_eq!(diff_stat_since_merge_base(dir.path(), "not-a-real-ref"), None);
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_since_merge_base_is_none_outside_a_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(diff_stat_since_merge_base(dir.path(), "main"), None);
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_against_mainline_falls_back_to_bare_branch_names_with_no_remote() {
        let dir = feature_branch_repo();
        // No `origin` remote at all, so `default_branch` resolves to `None`
        // and every `origin/*` candidate fails to resolve — the fallback to
        // the bare `main` candidate must still find the same diff.
        let stats =
            diff_stat_against_mainline(dir.path()).expect("bare `main` fallback must resolve");
        assert_eq!(stats, (4, 0));
    }

    #[test]
    #[parallel(loom_config_env)]
    fn diff_stat_against_mainline_is_none_when_head_equals_mainline() {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init", "-q", "--initial-branch=main"]);
        git(dir.path(), &["config", "user.email", "loom@example.com"]);
        git(dir.path(), &["config", "user.name", "Loom Test"]);
        git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "seed"]);
        // HEAD *is* main, so the merge-base diff is empty — zero lines
        // changed, a legitimate `Some((0, 0))`, not an unprobeable `None`.
        assert_eq!(diff_stat_against_mainline(dir.path()), Some((0, 0)));
    }

    // --- parse_numstat_rows / sum_diff_rows ---
    //
    // Pure functions over captured numstat text: no `git` subprocess is
    // forked, so the `loom_config_env` fencing above does not apply.

    #[test]
    fn parse_numstat_rows_yields_one_row_per_path() {
        let rows = parse_numstat_rows("10\t5\tsrc/main.rs\n3\t1\tsrc/lib.rs\n");
        assert_eq!(
            rows,
            vec![
                GitDiffRow {
                    added: 10,
                    deleted: 5,
                    path: "src/main.rs".to_string(),
                },
                GitDiffRow {
                    added: 3,
                    deleted: 1,
                    path: "src/lib.rs".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_numstat_rows_skips_binary_and_malformed_rows() {
        // A binary file carries `-` in both count columns.
        let rows = parse_numstat_rows("-\t-\timage.png\n10\t5\tsrc/main.rs\n");
        assert_eq!(
            rows,
            vec![GitDiffRow {
                added: 10,
                deleted: 5,
                path: "src/main.rs".to_string(),
            }]
        );
        // Fewer than three tab-separated columns is not a numstat row, and a
        // three-column row still needs numeric counts.
        assert!(parse_numstat_rows("1\t2\n").is_empty());
        assert!(parse_numstat_rows("x\ty\tsrc/main.rs\n").is_empty());
    }

    #[test]
    fn parse_numstat_rows_reports_renames_under_the_post_image_path() {
        let rows = parse_numstat_rows("1\t1\told.rs => new.rs\n2\t0\tsrc/main.rs\n");
        assert_eq!(
            rows,
            vec![
                GitDiffRow {
                    added: 1,
                    deleted: 1,
                    path: "new.rs".to_string(),
                },
                GitDiffRow {
                    added: 2,
                    deleted: 0,
                    path: "src/main.rs".to_string(),
                },
            ]
        );
    }

    #[test]
    fn parse_numstat_rows_is_empty_for_empty_output() {
        assert!(parse_numstat_rows("").is_empty());
    }

    #[test]
    fn sum_diff_rows_totals_every_rows_counts() {
        let rows = parse_numstat_rows("10\t5\tsrc/main.rs\n-\t-\timage.png\n3\t1\tsrc/lib.rs\n");
        // The binary row is absent from `rows`, so it adds nothing.
        assert_eq!(sum_diff_rows(&rows), (13, 6));
        assert_eq!(sum_diff_rows(&[]), (0, 0));
    }
}
