//! `loom-daemon operator-decision resolve` (#14399): the attended path that
//! records an operator's choice of one ranked option, bound to the exact
//! decision and proposal the operator reviewed. See [`super::resolution`] for
//! the record and the verifier.
//!
//! The order is the safety property:
//!
//! 1. read the issue, parse the published decision with the existing ranked
//!    parser, and **show** the exact option, effect and scope digests;
//! 2. require an explicit confirmation (the operator types the option ID);
//! 3. **re-read** the issue and refuse if the decision or proposal changed
//!    since the review;
//! 4. record the comment (a retry of the same resolution records nothing);
//! 5. read the comments back and run the **same verifier the gate uses**; a
//!    resolution that does not verify under the forge's author metadata never
//!    releases anything;
//! 6. only then, and only with `--release`, drop the `loom:operator` hold.
//!
//! Forge access goes through [`ResolveForge`] so every ordering rule is tested
//! against a recording fake.

use super::cli::exit;
use super::render::extract_block;
use super::resolution::{
    candidates, current, current_with_history, digest_option, render_comment, verify, BodyEdit,
    Current, Effect, Evidence, Resolution, Status,
};
use super::DECISION_LABEL;
use crate::comment_trust::TrustPolicy;
use std::io::Write;

/// Not run from an attended session.
pub const UNATTENDED: i32 = 5;
/// The comment was recorded (or already stood) but does not verify as an
/// operator's resolution under the forge's author metadata; nothing was
/// released.
pub const UNVERIFIED: i32 = 6;

/// An issue as `resolve` reads it.
#[derive(Debug, Clone, Default)]
pub struct IssueView {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
}

/// The forge operations `resolve` needs.
pub trait ResolveForge {
    /// Canonical lower-case `owner/repo`.
    fn repo(&mut self) -> Result<String, String>;
    fn view(&mut self, issue: u64) -> Result<IssueView, String>;
    /// Every comment of the issue, REST listing shape, oldest first.
    fn comments(&mut self, issue: u64) -> Result<Vec<serde_json::Value>, String>;
    /// The issue body's edit history (#15108): each edit's post-edit body and
    /// the forge's report of its editor, newest first. Only read when the
    /// body carries no decision block.
    fn body_history(&mut self, issue: u64) -> Result<Vec<BodyEdit>, String>;
    fn post_comment(&mut self, issue: u64, body: &str) -> Result<(), String>;
    fn remove_label(&mut self, issue: u64, label: &str) -> Result<(), String>;
}

#[derive(Debug, Clone)]
pub struct ResolveRequest {
    pub issue: u64,
    pub option: String,
    pub effect: Effect,
    /// Drop `loom:operator` once the resolution verifies (proceed only).
    pub release: bool,
    pub dry_run: bool,
}

/// Environment markers of a dispatched (unattended) run.
const UNATTENDED_ENV: &[&str] = &[
    "LOOM_SWEEP_ID",
    "LOOM_AGENT_SESSION_PID",
    "LOOM_SWEEP_LEASE_RENEW_DISPATCHED",
];

/// `Ok` only for an attended terminal session: stdin and stderr are TTYs and
/// no dispatched-run marker is set. A recommending agent, a sweep and a pipe
/// all fail it.
///
/// # Errors
///
/// Why the session is not attended.
pub fn require_attended() -> Result<(), String> {
    use std::io::IsTerminal;
    if let Some(v) = UNATTENDED_ENV
        .iter()
        .find(|v| std::env::var_os(v).is_some_and(|x| !x.is_empty()))
    {
        return Err(format!("{v} is set: this is a dispatched run, not an attended session"));
    }
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Err(
            "stdin/stderr is not a terminal: an operator must run this interactively".to_string()
        );
    }
    Ok(())
}

fn read_current(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    repo: &str,
    issue: u64,
    err: &mut dyn Write,
) -> Result<(IssueView, Current), i32> {
    let view = forge.view(issue).map_err(|e| {
        let _ = writeln!(err, "operator-decision: could not read issue #{issue}: {e}");
        exit::FORGE
    })?;
    let cur = match current(repo, issue, &view.title, &view.body) {
        Ok(c) => c,
        Err(_) if extract_block(&view.body).is_none() => {
            // #15108: an attended ruling may have replaced the block with an
            // "Operator ruling" section; the exact text the operator reviewed
            // survives as a prior body version. No block at all is the only
            // case that may fall back — a block that fails to parse stays
            // authoritative.
            let history = forge.body_history(issue).map_err(|e| {
                let _ = writeln!(
                    err,
                    "operator-decision: could not read the edit history of #{issue}: {e}"
                );
                exit::FORGE
            })?;
            current_with_history(repo, issue, &view.title, &view.body, &history, policy).map_err(
                |why| {
                    let _ = writeln!(
                        err,
                        "operator-decision: REFUSED - cannot resolve #{issue}: {why}"
                    );
                    exit::REFUSED
                },
            )?
        }
        Err(why) => {
            let _ = writeln!(err, "operator-decision: REFUSED - cannot resolve #{issue}: {why}");
            return Err(exit::REFUSED);
        }
    };
    Ok((view, cur))
}

fn status_now(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    cur: &Current,
    err: &mut dyn Write,
) -> Result<(Status, usize), i32> {
    let items = forge.comments(cur.issue).map_err(|e| {
        let _ =
            writeln!(err, "operator-decision: could not read the comments of #{}: {e}", cur.issue);
        exit::FORGE
    })?;
    let a = verify(&Evidence {
        current: Ok(cur.clone()),
        candidates: candidates(policy, &items),
    });
    Ok((a.status, a.ignored_unauthorized))
}

/// One item past its review: everything `record` needs, nothing asked yet.
#[derive(Debug, Clone)]
pub struct Reviewed {
    pub repo: String,
    pub issue: u64,
    pub cur: Current,
    pub resolution: Resolution,
    pub option_label: String,
}

/// Phase 1 of the safety property: read the issue (with the #15108 history
/// fallback), find the option, and print the review — the exact option,
/// effect and digests being approved. Asks nothing.
///
/// # Errors
///
/// The exit code: refusal reasons on `err`, forge failures as [`exit::FORGE`].
#[allow(clippy::too_many_arguments)]
pub fn review(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    repo: &str,
    issue: u64,
    option_id: &str,
    effect: Effect,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Reviewed, i32> {
    let (view, cur) = read_current(forge, policy, repo, issue, err)?;
    let Some(opt) = cur
        .decision
        .options
        .iter()
        .find(|o| o.id.trim() == option_id)
    else {
        let ids: Vec<&str> = cur.decision.options.iter().map(|o| o.id.trim()).collect();
        let _ = writeln!(
            err,
            "operator-decision: REFUSED - option `{option_id}` is not in the published decision \
             (options: {})",
            ids.join(", ")
        );
        return Err(exit::REFUSED);
    };
    if !super::resolution::id_is_markable(option_id) {
        let _ = writeln!(
            err,
            "operator-decision: REFUSED - option id `{option_id}` has characters a resolution \
             record cannot carry (use letters, digits, . _ : -); re-publish the decision with a \
             plain id"
        );
        return Err(exit::REFUSED);
    }
    let resolution = Resolution {
        repo: repo.to_ascii_lowercase(),
        issue,
        decision: cur.decision_digest.clone(),
        scope: cur.scope_digest.clone(),
        option: option_id.to_string(),
        option_digest: digest_option(opt),
        effect,
    };

    // The review: exactly what is being approved, before anything is asked.
    let one_line = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    let _ = writeln!(out, "REPO={}", resolution.repo);
    let _ = writeln!(out, "ISSUE={issue}");
    let _ = writeln!(out, "TITLE={}", one_line(&view.title));
    let _ = writeln!(out, "QUESTION={}", one_line(&cur.decision.question));
    let _ = writeln!(out, "OPTION={option_id}");
    let _ = writeln!(out, "OPTION_LABEL={}", one_line(&opt.label));
    let _ = writeln!(out, "OPTION_WHY={}", one_line(opt.why.as_deref().unwrap_or("")));
    let _ = writeln!(out, "EFFECT={}", effect.as_str());
    let _ = writeln!(out, "DECISION_DIGEST={}", cur.decision_digest);
    let _ = writeln!(out, "SCOPE_DIGEST={}", cur.scope_digest);
    let option_label = opt.label.clone();
    Ok(Reviewed {
        repo: resolution.repo.clone(),
        issue,
        cur,
        resolution,
        option_label,
    })
}

/// Phases 2-6 for one reviewed item: re-read and refuse if the decision or
/// proposal changed since the review, record (a retry of the same resolution
/// records nothing), verify with the gate's own verifier, and only with
/// `release` drop the `loom:operator` hold. Returns the item's exit code.
pub fn record(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    r: &Reviewed,
    release: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    let n = r.issue;
    let resolution = &r.resolution;
    // Scope may change between review and recording: re-read, compare.
    let (view, fresh) = match read_current(forge, policy, &r.repo, n, err) {
        Ok(x) => x,
        Err(code) => return code,
    };
    if fresh.decision_digest != r.cur.decision_digest || fresh.scope_digest != r.cur.scope_digest {
        let _ = writeln!(
            err,
            "operator-decision: REFUSED - the decision or the proposal changed after you \
             reviewed it; nothing was recorded. Re-run to review the current text"
        );
        return exit::REFUSED;
    }

    let (prior, _) = match status_now(forge, policy, &r.cur, err) {
        Ok(x) => x,
        Err(code) => return code,
    };
    let already = match &prior {
        Status::Resolved(p) if p.option == resolution.option && p.effect == resolution.effect => {
            true
        }
        Status::Pending | Status::Stale(_) | Status::Unavailable(_) | Status::Unreadable(_) => {
            false
        }
        other => {
            let _ = writeln!(
                err,
                "operator-decision: REFUSED - this decision already has a standing resolution \
                 ({}): {}",
                other.token(),
                other
                    .reason()
                    .unwrap_or_else(|| "a different choice is recorded".to_string())
            );
            return exit::REFUSED;
        }
    };
    if !already {
        let body = render_comment(resolution, &r.option_label);
        if let Err(e) = forge.post_comment(n, &body) {
            let _ =
                writeln!(err, "operator-decision: recording the resolution on #{n} failed: {e}");
            return exit::FORGE;
        }
    }

    // Persist, then verify what the forge now says, before any release.
    let (after, ignored) = match status_now(forge, policy, &r.cur, err) {
        Ok(x) => x,
        Err(code) => return code,
    };
    match &after {
        Status::Resolved(p) if p.option == resolution.option && p.effect == resolution.effect => {}
        other => {
            let _ = writeln!(
                err,
                "operator-decision: the resolution is on #{n} but does not verify ({}): {}{}. \
                 Nothing was released",
                other.token(),
                other.reason().unwrap_or_default(),
                if ignored > 0 {
                    "; the forge does not report the posting account as an operator of this \
                     repository (OWNER/MEMBER/COLLABORATOR or fleet admin)"
                } else {
                    ""
                }
            );
            return UNVERIFIED;
        }
    }
    let _ = writeln!(out, "RECORDED={}", if already { "already" } else { "true" });
    let _ = writeln!(out, "VERIFIED=true");

    if release {
        let held = view.labels.iter().any(|l| l == DECISION_LABEL);
        if held {
            if let Err(e) = forge.remove_label(n, DECISION_LABEL) {
                let _ = writeln!(
                    err,
                    "operator-decision: removing {DECISION_LABEL} from #{n} failed: {e}"
                );
                return exit::FORGE;
            }
        }
        let _ = writeln!(out, "RELEASED={held}");
    }
    exit::OK
}

/// Record the resolution. Returns the exit code.
pub fn resolve(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    req: &ResolveRequest,
    confirm: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if req.release && req.effect != Effect::Proceed {
        let _ = writeln!(err, "operator-decision: --release applies only to --effect proceed");
        return exit::USAGE;
    }
    let repo = match forge.repo() {
        Ok(r) => r.to_ascii_lowercase(),
        Err(e) => {
            let _ = writeln!(err, "operator-decision: could not determine the repository: {e}");
            return exit::FORGE;
        }
    };
    let option_id = req.option.trim().to_string();
    let r = match review(forge, policy, &repo, req.issue, &option_id, req.effect, out, err) {
        Ok(r) => r,
        Err(code) => return code,
    };
    let _ = writeln!(out, "RELEASE={}", req.release);
    if req.dry_run {
        let _ = writeln!(out, "DRY_RUN=true");
        return exit::OK;
    }

    if !confirm(&option_id) {
        let _ = writeln!(err, "operator-decision: REFUSED - not confirmed; nothing was recorded");
        return exit::REFUSED;
    }

    record(forge, policy, &r, req.release, out, err)
}

/// One `--from` line of a batch (#15108): `owner/repo#issue=option-id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolveItem {
    pub repo: String,
    pub issue: u64,
    pub option: String,
}

/// Parse `--from` content: one `owner/repo#issue=option` per line; blank
/// lines and `#`-comment lines are ignored. Any other line is a usage error
/// naming it. Repositories are lower-cased; duplicates are left in order (a
/// repeated item records `RECORDED=already`, like a retried single resolve).
///
/// # Errors
///
/// The first malformed line, named with its number.
pub fn parse_items(text: &str) -> Result<Vec<ResolveItem>, String> {
    let mut items = Vec::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let bad = || format!("line {}: `{raw}` is not owner/repo#issue=option", n + 1);
        let (target, option) = line.split_once('=').ok_or_else(bad)?;
        let option = option.trim();
        let (repo, num) = target.trim().split_once('#').ok_or_else(bad)?;
        let repo = repo.trim().to_ascii_lowercase();
        let issue: u64 = num
            .trim()
            .parse()
            .ok()
            .filter(|i| *i > 0)
            .ok_or_else(|| format!("line {}: `{raw}` has no positive issue number", n + 1))?;
        if repo.split('/').count() != 2 || repo.split('/').any(str::is_empty) {
            return Err(format!("line {}: `{raw}` has no owner/repo repository", n + 1));
        }
        if option.is_empty() {
            return Err(format!("line {}: `{raw}` has no option id", n + 1));
        }
        items.push(ResolveItem {
            repo,
            issue,
            option: option.to_string(),
        });
    }
    if items.is_empty() {
        return Err("no items: every line was blank or a comment".to_string());
    }
    Ok(items)
}

/// The word a batch's typed confirmation starts with: the operator types
/// `resolve <reviewed count>`. A single-issue confirmation types the option
/// id, and an option id can never contain a space, so one flow's expected
/// confirmation cannot satisfy the other's (#15108).
pub const BATCH_CONFIRM_WORD: &str = "resolve";

/// One batch request (#15108): every item reviewed before one confirmation.
#[derive(Debug, Clone)]
pub struct BatchRequest {
    pub items: Vec<ResolveItem>,
    pub effect: Effect,
    pub release: bool,
    pub dry_run: bool,
}

/// Batch `resolve` (#15108): review EVERY item first — each prints its own
/// `REPO=`/`ISSUE=`/…/`SCOPE_DIGEST=` block under an `ITEM=i/N` header — then
/// take ONE typed confirmation naming how many items were reviewed
/// (`resolve <N>`), then run the same record/re-read/verify/release steps per
/// item. A refused item (unknown option, changed scope, no recoverable block)
/// is reported on stderr and does not stop the others; the exit code is `0`
/// only when every item recorded (or already stood), otherwise the first
/// non-zero item code wins — review-phase codes before record-phase ones.
/// `forge_for` builds the forge for one item's repository.
pub fn resolve_batch(
    forge_for: &mut dyn FnMut(&str) -> Box<dyn ResolveForge>,
    policy: &TrustPolicy,
    req: &BatchRequest,
    confirm: &mut dyn FnMut(&str) -> bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> i32 {
    if req.release && req.effect != Effect::Proceed {
        let _ = writeln!(err, "operator-decision: --release applies only to --effect proceed");
        return exit::USAGE;
    }
    let n = req.items.len();
    let mut reviewed: Vec<(usize, Reviewed)> = Vec::new();
    let mut first_bad: Option<i32> = None;
    for (i, item) in req.items.iter().enumerate() {
        let _ = writeln!(out, "ITEM={}/{}", i + 1, n);
        let mut forge = forge_for(&item.repo);
        match review(
            &mut *forge,
            policy,
            &item.repo,
            item.issue,
            item.option.trim(),
            req.effect,
            out,
            err,
        ) {
            Ok(r) => reviewed.push((i, r)),
            Err(code) => {
                first_bad.get_or_insert(code);
            }
        }
    }
    let k = reviewed.len();
    let _ = writeln!(out, "BATCH_ITEMS={n}");
    let _ = writeln!(out, "BATCH_REVIEWED={k}");
    let _ = writeln!(out, "RELEASE={}", req.release);
    if req.dry_run {
        let _ = writeln!(out, "DRY_RUN=true");
        return first_bad.unwrap_or(exit::OK);
    }
    if k == 0 {
        let _ = writeln!(
            err,
            "operator-decision: REFUSED - no item could be reviewed; nothing was recorded"
        );
        return first_bad.unwrap_or(exit::REFUSED);
    }
    let expected = format!("{BATCH_CONFIRM_WORD} {k}");
    if !confirm(&expected) {
        let _ = writeln!(err, "operator-decision: REFUSED - not confirmed; nothing was recorded");
        return exit::REFUSED;
    }
    let mut ok = 0;
    for (i, r) in &reviewed {
        let _ = writeln!(out, "ITEM={}/{}", i + 1, n);
        let item = &req.items[*i];
        let mut forge = forge_for(&item.repo);
        let code = record(&mut *forge, policy, r, req.release, out, err);
        if code == exit::OK {
            ok += 1;
        } else {
            first_bad.get_or_insert(code);
        }
    }
    let _ = writeln!(out, "BATCH_RECORDED={ok}");
    first_bad.unwrap_or(exit::OK)
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
