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
use std::collections::HashSet;
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

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

struct StepError {
    code: i32,
    reason: String,
}

fn read_current(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    repo: &str,
    issue: u64,
    err: &mut dyn Write,
) -> Result<(IssueView, Current), StepError> {
    let view = forge.view(issue).map_err(|e| {
        let reason = format!("could not read issue #{issue}: {e}");
        let _ = writeln!(err, "operator-decision: {reason}");
        StepError {
            code: exit::FORGE,
            reason,
        }
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
                let reason = format!("could not read the edit history of #{issue}: {e}");
                let _ = writeln!(err, "operator-decision: {reason}");
                StepError {
                    code: exit::FORGE,
                    reason,
                }
            })?;
            current_with_history(repo, issue, &view.title, &view.body, &history, policy).map_err(
                |why| {
                    let reason = format!("cannot resolve #{issue}: {why}");
                    let _ = writeln!(err, "operator-decision: REFUSED - {reason}");
                    StepError {
                        code: exit::REFUSED,
                        reason,
                    }
                },
            )?
        }
        Err(why) => {
            let reason = format!("cannot resolve #{issue}: {why}");
            let _ = writeln!(err, "operator-decision: REFUSED - {reason}");
            return Err(StepError {
                code: exit::REFUSED,
                reason,
            });
        }
    };
    Ok((view, cur))
}

fn status_now(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    cur: &Current,
    err: &mut dyn Write,
) -> Result<(Status, usize), StepError> {
    let items = forge.comments(cur.issue).map_err(|e| {
        let reason = format!("could not read the comments of #{}: {e}", cur.issue);
        let _ = writeln!(err, "operator-decision: {reason}");
        StepError {
            code: exit::FORGE,
            reason,
        }
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

#[allow(clippy::too_many_arguments)]
fn review_detailed(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    repo: &str,
    issue: u64,
    option_id: &str,
    effect: Effect,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<Reviewed, StepError> {
    let (view, cur) = read_current(forge, policy, repo, issue, err)?;
    let Some(opt) = cur
        .decision
        .options
        .iter()
        .find(|o| o.id.trim() == option_id)
    else {
        let ids: Vec<&str> = cur.decision.options.iter().map(|o| o.id.trim()).collect();
        let reason = format!(
            "option `{option_id}` is not in the published decision (options: {})",
            ids.join(", ")
        );
        let _ = writeln!(err, "operator-decision: REFUSED - {reason}");
        return Err(StepError {
            code: exit::REFUSED,
            reason,
        });
    };
    if !super::resolution::id_is_markable(option_id) {
        let reason = format!(
            "option id `{option_id}` has characters a resolution record cannot carry (use \
             letters, digits, . _ : -); re-publish the decision with a plain id"
        );
        let _ = writeln!(err, "operator-decision: REFUSED - {reason}");
        return Err(StepError {
            code: exit::REFUSED,
            reason,
        });
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
    review_detailed(forge, policy, repo, issue, option_id, effect, out, err).map_err(|e| e.code)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReceiptStatus {
    Unverified,
    Verified { already: bool },
}

impl ReceiptStatus {
    fn is_verified(self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Unverified => "unverified",
            Self::Verified { already: false } => "verified",
            Self::Verified { already: true } => "verified(already)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReleaseStatus {
    NotRequested,
    Skipped,
    NotHeld,
    Released,
    Failed,
}

impl ReleaseStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotRequested => "-",
            Self::Skipped => "skipped",
            Self::NotHeld => "not-held",
            Self::Released => "released",
            Self::Failed => "failed",
        }
    }
}

struct RecordOutcome {
    code: i32,
    receipt: ReceiptStatus,
    release: ReleaseStatus,
    failure: Option<(&'static str, String)>,
}

fn record_detailed(
    forge: &mut dyn ResolveForge,
    policy: &TrustPolicy,
    r: &Reviewed,
    release: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> RecordOutcome {
    let n = r.issue;
    let resolution = &r.resolution;
    let skipped_release = if release {
        ReleaseStatus::Skipped
    } else {
        ReleaseStatus::NotRequested
    };
    let fail = |phase: &'static str, code: i32, reason: String| RecordOutcome {
        code,
        receipt: ReceiptStatus::Unverified,
        release: skipped_release,
        failure: Some((phase, reason)),
    };

    // Scope may change between review and recording: re-read, compare.
    let (view, fresh) = match read_current(forge, policy, &r.repo, n, err) {
        Ok(x) => x,
        Err(e) => return fail("record", e.code, e.reason),
    };
    if fresh.decision_digest != r.cur.decision_digest || fresh.scope_digest != r.cur.scope_digest {
        let reason = "the decision or the proposal changed after you reviewed it; nothing was \
                      recorded. Re-run to review the current text"
            .to_string();
        let _ = writeln!(err, "operator-decision: REFUSED - {reason}");
        return fail("record", exit::REFUSED, reason);
    }

    let (prior, _) = match status_now(forge, policy, &r.cur, err) {
        Ok(x) => x,
        Err(e) => return fail("record", e.code, e.reason),
    };
    let already = match &prior {
        Status::Resolved(p) if p.option == resolution.option && p.effect == resolution.effect => {
            true
        }
        Status::Pending | Status::Stale(_) | Status::Unavailable(_) | Status::Unreadable(_) => {
            false
        }
        other => {
            let reason = format!(
                "this decision already has a standing resolution ({}): {}",
                other.token(),
                other
                    .reason()
                    .unwrap_or_else(|| "a different choice is recorded".to_string())
            );
            let _ = writeln!(err, "operator-decision: REFUSED - {reason}");
            return fail("record", exit::REFUSED, reason);
        }
    };
    if !already {
        let body = render_comment(resolution, &r.option_label);
        if let Err(e) = forge.post_comment(n, &body) {
            let reason = format!("recording the resolution on #{n} failed: {e}");
            let _ = writeln!(err, "operator-decision: {reason}");
            return fail("record", exit::FORGE, reason);
        }
    }

    // Persist, then verify what the forge now says, before any release.
    let (after, ignored) = match status_now(forge, policy, &r.cur, err) {
        Ok(x) => x,
        Err(e) => return fail("verify", e.code, e.reason),
    };
    match &after {
        Status::Resolved(p) if p.option == resolution.option && p.effect == resolution.effect => {}
        other => {
            let reason = format!(
                "the resolution is on #{n} but does not verify ({}): {}{}. Nothing was released",
                other.token(),
                other.reason().unwrap_or_default(),
                if ignored > 0 {
                    "; the forge does not report the posting account as an operator of this \
                     repository (OWNER/MEMBER/COLLABORATOR or fleet admin)"
                } else {
                    ""
                }
            );
            let _ = writeln!(err, "operator-decision: {reason}");
            return fail("verify", UNVERIFIED, reason);
        }
    }
    let receipt = ReceiptStatus::Verified { already };
    let _ = writeln!(out, "RECORDED={}", if already { "already" } else { "true" });
    let _ = writeln!(out, "VERIFIED=true");

    let release_status = if release {
        let held = view.labels.iter().any(|l| l == DECISION_LABEL);
        if held {
            if let Err(e) = forge.remove_label(n, DECISION_LABEL) {
                let reason = format!("removing {DECISION_LABEL} from #{n} failed: {e}");
                let _ = writeln!(err, "operator-decision: {reason}");
                return RecordOutcome {
                    code: exit::FORGE,
                    receipt,
                    release: ReleaseStatus::Failed,
                    failure: Some(("release", reason)),
                };
            }
        }
        let _ = writeln!(out, "RELEASED={held}");
        if held {
            ReleaseStatus::Released
        } else {
            ReleaseStatus::NotHeld
        }
    } else {
        ReleaseStatus::NotRequested
    };
    RecordOutcome {
        code: exit::OK,
        receipt,
        release: release_status,
        failure: None,
    }
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
    record_detailed(forge, policy, r, release, out, err).code
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

impl ResolveItem {
    fn canonical_line(&self) -> String {
        format!("{}#{}={}", self.repo.to_ascii_lowercase(), self.issue, self.option.trim())
    }
}

/// Opening delimiter printed before the ready-to-save `--from` retry block on
/// partial batch failure (#15385). Starts with `#` so the entire delimited
/// block can be saved directly and parsed by [`parse_items`].
pub const RETRY_BEGIN: &str = "# --- BEGIN RETRY --from ---";
/// Closing delimiter printed after the ready-to-save `--from` retry block.
pub const RETRY_END: &str = "# --- END RETRY --from ---";

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

struct BatchOutcome {
    target: String,
    code: i32,
    receipt: ReceiptStatus,
    release: ReleaseStatus,
    failure: Option<(&'static str, String)>,
}

fn write_batch_summary(outcomes: &[BatchOutcome], out: &mut dyn Write) {
    let _ = writeln!(out, "BATCH_SUMMARY");
    let rows: Vec<(&str, &str, &str, &str, String)> = outcomes
        .iter()
        .map(|o| {
            let status = if o.code == exit::OK { "ok" } else { "failed" };
            let reason = match &o.failure {
                Some((phase, msg)) => format!("{phase}: {}", one_line(msg)),
                None => "-".to_string(),
            };
            (o.target.as_str(), status, o.receipt.as_str(), o.release.as_str(), reason)
        })
        .collect();
    let w_item = rows
        .iter()
        .map(|r| r.0.len())
        .max()
        .unwrap_or(0)
        .max("ITEM".len());
    let w_status = rows
        .iter()
        .map(|r| r.1.len())
        .max()
        .unwrap_or(0)
        .max("STATUS".len());
    let w_receipt = rows
        .iter()
        .map(|r| r.2.len())
        .max()
        .unwrap_or(0)
        .max("RECEIPT".len());
    let w_release = rows
        .iter()
        .map(|r| r.3.len())
        .max()
        .unwrap_or(0)
        .max("RELEASE".len());
    let _ = writeln!(
        out,
        "{:<w_item$}  {:<w_status$}  {:<w_receipt$}  {:<w_release$}  REASON",
        "ITEM", "STATUS", "RECEIPT", "RELEASE",
    );
    for (item, status, receipt, release, reason) in &rows {
        let _ = writeln!(
            out,
            "{item:<w_item$}  {status:<w_status$}  {receipt:<w_receipt$}  {release:<w_release$}  {reason}",
        );
    }
}

fn write_retry_block(
    outcomes: &[BatchOutcome],
    effect: Effect,
    release: bool,
    out: &mut dyn Write,
) {
    // A later success satisfies earlier failures of the same repo, issue and option.
    let mut completed = HashSet::new();
    let mut retry = Vec::new();
    for outcome in outcomes.iter().rev() {
        let target = outcome.target.as_str();
        if outcome.code == exit::OK {
            completed.insert(target);
        } else if !completed.contains(target) {
            retry.push(target);
        }
    }
    if retry.is_empty() {
        return;
    }
    let release_flag = if release { " --release" } else { "" };
    let _ = writeln!(out, "{RETRY_BEGIN}");
    let _ = writeln!(out, "# Save this block as retry.txt and rerun:");
    let _ = writeln!(
        out,
        "#   loom-daemon operator-decision resolve --from retry.txt --effect {}{release_flag}",
        effect.as_str()
    );
    for line in retry.into_iter().rev() {
        let _ = writeln!(out, "{line}");
    }
    let _ = writeln!(out, "{RETRY_END}");
}

/// Batch `resolve` (#15108, #15385): review EVERY item first — each prints its
/// own `REPO=`/`ISSUE=`/…/`SCOPE_DIGEST=` block under an `ITEM=i/N` header —
/// then take ONE typed confirmation naming how many items were reviewed
/// (`resolve <N>`), then run the same record/re-read/verify/release steps per
/// item. After a confirmed batch, prints a per-item summary table in input
/// order (including items refused at review and separating verified receipts
/// from failed hold releases) and, if any requests still need a retry, a
/// ready-to-save `--from` block containing those items. A refused or failed item
/// does not stop the others; the exit code is `0` only when every item
/// completed, otherwise the first non-zero item code wins — review-phase codes
/// before record-phase ones. `forge_for` builds the forge for one item's
/// repository.
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
    let mut outcomes: Vec<Option<BatchOutcome>> = (0..n).map(|_| None).collect();
    let mut first_bad: Option<i32> = None;
    let skipped_release = if req.release {
        ReleaseStatus::Skipped
    } else {
        ReleaseStatus::NotRequested
    };
    for (i, item) in req.items.iter().enumerate() {
        let _ = writeln!(out, "ITEM={}/{}", i + 1, n);
        let mut forge = forge_for(&item.repo);
        match review_detailed(
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
            Err(e) => {
                first_bad.get_or_insert(e.code);
                outcomes[i] = Some(BatchOutcome {
                    target: item.canonical_line(),
                    code: e.code,
                    receipt: ReceiptStatus::Unverified,
                    release: skipped_release,
                    failure: Some(("review", e.reason)),
                });
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
    for (i, r) in &reviewed {
        let _ = writeln!(out, "ITEM={}/{}", i + 1, n);
        let item = &req.items[*i];
        let mut forge = forge_for(&item.repo);
        let rec = record_detailed(&mut *forge, policy, r, req.release, out, err);
        if rec.code != exit::OK {
            first_bad.get_or_insert(rec.code);
        }
        outcomes[*i] = Some(BatchOutcome {
            target: item.canonical_line(),
            code: rec.code,
            receipt: rec.receipt,
            release: rec.release,
            failure: rec.failure,
        });
    }
    let final_outcomes: Vec<BatchOutcome> = outcomes.into_iter().flatten().collect();
    let recorded = final_outcomes
        .iter()
        .filter(|o| o.receipt.is_verified())
        .count();
    let failed = final_outcomes.iter().filter(|o| o.code != exit::OK).count();
    let _ = writeln!(out, "BATCH_RECORDED={recorded}");
    let _ = writeln!(out, "BATCH_FAILED={failed}");
    write_batch_summary(&final_outcomes, out);
    write_retry_block(&final_outcomes, req.effect, req.release, out);
    first_bad.unwrap_or(exit::OK)
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
