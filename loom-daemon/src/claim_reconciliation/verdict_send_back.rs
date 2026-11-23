//! The evidence walk behind [`VerdictPr::operator_send_back`] (#15068).
//!
//! A human rejecting a PR by hand (`gh pr edit --add-label
//! loom:changes-requested`, an operator ruling like #14470's) writes no
//! `loom:verdict-sha` marker, so the newest marker on the PR names an OLDER
//! verdict's tree and the pure marker-vs-head comparison in
//! [`super::decide_verdict`] reads their send-back as stale — clears the
//! rejection and re-queues the very head the operator just refused, with no
//! push in between. Clearing a *rejection* is the unsafe direction (the
//! opposite of a stale approval, which must never be carried forward), so
//! #13586 — for the shell guard, hardened by #14516 — pinned such a label to
//! the head current when it landed. This module is that rule's daemon-side
//! twin, so `forge reconcile_pr_verdicts` and
//! `defaults/scripts/verdict-staleness-guard.sh` step 4b agree; the shared
//! fixtures are `verdict_send_back_tests.rs` here and the `(v)`/`(w)` cases
//! (plus the #14470 case) in
//! `defaults/scripts/tests/test-verdict-staleness-guard.sh`.
//!
//! # The conditions, ported verbatim from the shell guard
//!
//! All of these must hold, checked against the live PR timeline and comment
//! listing — never against a marker:
//!
//! 1. the LATEST `labeled loom:changes-requested` timeline event exists, with
//!    a known actor and timestamp;
//! 2. that event is NEWER than the newest marker comment of this verdict kind
//!    (the marker's own comment timestamp, not the SHA);
//! 3. the label actor is a DIFFERENT login (case-insensitive) from the newest
//!    marker comment's author — `post-verdict.sh` writes the marker and
//!    applies the label under one identity, so the writer's own label carries
//!    no evidence about the head beyond the marker's SHA (#14516);
//! 4. ZERO `committed` / `head_ref_force_pushed` events follow that label
//!    event **by position** in the timeline, not by commit date — a commit
//!    prepared locally before the verdict and pushed after it (a Doctor
//!    fast-forward) lists by its commit date, before the label, and a
//!    fast-forward push emits no event at all;
//! 5. the actor is trusted: the write-or-higher collaborator permission
//!    (`admin`/`maintain`/`write` proves `COLLABORATOR`, since a timeline
//!    event carries no `author_association`) fed through the same
//!    [`crate::comment_trust`] predicate the marker scan uses (#9548).
//!
//! # Fail closed
//!
//! An unreadable timeline or comment listing, an unparseable timestamp, an
//! unknown marker author, an untrusted or unverifiable actor, or any missing
//! field yields `None` — and `None` means "no evidence", leaving
//! [`super::decide_verdict`] to answer exactly as it did before this module
//! existed. A redundant re-review is the cost of missing evidence; a cleared
//! human rejection is the cost of guessing.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::gh_call;
use super::{OperatorSendBack, VerdictKind};
use crate::cmd_out::CmdOutcome;
use crate::comment_trust::{self, Author};
use crate::gh_invocation::{AccessIntent, GhInvocation, GhTarget, Operation};

/// The newest marker comment of `kind`, as the live trusted listing reports
/// it: its author and the forge-assigned `created_at`, the two facts the 4b
/// comparison needs (the SHA itself stays [`super::extract_latest_verdict_sha`]'s
/// business — same listing, same newest-wins rule).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct MarkerComment {
    pub(super) author: String,
    pub(super) at: DateTime<Utc>,
}

/// The latest `labeled <verdict label>` timeline event, plus how many
/// head-moving events (`committed` / `head_ref_force_pushed`) sit AFTER it by
/// position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LabelEvent {
    pub(super) actor: String,
    pub(super) at: DateTime<Utc>,
    pub(super) head_moves_after: usize,
}

/// Read the PR's live timeline and comment listing and answer whether a
/// trusted human's send-back pins the standing `changes-requested` verdict to
/// the current head. Called only for the would-invalidate shape (see
/// `list_verdict_prs`), through [`super::read_cache::VERDICT_SEND_BACK`], so
/// an unmoved PR costs one walk per cache window rather than per tick.
///
/// `kind` is [`VerdictKind::ChangesRequested`] by construction; it is taken
/// as a parameter so the marker lookup reuses the same kind-filtered regex
/// the main scan does, never a marker of the other verdict.
pub(super) fn fetch_operator_send_back(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    kind: VerdictKind,
) -> Option<OperatorSendBack> {
    let label = kind.label();
    let marker = newest_marker_comment(gh_bin, root, pr_number, kind)?;
    let event = latest_label_event(gh_bin, root, pr_number, label)?;
    if !describes_current_head(&marker, &event) {
        return None;
    }
    if !actor_trusted(gh_bin, root, &event.actor) {
        return None;
    }
    Some(OperatorSendBack {
        actor: event.actor,
        labeled_at: event.at,
    })
}

/// Conditions 2-4 of the 4b test: the label event is newer than the newest
/// marker, by a different login, with no head move after it. Pure.
pub(super) fn describes_current_head(marker: &MarkerComment, event: &LabelEvent) -> bool {
    !marker.author.is_empty()
        && !event.actor.is_empty()
        && event.at > marker.at
        && !event.actor.eq_ignore_ascii_case(&marker.author)
        && event.head_moves_after == 0
}

/// The newest trusted comment carrying a `verdict-sha` marker of `kind`:
/// its author and `created_at` — the shell guard's `MARKER_AT` /
/// `MARKER_AUTHOR`, read from the same REST comment listing (so an
/// App-authored marker's `[bot]` spelling survives, per `comment_trust`'s
/// GraphQL note). `None` — fail closed — when the listing cannot be read, no
/// trusted comment carries such a marker, the comment's author is unknown (a
/// deleted account) or its timestamp does not parse.
fn newest_marker_comment(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    kind: VerdictKind,
) -> Option<MarkerComment> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/comments?per_page=100");
    let out = gh_call::ok_stdout(gh_call::read("verdict.pr_comments", gh_bin, root).args([
        "api",
        &path,
        "--paginate",
    ]))?;
    let items = comment_trust::parse_listing(&out)?;
    let policy = comment_trust::TrustPolicy::for_root(root);
    let mut newest = None;
    for item in policy.filter(items) {
        let Some(body) = item.get("body").and_then(Value::as_str) else {
            continue;
        };
        if super::body_carries_verdict_marker(body, kind) {
            newest = Some(item);
        }
    }
    let item = newest?;
    let author = item
        .get("user")
        .and_then(|u| u.get("login"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let at = item
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())?
        .with_timezone(&Utc);
    if author.is_empty() {
        return None;
    }
    Some(MarkerComment { author, at })
}

/// The latest `labeled <label>` event on the PR's timeline, with the number
/// of `committed` / `head_ref_force_pushed` events positioned after it.
/// `None` — fail closed — when the timeline cannot be read or parsed, no such
/// event exists, or its actor or timestamp is missing or unparseable.
fn latest_label_event(
    gh_bin: &Path,
    root: &Path,
    pr_number: u32,
    label: &str,
) -> Option<LabelEvent> {
    let path = format!("repos/{{owner}}/{{repo}}/issues/{pr_number}/timeline?per_page=100");
    let out = gh_call::ok_stdout(gh_call::read("verdict.pr_timeline", gh_bin, root).args([
        "api",
        &path,
        "--paginate",
    ]))?;
    let entries = comment_trust::parse_listing(&out)?;
    let mut latest_index = None;
    for (i, entry) in entries.iter().enumerate() {
        if entry.get("event").and_then(Value::as_str) == Some("labeled")
            && entry
                .pointer("/label/name")
                .and_then(Value::as_str)
                .is_some_and(|n| n == label)
        {
            latest_index = Some(i);
        }
    }
    let entry = entries.get(latest_index?)?;
    let actor = entry
        .pointer("/actor/login")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_default();
    let at = entry
        .get("created_at")
        .and_then(Value::as_str)
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())?
        .with_timezone(&Utc);
    if actor.is_empty() {
        return None;
    }
    let head_moves_after = entries[latest_index.unwrap() + 1..]
        .iter()
        .filter(|e| {
            matches!(
                e.get("event").and_then(Value::as_str),
                Some("committed") | Some("head_ref_force_pushed")
            )
        })
        .count();
    Some(LabelEvent {
        actor,
        at,
        head_moves_after,
    })
}

/// Whether `actor` may be believed (#9548): a timeline event carries no
/// `author_association`, so repo-insider status is proved the way the shell
/// guard proves it — the collaborator-permission endpoint, where
/// `admin`/`maintain`/`write` maps to `COLLABORATOR` — and the resulting
/// association is fed through the same [`comment_trust::TrustPolicy`] the
/// marker scan uses, so an allowlisted login, a fleet admin or a fleet App
/// passes exactly as it does for comments. An unreadable permission probe
/// (non-zero exit, 404 for a non-collaborator, 403 for a reader-only App)
/// is "no `COLLABORATOR` proof" (`NONE`), **not** "untrusted": the login
/// rules still apply, exactly as in `verdict-staleness-guard.sh` step 4b.
/// (The actor is passed as a user, not an App, so the fleet-App rule does
/// not fire for a timeline actor.)
///
/// The permission probe runs on the **writer** identity (it requires push
/// access to answer at all — a reader App is 403'd, which would make every
/// operator unverifiable), the same pin `cli/forge_identity_cmd.rs`'s
/// promotion probe uses; the site is listed in `tests/writer_read_guard.rs`.
fn actor_trusted(gh_bin: &Path, root: &Path, actor: &str) -> bool {
    let path = format!("repos/{{owner}}/{{repo}}/collaborators/{actor}/permission");
    let inv = GhInvocation::new(
        Operation::new("verdict.sendback_permission"),
        AccessIntent::Read,
        GhTarget::None,
        crate::cmd_out::DEFAULT_TIMEOUT,
    )
    .program(gh_bin)
    .current_dir(root)
    .writer_identity()
    .args(["api", &path, "--jq", ".permission"]);
    let perm = match inv.run() {
        CmdOutcome::Ran(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .trim()
            .to_ascii_lowercase(),
        // Unreadable == no COLLABORATOR proof, not untrusted.
        _ => String::new(),
    };
    let association = match perm.as_str() {
        "admin" | "maintain" | "write" => "COLLABORATOR",
        _ => "NONE",
    };
    let policy = comment_trust::TrustPolicy::for_root(root);
    policy.trusts(&Author::new(Some(actor), Some(association)))
}
