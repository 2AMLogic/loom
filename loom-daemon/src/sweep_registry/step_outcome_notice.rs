//! The visible reason behind a Builder step that ended without a PR (#13054).
//!
//! A Builder step that ends `noop` or `failed` returns the issue to the
//! draw. Only the `/loom:sweep` orchestrator commented on a no-op, and only
//! when the Builder got far enough to write `.no-changes-needed`; a step run
//! by the step engine, or one cut short, left no trace of why it looped.
//! [`SweepRegistry::note_builder_outcome`] makes the comment a daemon-side
//! guarantee: one marked comment per issue, edited in place on later steps (#13245), carrying
//! the outcome's reason token and, when the marker file is still on disk, its
//! first 200 characters. It is a comment only: no label change.
//!
//! #15413: a failed attempt also records what the daemon observed of its end
//! (exit status, run time) and where its output lives (worker host, log path,
//! dispatch sweep id). The comment is edited in place, so every attempt keeps
//! one line in its `Attempts` list and a retry carries the earlier lines
//! forward: the quarantine text can then send an operator to the right host
//! and dispatch section for each attempt of a streak, including attempts on
//! other workers. The pointer is never a transcript, which can hold
//! credentials.

use std::path::Path;

use super::step_ledger::StepRun;
use super::step_spawn::ended::{AttemptEvidence, Termination};
use super::SweepRegistry;
use crate::comment_trust::{records::AUTHOR_JQ, TrustPolicy};
use crate::step_dispatch::outcome_ledger::OutcomeClass;

/// Marker prefix on the comment; the sweep id follows, so a second record of
/// the same step is recognizable.
pub const STEP_OUTCOME_COMMENT_MARKER: &str = "<!-- loom:builder-step-outcome (#13054)";

/// Longest `.no-changes-needed` excerpt quoted.
const EXCERPT_CHARS: usize = 200;

/// Most sweep ids remembered for de-duplication.
const MAX_REMEMBERED: usize = 1024;

/// Heading of the attempts list in the comment body.
const ATTEMPTS_HEADING: &str = "Attempts (oldest first, one line per dispatch):";

/// Attempt lines kept in the comment at least: the oldest drop first, so the
/// body stays bounded however often the issue is redrawn. The registry raises
/// the bound to the quarantine threshold ([`attempts_kept`]) so the whole
/// failure streak a park cites stays discoverable.
const MAX_ATTEMPTS: usize = 8;

/// Attempt lines to keep for a quarantine threshold of `threshold`: never
/// fewer than [`MAX_ATTEMPTS`], and enough to cover the streak that parks.
#[must_use]
pub fn attempts_kept(threshold: u32) -> usize {
    MAX_ATTEMPTS.max(usize::try_from(threshold).unwrap_or(usize::MAX))
}

/// How an attempt ended, as one phrase. An unavailable status is stated, and a
/// numeric status is reported as observed — never as a signal. Pure.
fn termination_phrase(termination: Termination) -> String {
    match termination {
        Termination::Status(code) => format!("exit status `{code}`"),
        Termination::StatusUnavailable => "exit status unavailable (none was observed)".into(),
        Termination::TimedOut => "killed at the step timeout (no exit status)".into(),
        Termination::Cancelled => "cancelled (no exit status)".into(),
    }
}

/// Make a value safe inside a backticked, one-line attempt entry.
fn inline(value: &str) -> String {
    value.replace(['`', '\n', '\r'], "'")
}

/// The one-line list entry for `run`'s attempt, or `None` without evidence.
/// It starts with the sweep id so a retry's entry replaces its own earlier
/// one, and reads back through [`prior_attempts`].
fn attempt_line(run: &StepRun) -> Option<String> {
    let attempt: &AttemptEvidence = run.attempt.as_ref()?;
    let host = if attempt.host.is_empty() {
        "unknown"
    } else {
        attempt.host.as_str()
    };
    Some(format!(
        "- `{}`: host `{}`, {}, ran {}s, log `{}`",
        inline(&run.sweep_id),
        inline(host),
        termination_phrase(attempt.termination),
        attempt.ran_for.as_secs(),
        inline(&attempt.log),
    ))
}

/// The attempt lines of an earlier outcome comment `body`, in order. Pure.
#[must_use]
pub fn prior_attempts(body: &str) -> Vec<String> {
    body.lines()
        .skip_while(|l| l.trim() != ATTEMPTS_HEADING)
        .skip(1)
        .take_while(|l| l.starts_with("- `"))
        .map(str::to_string)
        .collect()
}

/// The sweep id an attempt line starts with.
fn attempt_id(line: &str) -> Option<&str> {
    line.split('`').nth(1)
}

/// Append `line`, replacing any earlier line of the same sweep.
fn push_attempt(lines: &mut Vec<String>, line: String) {
    let id = attempt_id(&line).unwrap_or_default().to_string();
    lines.retain(|l| attempt_id(l) != Some(id.as_str()));
    lines.push(line);
}

/// The attempts list for a rewritten body: `prior` lines, this attempt's line
/// replacing any earlier line of the same sweep, bounded to [`MAX_ATTEMPTS`].
fn attempts_block(prior: &[String], current: Option<String>, keep: usize) -> String {
    let mut lines: Vec<String> = prior.to_vec();
    if let Some(current) = current {
        push_attempt(&mut lines, current);
    }
    if lines.is_empty() {
        return String::new();
    }
    let drop = lines.len().saturating_sub(keep);
    format!("\n\n{ATTEMPTS_HEADING}\n{}", lines[drop..].join("\n"))
}

/// Where to look for a Builder attempt's details, for the fast-fail park
/// comment (#15413). Both parks (this host's streak and the captain's order)
/// use it: the attempts of a streak may have run on other workers.
#[must_use]
pub fn attempt_lookup_note() -> String {
    format!(
        " To see why each attempt ended: this issue's Builder step-outcome comment (marker \
         `{STEP_OUTCOME_COMMENT_MARKER}`) is edited in place and keeps one line per recent \
         attempt, giving its dispatch sweep id, worker host, exit status, run time and log path, \
         including attempts that ran on other workers. On the host a line names, open that log \
         path (attempts append to one file) and find the `{}` header whose `sweep_id=` equals the \
         line's sweep id; that section is the attempt's output. A full-issue sweep (not a \
         Builder step) posts no such comment: for it, open the sweep log \
         `.loom/logs/sweep-issue-<N>.log` on the host that ran it and read its last `{}` section.",
        crate::sweep_registry::DISPATCH_HEADER_MARKER,
        crate::sweep_registry::DISPATCH_HEADER_MARKER
    )
}

/// The first [`EXCERPT_CHARS`] characters of the Builder's no-changes marker
/// in `worktree`, if it is still there and non-empty.
#[must_use]
pub fn marker_excerpt(worktree: &Path) -> Option<String> {
    let text = std::fs::read_to_string(worktree.join(".no-changes-needed")).ok()?;
    let text: String = text.trim().chars().take(EXCERPT_CHARS).collect();
    (!text.is_empty()).then_some(text)
}

/// The comment body for `run`, or `None` when the outcome is not a Builder
/// `noop` / `failed`. Pure.
#[must_use]
pub fn outcome_notice_body(run: &StepRun, excerpt: Option<&str>) -> Option<String> {
    outcome_notice_body_with(run, excerpt, &[])
}

/// As [`outcome_notice_body`], carrying `prior` attempt lines forward from the
/// comment this one replaces ([`prior_attempts`]).
#[must_use]
pub fn outcome_notice_body_with(
    run: &StepRun,
    excerpt: Option<&str>,
    prior: &[String],
) -> Option<String> {
    outcome_notice_body_keeping(run, excerpt, prior, MAX_ATTEMPTS)
}

/// As [`outcome_notice_body_with`], keeping at most `keep` attempt lines.
#[must_use]
pub fn outcome_notice_body_keeping(
    run: &StepRun,
    excerpt: Option<&str>,
    prior: &[String],
    keep: usize,
) -> Option<String> {
    let marker = format!("{STEP_OUTCOME_COMMENT_MARKER} sweep={} -->", run.sweep_id);
    let reason = run
        .failure_class
        .as_deref()
        .unwrap_or(run.outcome.reason.as_str());
    match run.outcome.class {
        OutcomeClass::Noop => {
            let said = excerpt.map_or(String::new(), |e| format!(" Builder's note: {e}"));
            Some(format!(
                "{marker}\n**Builder found no actionable changes this pass** (reason: \
                 `{reason}`).{said} The claim was released without a PR and a no-op cooldown \
                 applies; this issue is parked after repeated no-op passes.{}",
                attempts_block(prior, None, keep)
            ))
        }
        OutcomeClass::Failed => {
            let observed =
                run.attempt.as_ref().map_or(String::new(), |a| {
                    format!(
                    " Observed: {}, ran {}s on host `{}`; its output is in `{}` on that host, in \
                     the dispatch section for sweep `{}`.",
                    termination_phrase(a.termination),
                    a.ran_for.as_secs(),
                    inline(if a.host.is_empty() { "unknown" } else { &a.host }),
                    inline(&a.log),
                    inline(&run.sweep_id),
                )
                });
            Some(format!(
                "{marker}\n**Builder step ended without a PR** (reason: `{reason}`). It left no \
                 `.no-changes-needed` conclusion, so this is a failed pass, not a deliberate \
                 no-op.{observed} The daemon will retry under its failure ladder.{}",
                attempts_block(prior, attempt_line(run), keep)
            ))
        }
        _ => None,
    }
}

impl SweepRegistry {
    /// Whether the notice for `sweep_id` is still due, remembering it when so.
    pub(crate) fn claim_outcome_notice(&mut self, sweep_id: &str) -> bool {
        if sweep_id.is_empty() {
            return false;
        }
        if self.step_outcome_notices.len() >= MAX_REMEMBERED {
            self.step_outcome_notices.clear();
        }
        self.step_outcome_notices.insert(sweep_id.to_string())
    }

    /// Post the reason a Builder step ended `noop` / `failed`, once per step.
    /// Best-effort and forge-less in tests.
    pub(crate) fn note_builder_outcome(&mut self, run: &StepRun) {
        if !matches!(run.outcome.class, OutcomeClass::Noop | OutcomeClass::Failed)
            || !self.claim_outcome_notice(&run.sweep_id)
        {
            return;
        }
        let worktree = self
            .config
            .workspace_root
            .join(".loom/worktrees")
            .join(format!("issue-{}", run.item));
        let excerpt = marker_excerpt(&worktree);
        let issue_arg = run.item.to_string();
        // #15413: the comment about to be edited, read first so its earlier
        // attempt lines survive the rewrite. A read that failed is not a
        // first attempt: the line waits in `step_outcome_pending` rather than
        // starting a second comment that would split the history.
        let lookup = if self.config.skip_label_flip {
            OutcomeLookup::Absent
        } else {
            self.latest_outcome_comment(&issue_arg)
        };
        let existing = match &lookup {
            OutcomeLookup::Found(id, body) => Some((*id, body.clone())),
            _ => None,
        };
        let mut prior = existing
            .as_ref()
            .map(|(_, body)| prior_attempts(body))
            .unwrap_or_default();
        for line in self
            .step_outcome_pending
            .get(&run.item)
            .into_iter()
            .flatten()
        {
            push_attempt(&mut prior, line.clone());
        }
        let keep = attempts_kept(self.quarantine_config.threshold);
        if matches!(lookup, OutcomeLookup::Unreadable) {
            if let Some(line) = attempt_line(run) {
                log::warn!(
                    "step_ledger: #{} outcome comment unreadable; attempt {} held for the next write",
                    run.item,
                    run.sweep_id
                );
                let pending = self.step_outcome_pending.entry(run.item).or_default();
                push_attempt(pending, line);
                let drop = pending.len().saturating_sub(keep);
                pending.drain(..drop);
            }
            return;
        }
        let Some(body) = outcome_notice_body_keeping(run, excerpt.as_deref(), &prior, keep) else {
            return;
        };
        log::info!(
            "step_ledger: #{} builder step {} ended {} ({})",
            run.item,
            run.sweep_id,
            run.outcome.class.as_str(),
            run.outcome.reason
        );
        if self.config.skip_label_flip {
            return;
        }
        // #13245: one outcome comment per issue, edited in place, so an issue
        // redrawn many times does not grow a comment per pass.
        if let Some((id, _)) = existing {
            let path = format!("repos/{{owner}}/{{repo}}/issues/comments/{id}");
            let patch = [
                "api",
                "-X",
                "PATCH",
                path.as_str(),
                "-f",
                &format!("body={body}"),
                "--silent",
            ];
            if self
                .gh_write("step_outcome.edit", patch)
                .is_ok_and(|o| o.is_some_and(|o| o.status.success()))
            {
                self.step_outcome_pending.remove(&run.item);
                return;
            }
        }
        let mut comment = vec![
            "issue",
            "comment",
            issue_arg.as_str(),
            "--body",
            body.as_str(),
        ];
        let repo_flag = crate::claim_reconciliation::gh_call::loom_repo_flag();
        comment.extend(repo_flag.iter().map(String::as_str));
        match self.gh_write("step_outcome.comment", comment) {
            Ok(Some(o)) if o.status.success() => {
                self.step_outcome_pending.remove(&run.item);
            }
            other => {
                log::debug!(
                    "step_ledger: builder outcome comment for #{} not written: {other:?}",
                    run.item
                );
                if let Some(line) = attempt_line(run) {
                    push_attempt(self.step_outcome_pending.entry(run.item).or_default(), line);
                }
            }
        }
    }

    /// The newest outcome comment on `issue` written by a trusted author
    /// (the daemon's own fleet identity, an insider; see
    /// [`crate::comment_trust`]). A comment from anyone else is content, not
    /// history: it is never copied into the next notice or edited over.
    fn latest_outcome_comment(&self, issue: &str) -> OutcomeLookup {
        let jq = format!(
            r#".[] | select(.body != null and (.body | startswith("{STEP_OUTCOME_COMMENT_MARKER}"))) | ({{id, body, {AUTHOR_JQ}}} | tojson)"#
        );
        let path = format!("repos/{{owner}}/{{repo}}/issues/{issue}/comments");
        let Ok(Some(out)) = self.gh_read(
            "step_outcome.list",
            ["api", path.as_str(), "--paginate", "--jq", jq.as_str()],
        ) else {
            return OutcomeLookup::Unreadable;
        };
        if !out.status.success() {
            return OutcomeLookup::Unreadable;
        }
        let ndjson = TrustPolicy::for_root(&self.config.workspace_root).trusted_ndjson(&out.stdout);
        // An unparseable line is dropped by the trust filter; a non-empty
        // projection with nothing surviving is an untrusted-only history.
        String::from_utf8_lossy(&ndjson)
            .lines()
            .filter_map(|l| {
                let v: serde_json::Value = serde_json::from_str(l.trim()).ok()?;
                Some((v.get("id")?.as_u64()?, v.get("body")?.as_str()?.to_string()))
            })
            .next_back()
            .map_or(OutcomeLookup::Absent, |(id, body)| OutcomeLookup::Found(id, body))
    }
}

/// What reading the issue's outcome comments told the daemon.
enum OutcomeLookup {
    /// The newest trusted outcome comment.
    Found(u64, String),
    /// The read succeeded and no trusted outcome comment exists: a first attempt.
    Absent,
    /// The read failed: the history is unknown, not empty.
    Unreadable,
}

#[cfg(test)]
#[path = "step_outcome_notice_tests.rs"]
mod tests;
