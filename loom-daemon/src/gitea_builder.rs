//! Live Gitea evidence for the Builder's claim and split-parent gates.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{anyhow, ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::comment_trust::{allowlist_from_config, trusted_by, Author};
use crate::forge_check_claim::{classify_claim_labels, lease_leg, LabelLeg, LeaseLeg, LeaseRow};
use crate::forge_cmd::GiteaConfig;
use crate::parent_link::{parent_marker, split_state_from_children, SplitState};
use crate::worktree_ops::gh::{
    linkage_phrase_regex, merged_since, MergedBranchPrProbe, OpenPrProbe,
};
use crate::write_scope::probe::{gitea_connection, gitea_get_counted};

pub(crate) struct Repository {
    connection: GiteaConfig,
    slug: String,
}

impl Repository {
    pub(crate) fn resolve(root: &Path, explicit: Option<&str>) -> Result<Self> {
        let slug = crate::forge_etag_store::resolve_target(Some(root), explicit)
            .repo
            .filter(|s| crate::forge_slug::valid_slug(s))
            .context("cannot resolve Gitea owner/repo")?;
        let connection = gitea_connection(root).map_err(anyhow::Error::msg)?;
        Ok(Self { connection, slug })
    }

    fn get(&self, path: &str) -> Result<(Value, Option<usize>)> {
        let (status, body, total) =
            gitea_get_counted(&self.connection, &format!("repos/{}/{path}", self.slug))
                .map_err(anyhow::Error::msg)?;
        ensure!(status == 200, "Gitea {path} returned HTTP {status}");
        let row = serde_json::from_str(&body)
            .with_context(|| format!("invalid JSON from Gitea {path}"))?;
        Ok((row, total))
    }

    /// A server may cap the requested limit. Walk to an empty page and reject
    /// overlapping pages (including a server ignoring the page parameter).
    fn each_page(
        &self,
        path: &str,
        key: &str,
        mut visit: impl FnMut(&Value) -> Result<()>,
    ) -> Result<()> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut seen = HashSet::new();
        let mut expected = None;
        for page in 1u32.. {
            let (response, count) = self.get(&format!("{path}{separator}limit=50&page={page}"))?;
            let rows = response
                .as_array()
                .with_context(|| format!("expected a Gitea {path} listing"))?;
            if let Some(count) = count {
                ensure!(
                    expected.is_none_or(|n| n == count),
                    "Gitea {path} count changed while reading"
                );
                expected = Some(count);
            }
            if rows.is_empty() {
                ensure!(
                    expected.is_none_or(|n| n == seen.len()),
                    "incomplete Gitea {path} listing"
                );
                return Ok(());
            }
            for row in rows {
                record_identity(&mut seen, row, key, path)?;
                visit(row)?;
            }
        }
        anyhow::bail!("Gitea {path} never reached its last page")
    }

    fn issue(&self, number: u32) -> Result<Value> {
        let (row, _) = self.get(&format!("issues/{number}"))?;
        ensure!(issue_number(&row)? == number, "Gitea returned the wrong issue");
        Ok(row)
    }

    pub(crate) fn split_state(&self, parent: u32) -> Result<SplitState> {
        let parent_issue = self.issue(parent)?;
        ensure!(!is_pull(&parent_issue)?, "split parent is a pull request");
        let marker = parent_marker(parent);
        let mut children = Vec::new();
        // Gitea 1.24 has dependency and blocker edges but no native sub-issue
        // relationship. Only Loom's parent marker identifies a child.
        self.each_page("issues?state=all&type=issues", "number", |row| {
            let number = issue_number(row)?;
            let closed = closed(row)?;
            if number != parent && !is_pull(row)? && body(row)?.contains(&marker) {
                children.push((number, closed));
            }
            Ok(())
        })?;
        split_state_from_children(&children, || body(&parent_issue))
    }

    pub(crate) fn labels(&self, number: u32, owned: bool) -> Result<LabelLeg> {
        let row = self.issue(number)?;
        if is_pull(&row)? {
            return Ok(LabelLeg::IsPullRequest);
        }
        if closed(&row)? {
            return Ok(LabelLeg::Closed);
        }
        let labels = row["labels"]
            .as_array()
            .context("missing issue labels")?
            .iter()
            .map(|l| Ok(string(l, "name")?.to_owned()))
            .collect::<Result<Vec<_>>>()?;
        Ok(classify_claim_labels(&labels, owned))
    }

    /// The same pull listing supplies both PR legs. Inspect promotion history
    /// only if an unlinked, merged PR from the repository's issue branch exists.
    pub(crate) fn pulls(
        &self,
        number: u32,
        trust: &mut TrustedAuthors,
    ) -> Result<(OpenPrProbe, MergedBranchPrProbe)> {
        let reference = linkage_phrase_regex(number).context("invalid PR linkage expression")?;
        let branch = format!("feature/issue-{number}");
        let mut open: Option<u32> = None;
        let mut merged = Vec::new();
        self.each_page("pulls?state=all", "number", |row| {
            let pr = issue_number(row)?;
            let linked = reference.is_match(body(row)?);
            if !closed(row)? {
                let blocks = if linked {
                    self.same_head_repo(row)? || trust.contains(self, row)?
                } else {
                    self.matches_head_branch(row, &branch)?
                };
                if blocks {
                    open = Some(open.map_or(pr, |old| old.min(pr)));
                }
            } else if !linked
                && row["merged"].as_bool().context("missing PR merge state")?
                && self.matches_head_branch(row, &branch)?
            {
                merged.push((pr, string(row, "merged_at")?.to_owned()));
            }
            Ok(())
        })?;
        if let Some(pr) = open {
            return Ok((OpenPrProbe::Open(pr), MergedBranchPrProbe::NoneMerged));
        }
        if merged.is_empty() {
            return Ok((OpenPrProbe::NoneOpen, MergedBranchPrProbe::NoneMerged));
        }
        let promoted = self.latest_promotion(number)?;
        Ok((OpenPrProbe::NoneOpen, merged_since(&merged, promoted)))
    }

    fn latest_promotion(&self, number: u32) -> Result<Option<DateTime<Utc>>> {
        // Gitea 1.24 FindComments paginates only for page > 0. Omit page to
        // read the entire visible timeline: a paged read can hide every row
        // on one page while a later page still contains a label addition.
        let (timeline, count) = self.get(&format!("issues/{number}/timeline"))?;
        let rows = match &timeline {
            Value::Null => &[][..],
            Value::Array(rows) => rows,
            _ => anyhow::bail!("expected a Gitea timeline listing"),
        };
        ensure!(count == Some(rows.len()), "incomplete Gitea timeline listing");
        let mut seen = HashSet::new();
        let mut promoted = None;
        for row in rows {
            record_identity(&mut seen, row, "id", "timeline")?;
            if string(row, "type")? == "label" && string(&row["label"], "name")? == "loom:issue" {
                // Gitea records label addition as "1" and removal as "".
                match row["body"].as_str() {
                    Some("1") => {
                        let at = string(row, "created_at")?.parse::<DateTime<Utc>>()?;
                        promoted = promoted.max(Some(at));
                    }
                    Some("") => {}
                    _ => anyhow::bail!("unrecognised Gitea label event"),
                }
            }
        }
        Ok(promoted)
    }

    fn matches_head_branch(&self, row: &Value, branch: &str) -> Result<bool> {
        // Gitea retains the original branch in label (empty for AGit), even
        // when branch deletion replaces ref with refs/pull/N/head.
        let name = row["head"]["label"]
            .as_str()
            .context("missing Gitea head branch")?;
        Ok(name == branch && self.same_head_repo(row)?)
    }

    fn same_head_repo(&self, row: &Value) -> Result<bool> {
        if row["head"]["repo"].is_null() && row["head"]["repo_id"].as_i64() == Some(-1) {
            // Gitea no longer has the fork. The base repository still exists.
            return Ok(false);
        }
        Ok(string(&row["head"]["repo"], "full_name")?.eq_ignore_ascii_case(&self.slug))
    }

    pub(crate) fn lease(
        &self,
        issue: u32,
        trust: &mut TrustedAuthors,
        now: DateTime<Utc>,
        ttl: f64,
    ) -> Result<LeaseLeg> {
        // The issue comments endpoint is unpaginated in Gitea 1.24.
        let (response, count) = self.get(&format!("issues/{issue}/comments"))?;
        let comments = response.as_array().context("expected issue comments")?;
        ensure!(count.is_none_or(|n| n == comments.len()), "incomplete issue comments");
        let mut rows = Vec::new();
        for row in comments {
            let body = row["body"].as_str().context("missing comment body")?;
            if !(body.starts_with(crate::sweep_registry::LEASE_MARKER_PREFIX)
                || body.starts_with(crate::forge_check_claim::YIELD_MARKER_PREFIX))
            {
                continue;
            }
            if row["original_author"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
                || !trust.contains(self, row)?
            {
                continue;
            }
            rows.push(LeaseRow {
                body: body.to_string(),
                updated_at: string(row, "updated_at")?.parse()?,
            });
        }
        Ok(lease_leg(&rows, now, ttl))
    }
}

/// Gitea does not supply GitHub's author_association. Its unpaginated
/// assignees endpoint lists repository writers. Fetch it once, and only when
/// a marker or a fork PR actually needs an author decision.
pub(crate) struct TrustedAuthors {
    writers: Option<Vec<String>>,
    allowlist: Vec<String>,
}

impl TrustedAuthors {
    pub(crate) fn for_root(root: &Path) -> Self {
        Self {
            writers: None,
            allowlist: allowlist_from_config(&crate::config_resolver::resolve_effective_config(
                root,
            )),
        }
    }

    fn contains(&mut self, repo: &Repository, row: &Value) -> Result<bool> {
        let user = row.get("user").context("missing Gitea author")?;
        if user.is_null() {
            return Ok(false);
        }
        let login = string(user, "login")?;
        let author = Author::new(Some(login), None);
        if trusted_by(&author, |_| false, None, &self.allowlist) {
            return Ok(true);
        }
        if self.writers.is_none() {
            let (users, count) = repo.get("assignees")?;
            let users = users.as_array().context("expected Gitea assignees")?;
            ensure!(count.is_none_or(|n| n == users.len()), "incomplete Gitea assignees");
            let writers = users
                .iter()
                .map(|u| Ok(string(u, "login")?.to_owned()))
                .collect::<Result<_>>()?;
            self.writers = Some(writers);
        }
        Ok(self
            .writers
            .as_ref()
            .is_some_and(|writers| writers.iter().any(|w| w.eq_ignore_ascii_case(login))))
    }
}

fn record_identity(seen: &mut HashSet<u64>, row: &Value, key: &str, path: &str) -> Result<()> {
    let id = row[key]
        .as_u64()
        .filter(|n| *n > 0)
        .context("missing Gitea row identity")?;
    ensure!(seen.insert(id), "repeated Gitea {path} row {id}");
    Ok(())
}

fn issue_number(row: &Value) -> Result<u32> {
    row["number"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .filter(|&n| n > 0)
        .context("invalid issue number")
}

fn string<'a>(row: &'a Value, field: &str) -> Result<&'a str> {
    row[field]
        .as_str()
        .filter(|s| !s.is_empty())
        .with_context(|| format!("missing Gitea {field}"))
}

fn is_pull(row: &Value) -> Result<bool> {
    match row.get("pull_request") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Object(_)) => Ok(true),
        _ => Err(anyhow!("invalid Gitea pull_request field")),
    }
}

fn closed(row: &Value) -> Result<bool> {
    match row["state"].as_str() {
        Some("open") => Ok(false),
        Some("closed") => Ok(true),
        _ => Err(anyhow!("invalid Gitea issue state")),
    }
}

fn body(row: &Value) -> Result<&str> {
    match row.get("body") {
        Some(Value::String(body)) => Ok(body),
        Some(Value::Null) => Ok(""),
        _ => Err(anyhow!("missing Gitea body")),
    }
}
