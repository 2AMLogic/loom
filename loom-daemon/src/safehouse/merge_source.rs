//! Forge lookups behind the merged-PR report (#13946).
//!
//! Three read-only `gh` questions, each answered from forge truth and each
//! `None` when the forge did not answer (so a caller can tell "nothing merged"
//! from "could not look"):
//!
//! - [`merged_pr_for_issue`] — did the PR on an issue's branch merge?
//! - [`merged_pr_by_number`] — did this PR merge? (a Champion merge step ran on it)
//! - [`merged_prs_between`] — every PR merged in a time window (the captain's
//!   catch-up scan).
//!
//! Every lookup consults the rate-limit breaker first ([`super::forge_gate`])
//! and degrades to `None` on any failure: a missing report is tolerated, a
//! blocked sweep is not.

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::Value;

use super::{forge_gate, GH_BIN_ENV};

/// Bound on one `gh` lookup.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(10);

/// `gh pr list|view --json` fields every lookup requests.
const PR_FIELDS: &str = "number,url,mergedAt,title";

/// Row cap of one windowed scan. A full page means the window may hold more
/// merges than one query read; [`merged_prs_between`] reports that as
/// `saturated` so the caller can narrow the window instead of dropping rows.
pub(super) const SCAN_LIMIT: u32 = 100;

/// A merged PR, as much of it as the report needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MergedPr {
    pub(super) number: u32,
    /// Canonical web URL; also where the `owner/repo` slug comes from.
    pub(super) url: String,
    pub(super) title: Option<String>,
    pub(super) merged_at: DateTime<Utc>,
}

impl MergedPr {
    /// The forge `owner/repo` this PR belongs to, parsed from its URL
    /// (`https://host/owner/repo/pull/N`). `None` for a URL that does not have
    /// that shape: the report's identity is `owner/repo#N`, so a PR whose
    /// repository cannot be named is not reported.
    pub(super) fn repo_slug(&self) -> Option<String> {
        slug_from_pr_url(&self.url)
    }
}

/// `owner/repo` out of a PR URL such as `https://github.com/owner/repo/pull/7`
/// (Gitea uses `/pulls/`). Pure.
pub(super) fn slug_from_pr_url(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let mut segments = rest.split('/').skip(1);
    let owner = segments.next().filter(|s| !s.is_empty())?;
    let repo = segments.next().filter(|s| !s.is_empty())?;
    matches!(segments.next()?, "pull" | "pulls").then(|| format!("{owner}/{repo}"))
}

/// One `gh pr list|view` row as a [`MergedPr`]. `None` for a PR that is not
/// merged (`mergedAt` null or blank — treated as "not merged" rather than
/// reporting a merge on a guess) or that lacks a number or URL. Pure.
pub(super) fn parse_row(row: &Value) -> Option<MergedPr> {
    let merged_at = row
        .get("mergedAt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let merged_at = DateTime::parse_from_rfc3339(merged_at)
        .ok()?
        .with_timezone(&Utc);
    let number = u32::try_from(row.get("number")?.as_u64()?).ok()?;
    let url = row.get("url")?.as_str()?.trim().to_owned();
    if url.is_empty() {
        return None;
    }
    let title = row
        .get("title")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(ToOwned::to_owned);
    Some(MergedPr {
        number,
        url,
        title,
        merged_at,
    })
}

/// The merged PR on `issue`'s branch, if there is one — a sweep that exits
/// cleanly with its PR still open (awaiting a Judge) returns `None`.
pub(super) async fn merged_pr_for_issue(workspace_root: &Path, issue: u32) -> Option<MergedPr> {
    let branch = crate::worktree_ops::naming::branch_name(issue);
    let args = [
        "pr", "list", "--head", &branch, "--state", "merged", "--json", PR_FIELDS, "--limit", "1",
    ];
    let rows = run_json("pr list", "safehouse.merged_pr", workspace_root, &args).await?;
    parse_row(rows.as_array()?.first()?)
}

/// The PR `pr`, if it is merged.
pub(super) async fn merged_pr_by_number(workspace_root: &Path, pr: u32) -> Option<MergedPr> {
    let number = pr.to_string();
    let args = ["pr", "view", &number, "--json", PR_FIELDS];
    let row = run_json("pr view", "safehouse.step_merged_pr", workspace_root, &args).await?;
    parse_row(&row)
}

/// One windowed scan's answer.
#[derive(Debug)]
pub(super) struct Page {
    /// Merged PRs in the window, newest first (the forge's order).
    pub(super) rows: Vec<MergedPr>,
    /// The forge returned a full page: older merges in the window may be hidden.
    pub(super) saturated: bool,
}

/// The `gh pr list` arguments for a scan of PRs merged in `[since, until]`
/// (`until: None` is open-ended). Pure.
pub(super) fn scan_args(since: DateTime<Utc>, until: Option<DateTime<Utc>>) -> Vec<String> {
    let stamp = |t: DateTime<Utc>| t.to_rfc3339_opts(SecondsFormat::Secs, true);
    let search = match until {
        Some(until) => format!("merged:{}..{}", stamp(since), stamp(until)),
        None => format!("merged:>={}", stamp(since)),
    };
    [
        "pr",
        "list",
        "--state",
        "merged",
        "--json",
        PR_FIELDS,
        "--search",
        &search,
        "--limit",
        &SCAN_LIMIT.to_string(),
    ]
    .into_iter()
    .map(ToOwned::to_owned)
    .collect()
}

/// Every PR merged in `[since, until]`. `None` when the forge did not answer —
/// the breaker was cooling, `gh` failed or timed out, or printed something that
/// is not a JSON array.
pub(super) async fn merged_prs_between(
    workspace_root: &Path,
    since: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
) -> Option<Page> {
    let owned = scan_args(since, until);
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    let rows = run_json("pr list (scan)", "safehouse.recent_merged", workspace_root, &args).await?;
    let array = rows.as_array()?;
    let saturated = u32::try_from(array.len()).is_ok_and(|n| n >= SCAN_LIMIT);
    Some(Page {
        rows: array.iter().filter_map(parse_row).collect(),
        saturated,
    })
}

/// Run one `gh` read and parse its stdout as JSON. `None` when the breaker is
/// cooling, `gh` did not run or finish, exited non-zero, or printed non-JSON.
async fn run_json(
    call: &str,
    op: &'static str,
    workspace_root: &Path,
    args: &[&str],
) -> Option<Value> {
    if forge_gate::suppressed(call, workspace_root) {
        return None;
    }
    let gh_bin = super::env_nonempty(GH_BIN_ENV).unwrap_or_else(|| "gh".to_owned());
    let output = run_gh(call, op, &gh_bin, args, workspace_root, LOOKUP_TIMEOUT).await?;
    if !output.status.success() {
        forge_gate::failed(call, workspace_root, &gh_bin, &output.stderr);
        return None;
    }
    log_gh_recovery_once(call, workspace_root);
    serde_json::from_slice(&output.stdout).ok()
}

/// One `gh` read through the facade (#10089: counted in `forge_call_stats` as
/// `op`). The facade runs it in `workspace_root` under that root's owner
/// credential (#6596). `None` when the process never started or never finished;
/// which one is logged once per workspace. A non-zero exit is returned so the
/// caller can inspect `stderr`.
async fn run_gh(
    call: &str,
    op: &'static str,
    gh_bin: &str,
    args: &[&str],
    workspace_root: &Path,
    timeout: Duration,
) -> Option<std::process::Output> {
    use crate::cmd_out::{CmdOutcome, Unavailable};
    use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};
    let inv = GhInvocation::new(Operation::new(op), AccessIntent::Read, GhTarget::None, timeout)
        .program(gh_bin)
        .current_dir(workspace_root)
        .args(args);
    let why = match inv.run_async().await {
        CmdOutcome::Ran(output) => return Some(output),
        CmdOutcome::Unavailable(u) => {
            // Every failure is counted (#13156); the log below stays one-shot.
            crate::observability::ops::safehouse_forge_lookup::record_failed(
                call,
                &crate::host_identity::repo_slug(workspace_root),
                &u,
            );
            match u {
                Unavailable::TimedOut { .. } => format!("timed out after {}s", timeout.as_secs()),
                u => format!("could not run gh: {u}"),
            }
        }
    };
    log_gh_failure_once(call, workspace_root, &why);
    None
}

/// One-per-`(call, workspace)` record of an already-warned forge failure. Every
/// `gh` failure in this module degrades silently by design, which made the
/// #6596 credential gap a half-day diagnosis; this keeps the silence at the
/// *behavior* level while leaving one breadcrumb per workspace in the log.
fn warned_gh_failures() -> &'static Mutex<HashSet<(String, String)>> {
    static WARNED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    WARNED.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Whether this is the first observed failure of `call` in `workspace_root`
/// (and record it). A poisoned mutex degrades to "not the first".
fn first_gh_failure_for(call: &str, workspace_root: &Path) -> bool {
    let key = (call.to_owned(), workspace_root.display().to_string());
    warned_gh_failures()
        .lock()
        .map(|mut seen| seen.insert(key))
        .unwrap_or(false)
}

/// Clear a recorded failure for `(call, workspace_root)` on a **success**
/// (#6619). Returns whether an entry was removed, so a caller can tell "just
/// recovered" from "was healthy".
fn clear_gh_failure_for(call: &str, workspace_root: &Path) -> bool {
    let key = (call.to_owned(), workspace_root.display().to_string());
    warned_gh_failures()
        .lock()
        .map(|mut seen| seen.remove(&key))
        .unwrap_or(false)
}

/// A short, single-line, length-capped excerpt of a `gh` stderr, safe to put in
/// a log line.
pub(super) fn stderr_head(stderr: &[u8]) -> String {
    const MAX: usize = 200;
    let text = String::from_utf8_lossy(stderr);
    let Some(line) = text.lines().map(str::trim).find(|l| !l.is_empty()) else {
        return "no stderr".to_owned();
    };
    if line.chars().count() <= MAX {
        return line.to_owned();
    }
    let head: String = line.chars().take(MAX).collect();
    format!("{head}…")
}

/// Log a forge-lookup failure **once per `(call, workspace)`** at `warn`, and
/// at `debug` on every repeat (#6596): a persistently unauthorized workspace
/// must not flood the log.
pub(super) fn log_gh_failure_once(call: &str, workspace_root: &Path, detail: &str) {
    let root = workspace_root.display();
    if first_gh_failure_for(call, workspace_root) {
        log::warn!(
            "safehouse: `gh {call}` failed in {root} ({detail}); merged-PR reports for this \
             workspace degrade silently — if this is a private repo owned by another org, \
             check its per-owner GH_CONFIG_DIR (further failures here log at debug)"
        );
    } else {
        log::debug!("safehouse: `gh {call}` failed again in {root} ({detail})");
    }
}

/// Log a forge-lookup **recovery** once a `(call, workspace)` pair that
/// previously warned succeeds again (#6619). Silent on every ordinary success.
fn log_gh_recovery_once(call: &str, workspace_root: &Path) {
    if clear_gh_failure_for(call, workspace_root) {
        log::info!(
            "safehouse: `gh {call}` succeeded again in {}; merged-PR reports for this workspace resume",
            workspace_root.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn slug_comes_from_the_pr_url() {
        assert_eq!(
            slug_from_pr_url("https://github.com/2AMLogic/loom/pull/15298").as_deref(),
            Some("2AMLogic/loom")
        );
        assert_eq!(
            slug_from_pr_url("https://git.example.org/acme/widget/pulls/4").as_deref(),
            Some("acme/widget")
        );
    }

    #[test]
    fn slug_rejects_urls_that_are_not_prs() {
        for url in [
            "",
            "github.com/a/b/pull/1",
            "https://github.com/a/b/issues/1",
            "https://github.com/a",
            "https://github.com//b/pull/1",
        ] {
            assert_eq!(slug_from_pr_url(url), None, "{url:?}");
        }
    }

    #[test]
    fn parse_row_reads_a_merged_pr() {
        let row = json!({
            "number": 7,
            "url": "https://github.com/a/b/pull/7",
            "mergedAt": "2026-11-25T10:00:00Z",
            "title": "  Fix the thing  ",
        });
        let pr = parse_row(&row).unwrap();
        assert_eq!(pr.number, 7);
        assert_eq!(pr.title.as_deref(), Some("Fix the thing"));
        assert_eq!(pr.merged_at, ts("2026-11-25T10:00:00Z"));
        assert_eq!(pr.repo_slug().as_deref(), Some("a/b"));
    }

    #[test]
    fn parse_row_refuses_an_unmerged_or_incomplete_pr() {
        let base = json!({"number": 7, "url": "https://github.com/a/b/pull/7"});
        assert!(parse_row(&base).is_none(), "no mergedAt");
        let mut open = base.clone();
        open["mergedAt"] = Value::Null;
        assert!(parse_row(&open).is_none(), "null mergedAt is not a merge");
        let mut blank = base.clone();
        blank["mergedAt"] = json!("  ");
        assert!(parse_row(&blank).is_none(), "blank mergedAt is not a merge");
        let mut no_url = json!({"number": 7, "mergedAt": "2026-11-25T10:00:00Z"});
        assert!(parse_row(&no_url).is_none(), "no url");
        no_url["url"] = json!("");
        assert!(parse_row(&no_url).is_none(), "empty url");
    }

    #[test]
    fn a_pr_without_a_title_is_still_a_merge() {
        let row = json!({
            "number": 9, "url": "https://github.com/a/b/pull/9",
            "mergedAt": "2026-11-25T10:00:00Z", "title": "",
        });
        assert_eq!(parse_row(&row).unwrap().title, None);
    }

    #[test]
    fn scan_args_bound_the_window() {
        let since = ts("2026-11-25T04:00:00Z");
        let open = scan_args(since, None);
        assert!(open.contains(&"merged:>=2026-11-25T04:00:00Z".to_owned()));
        assert!(open.contains(&SCAN_LIMIT.to_string()));
        let until = ts("2026-11-25T09:30:00Z");
        let ranged = scan_args(since, Some(until));
        assert!(ranged.contains(&"merged:2026-11-25T04:00:00Z..2026-11-25T09:30:00Z".to_owned()));
        assert_eq!(&ranged[..4], ["pr", "list", "--state", "merged"]);
    }

    #[test]
    fn stderr_head_is_one_capped_line() {
        assert_eq!(stderr_head(b""), "no stderr");
        assert_eq!(stderr_head(b"\n  boom  \nsecond"), "boom");
        let long = "x".repeat(300);
        let head = stderr_head(long.as_bytes());
        assert_eq!(head.chars().count(), 201);
        assert!(head.ends_with('…'));
    }
}
