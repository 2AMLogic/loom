//! The PR-lease half of the Judge no-verdict backstop (#11855): `release`
//! records a Judge pass that ended without a verdict. `claim` enforces
//! the hold, and `check` reports it. The rules live in
//! [`crate::judge_backstop`]. `backstop-clear` (#15096) is the operator's
//! release for those holds: it rewrites the records themselves.

use std::path::PathBuf;

use crate::comment_trust::TrustPolicy;
use crate::forge_call_stats::ops;
use crate::gh_invocation::{AccessIntent, GhTarget};
use crate::judge_backstop::{self as backstop, CiRead, CiState};
use crate::step_role::StepRole;
use chrono::{DateTime, Utc};

use super::{Ctx, Gh, Role, Row};

/// `claim` or `check`: a Judge held by the backstop. Like 4 and 5, the prompt's
/// `[[ $? =~ ^[02]$ ]]` reads it as "stand down; next PR".
pub(crate) const EXIT_BACKSTOP: i32 = 6;

/// `backstop-clear`: a read, the write target or a record rewrite failed.
/// Deliberately not one of the lease-state codes (0/3/4/5/6/8) a caller may
/// branch on: this verb has no lease state, only done-or-failed.
pub(crate) const EXIT_CLEAR_FAILED: i32 = 1;

impl Gh {
    /// `(open, head sha, labels)` for `pr`; `None` when unreadable.
    pub(crate) fn pr_head(&self, pr: u64) -> Option<(bool, String, Vec<String>)> {
        let out = self
            .inv("pr_lease.pr_head", AccessIntent::Read, ops::PR_VIEW_STATE)
            .args([
                "api",
                &format!("repos/{{owner}}/{{repo}}/pulls/{pr}"),
                "--jq",
                "{state: .state, head: .head.sha, labels: [.labels[].name]}",
            ])
            .run();
        let v: serde_json::Value = serde_json::from_slice(&out.ok_output()?.stdout).ok()?;
        let open = v.get("state")?.as_str()? == "open";
        let head = v.get("head")?.as_str()?.to_string();
        let labels = v
            .get("labels")?
            .as_array()?
            .iter()
            .filter_map(|l| l.as_str().map(str::to_string))
            .collect();
        Some((open, head, labels))
    }

    /// `sha`'s CI as the backstop sees it ([`backstop::ci_state`]).
    pub(crate) fn ci_state(&self, sha: &str) -> CiState {
        backstop::ci_state(sha, Utc::now(), |kind, path| {
            let (name, op) = match kind {
                CiRead::CheckRuns => ("pr_lease.checks_pending", ops::CI_CHECK_RUNS_FOR_SHA),
                CiRead::CommitStatus => ("pr_lease.commit_status", ops::CI_COMMIT_STATUS),
            };
            let out = self
                .inv(name, AccessIntent::Read, op)
                .args(["api", &format!("repos/{{owner}}/{{repo}}/{path}")])
                .run();
            serde_json::from_slice(&out.ok_output()?.stdout).ok()
        })
    }

    /// The open PRs carrying `label`, by number, newest first: one paged
    /// `pulls?state=open` read (the REST listing has no label filter, so the
    /// `--jq` filters client-side, the `pr_planning` shape). `Err` names why
    /// a failed read failed, so the operator's stand-down is diagnosable.
    pub(crate) fn open_prs_with_label(&self, label: &str) -> Result<Vec<u64>, String> {
        let jq = format!(r#".[] | select(any(.labels[].name; . == "{label}")) | .number"#);
        let slug = self.read_slug();
        let target = slug
            .as_deref()
            .and_then(|s| GhTarget::repo(s).ok())
            .unwrap_or(GhTarget::None);
        let repo_path = slug.unwrap_or_else(|| "{owner}/{repo}".to_string());
        let out = self
            .inv_at("pr_lease.prs_labelled", AccessIntent::Read, ops::PR_LIST_OPEN, target)
            .args([
                "api",
                &format!(
                    "repos/{repo_path}/pulls?state=open&sort=created&direction=desc&per_page=100"
                ),
                "--paginate",
                "--jq",
                &jq,
            ])
            .run();
        let out = match out.into_result() {
            Ok(o) if o.status.success() => o,
            Ok(o) => {
                let err = String::from_utf8_lossy(&o.stderr);
                let line = err
                    .lines()
                    .rev()
                    .find(|l| !l.trim().is_empty())
                    .unwrap_or("");
                return Err(super::bounded_reason(line));
            }
            Err(u) => return Err(super::bounded_reason(&u.to_string())),
        };
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| line.trim().parse().ok())
            .collect())
    }
}

/// The fields a Judge release appends to its marker: the no-verdict record
/// when the PR is still waiting for review with no verdict, else nothing (a
/// verdict landed, the PR closed, or its state could not be read). A pass that
/// deferred because the head's checks are still running (`ci_exempts`, asked
/// only once a record is otherwise due; [`CiState::exempts`]) is not a
/// blocked pass: CI settling changes neither the head nor the labels, so a
/// record would hold the PR from Judge long after CI went green. `refused`
/// is the gate-refusal token of the release's skip reason, if any.
pub(crate) fn release_fields(
    role: Role,
    state: Option<(bool, String, Vec<String>)>,
    refused: Option<&str>,
    ci_exempts: impl FnOnce(&str) -> bool,
) -> String {
    match state {
        // A pass a gate refused (#13135) is recorded as such whatever CI is
        // doing: it reached a verdict, so CI settling is not why it ended.
        Some((open, head, labels))
            if role == Role::Judge && backstop::ended_without_verdict(open, &labels) =>
        {
            match refused {
                Some(token) => backstop::refused_fields(&head, &labels, token),
                None if ci_exempts(&head) => String::new(),
                None => backstop::released_fields(&head, &labels),
            }
        }
        _ => String::new(),
    }
}

/// The fields the reap of a terminated Judge appends (#11931): the no-verdict
/// record, `reaped=<reason>` added, when the PR is still waiting for review
/// with no verdict; else nothing. Unlike [`release_fields`] there is no CI
/// exemption: a pass the role runner killed at its timeout ceiling spent the
/// whole ceiling, and is exactly the re-run the backstop exists to bound.
/// A pass that never reached a review (`seat_busy`, a rejected launch, an
/// exhausted provider; [`crate::pr_lease_reap::reason_never_reviewed`]) is
/// not recorded (#14951): a runtime fault is not a no-verdict review.
pub(crate) fn reaped_release_fields(
    role: Role,
    state: Option<(bool, String, Vec<String>)>,
    reason: &str,
) -> String {
    match state {
        Some((open, head, labels))
            if !crate::pr_lease_reap::reason_never_reviewed(reason)
                && role == Role::Judge
                && backstop::ended_without_verdict(open, &labels) =>
        {
            backstop::reaped_fields(&head, &labels, reason)
        }
        _ => String::new(),
    }
}

/// The typed outcome a Champion release records (#12616), as the pair its
/// marker's fields and the step's journal (#14639) both carry: the step's
/// `CHAMPION-STEP` outcome word (`none` when it wrote no line — a step that
/// died), and for a `requeued` / `skipped` outcome the gate it stopped at,
/// `reason=<token>` (#12715; `unspecified` when it named none). Anything but
/// `[A-Za-z0-9_-]` is dropped so the word cannot break the marker. `None`
/// when this release records no typed outcome: another role, a promotion
/// step, or a merge (the marker keeps only its identity).
pub(crate) fn champion_outcome(
    role: Role,
    step: Option<StepRole>,
    outcome: Option<&str>,
    stop_reason: Option<&str>,
) -> Option<(String, Option<String>)> {
    if role != Role::Champion || step == Some(StepRole::ChampionPromote) {
        return None;
    }
    let token = |raw: Option<&str>, max: usize| -> String {
        raw.unwrap_or_default()
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .take(max)
            .collect()
    };
    let word = token(outcome, 32);
    match word.as_str() {
        "merged" => None,
        "" => Some(("none".to_string(), None)),
        w @ ("requeued" | "skipped") => {
            let reason = token(stop_reason, 32);
            let reason = if reason.is_empty() {
                "unspecified"
            } else {
                &reason
            };
            Some((w.to_string(), Some(reason.to_string())))
        }
        w => Some((w.to_string(), None)),
    }
}

/// The marker fields of [`champion_outcome`]'s pair.
pub(crate) fn format_champion_outcome(word: &str, reason: Option<&str>) -> String {
    match (word, reason) {
        (w, Some(r)) => format!(" outcome={w} reason={r}"),
        ("none", None) => " outcome=none".to_string(),
        (w, None) => format!(" outcome={w}"),
    }
}

/// When the backstop's hold on this PR ends, from its trusted lease `rows`
/// and its live `(open, head, labels)`; `None` when not held. Pure.
#[cfg(test)]
pub(crate) fn hold(
    rows: &[Row],
    state: Option<(bool, String, Vec<String>)>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    hold_with_reason(rows, state, now).map(|(until, _)| until)
}

/// [`hold`] plus the gate-refusal token behind it, when the hold is the
/// short cool-down of a gate-refused pass (#13135).
pub(crate) fn hold_with_reason(
    rows: &[Row],
    state: Option<(bool, String, Vec<String>)>,
    now: DateTime<Utc>,
) -> Option<(DateTime<Utc>, Option<String>)> {
    let (open, head, labels) = state?;
    if !open {
        return None;
    }
    let records: Vec<_> = rows
        .iter()
        .filter_map(|r| backstop::parse(&r.body).map(|nv| (r.updated_at, nv)))
        .collect();
    backstop::hold(&records, &head, &backstop::label_digest(&labels), now)
}

/// The comment ids `backstop-clear` rewrites on a PR whose current head is
/// `head`: every trusted in-window no-verdict record at that head — plain,
/// `reaped=` and `refused=` alike, whatever its label digest (a
/// digest-mismatched record would hold the PR again if its labels reverted).
/// A record at another head, or out of window, is left alone. Pure.
#[must_use]
pub(crate) fn clearable(rows: &[Row], head: &str, now: DateTime<Utc>) -> Vec<u64> {
    let head = head.to_ascii_lowercase();
    let window = chrono::Duration::hours(backstop::WINDOW_HOURS);
    rows.iter()
        .filter(|r| backstop::parse(&r.body).is_some_and(|nv| nv.head == head))
        .filter(|r| now - r.updated_at < window)
        .map(|r| r.id)
        .collect()
}

/// Whether `lease pr check --role judge` would report this PR
/// `held-backstop`: an active no-verdict hold and no live Judge lease (a
/// live lease takes precedence over the hold). Pure.
pub(crate) fn held_backstop(
    rows: &[Row],
    state: Option<(bool, String, Vec<String>)>,
    now: DateTime<Utc>,
    ttl: chrono::Duration,
) -> bool {
    hold_with_reason(rows, state, now).is_some()
        && super::live_leases(rows, Role::Judge, now, ttl).is_empty()
}

/// Which PRs one `backstop-clear` run covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClearTarget {
    /// `--pr N`: clear this PR's records whatever holds it.
    Pr(u64),
    /// `--all`: clear only PRs `check` would answer `held-backstop`.
    All,
}

/// One PR's share of `backstop-clear`: rewrite its in-window no-verdict
/// records at its current head ([`clear_record`]). `only_if_held` (the
/// `--all` shape) rewrites nothing unless the PR is [`held_backstop`], the
/// condition `lease pr check --role judge` reports. Only trusted rows were
/// read (`Gh::read_rows_retrying_why`), so an untrusted author's record is
/// never rewritten; `Err` names why nothing could be done.
fn clear_pr(
    gh: &Gh,
    policy: &TrustPolicy,
    pr: u64,
    only_if_held: bool,
    ttl: chrono::Duration,
) -> Result<usize, String> {
    let rows = gh.read_rows_retrying_why(pr, policy)?;
    let state = gh.pr_head(pr);
    let Some((open, head, _)) = state.as_ref() else {
        return Err(format!("could not read PR #{pr}'s head; nothing rewritten"));
    };
    if !*open || (only_if_held && !held_backstop(&rows, state.clone(), Utc::now(), ttl)) {
        return Ok(0);
    }
    let mut cleared = 0;
    for id in clearable(&rows, head, Utc::now()) {
        let Some(row) = rows.iter().find(|r| r.id == id) else {
            continue;
        };
        let Some(body) = backstop::clear_record(&row.body) else {
            continue;
        };
        match gh.patch(id, &body) {
            Ok(()) => cleared += 1,
            // The comment is gone (404): it can no longer hold anything.
            Err(true) => {}
            Err(false) => return Err(format!("could not rewrite comment {id}")),
        }
    }
    Ok(cleared)
}

/// `backstop-clear` over an already-resolved checkout (tests enter here;
/// [`backstop_clear`] resolves the checkout and builds `gh`). Prints one
/// `BACKSTOP_CLEAR pr=<n> cleared=<k>` line per PR (always for an explicit
/// `--pr`, only when something cleared under `--all`), and returns the exit
/// code: 0 done, [`EXIT_CLEAR_FAILED`] when any read or rewrite failed —
/// a later PR still runs, so one bad PR cannot hide the rest.
pub(crate) fn backstop_clear_on(
    gh: &Gh,
    policy: &TrustPolicy,
    target: ClearTarget,
    ttl: chrono::Duration,
) -> i32 {
    let prs = match target {
        ClearTarget::Pr(pr) => vec![pr],
        ClearTarget::All => {
            // The `held-backstop` condition only exists while the backstop
            // is on (`check` reports `free` with it off); nothing is held.
            if !backstop::enabled() {
                eprintln!(
                    "lease pr backstop-clear: the backstop is off ({}=0); nothing is held",
                    backstop::DISABLE_ENV
                );
                return super::EXIT_HELD;
            }
            match gh.open_prs_with_label(backstop::REVIEW_LABEL) {
                Ok(prs) => prs,
                Err(why) => {
                    eprintln!(
                        "lease pr backstop-clear: could not list open {} PRs: {why}",
                        backstop::REVIEW_LABEL
                    );
                    return EXIT_CLEAR_FAILED;
                }
            }
        }
    };
    let only_if_held = target == ClearTarget::All;
    let mut failed = false;
    let mut total = 0;
    for pr in prs {
        match clear_pr(gh, policy, pr, only_if_held, ttl) {
            Ok(cleared) => {
                if cleared > 0 || matches!(target, ClearTarget::Pr(_)) {
                    println!("BACKSTOP_CLEAR pr={pr} cleared={cleared}");
                }
                total += cleared;
            }
            Err(why) => {
                eprintln!("lease pr backstop-clear: PR #{pr}: {why}");
                failed = true;
            }
        }
    }
    if target == ClearTarget::All {
        eprintln!("lease pr backstop-clear: cleared {total} no-verdict record(s) in total");
    }
    if failed {
        EXIT_CLEAR_FAILED
    } else {
        super::EXIT_HELD
    }
}

/// The CLI verb's entry: resolve the checkout, vet the write target, and run
/// [`backstop_clear_on`]. `loom-tools` refuses this verb
/// (`PrLeaseAction::run_tools`): it is an operator write verb of
/// `loom-daemon`.
pub(crate) fn backstop_clear(args: &super::BackstopClearArgs) -> i32 {
    let root = match crate::repo_root::resolve_repo_root(&args.workspace.to_string_lossy()) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("lease pr backstop-clear: not inside a Loom checkout ({e})");
            return EXIT_CLEAR_FAILED;
        }
    };
    use crate::forge_cmd::{detect_forge, ForgeType};
    if detect_forge(Some(&root)) == ForgeType::Gitea {
        eprintln!(
            "lease pr backstop-clear: PR leases are GitHub-only; on Gitea there is nothing to clear"
        );
        return super::EXIT_HELD;
    }
    let gh = Gh {
        bin: PathBuf::from(std::env::var("LOOM_GH_BIN").unwrap_or_else(|_| "gh".into())),
        root: root.clone(),
        write_repo: super::write_target(&root),
    };
    if let Err(why) = &gh.write_repo {
        eprintln!("lease pr backstop-clear: refusing to write here (#9548): {why}");
        return EXIT_CLEAR_FAILED;
    }
    let policy = TrustPolicy::for_root(&root);
    let ttl_minutes = crate::claim_reconciliation::resolve_lease_ttl_minutes();
    let target = match (args.pr, args.all) {
        (Some(pr), _) => ClearTarget::Pr(pr),
        (None, true) => ClearTarget::All,
        (None, false) => {
            eprintln!("lease pr backstop-clear: pass --pr N or --all");
            return EXIT_CLEAR_FAILED;
        }
    };
    backstop_clear_on(&gh, &policy, target, chrono::Duration::seconds((ttl_minutes * 60.0) as i64))
}

impl Ctx {
    /// Active hold for an enabled Judge. Callers consult this when the role's lease is free.
    pub(super) fn backstop_hold(&self, rows: &[Row]) -> Option<(DateTime<Utc>, Option<String>)> {
        if self.me.role != Role::Judge || !backstop::enabled() {
            return None;
        }
        hold_with_reason(rows, self.gh.pr_head(self.pr()), (self.now)())
    }

    /// `claim`'s backstop check: `Some(exit)` when this Judge must stand down.
    pub(crate) fn backstop_refusal(&self, rows: &[Row]) -> Option<i32> {
        let (until, refused) = self.backstop_hold(rows)?;
        let why = match refused {
            Some(token) => format!(
                "the last pass was refused by a gate (`{token}`), not a looping pass (#13135)"
            ),
            None => "Judge passes at this head and label set already ended without a verdict \
                     (#11855)"
                .to_string(),
        };
        self.say(&format!(
            "BACKSTOP: {why}; held until {} — stand down, next PR (a push or a change to the \
             PR's `loom:*` label set releases it — removing and re-adding the same label does \
             not; {}=0 disables)",
            until.to_rfc3339(),
            backstop::DISABLE_ENV
        ));
        Some(EXIT_BACKSTOP)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// [`backstop::ci_state`] with `Gh::ci_state`'s op names and full paths.
    fn ci_state(
        sha: &str,
        now: DateTime<Utc>,
        mut get: impl FnMut(&str, &str) -> Option<serde_json::Value>,
    ) -> CiState {
        backstop::ci_state(sha, now, |kind, path| {
            let op = match kind {
                CiRead::CheckRuns => "pr_lease.checks_pending",
                CiRead::CommitStatus => "pr_lease.commit_status",
            };
            get(op, &format!("repos/{{owner}}/{{repo}}/{path}"))
        })
    }

    fn ci_pending(
        sha: &str,
        now: DateTime<Utc>,
        get: impl FnMut(&str, &str) -> Option<serde_json::Value>,
    ) -> bool {
        ci_state(sha, now, get) == CiState::Young
    }

    const HEAD: &str = "3333333333333333333333333333333333333333";

    fn state(labels: &[&str]) -> Option<(bool, String, Vec<String>)> {
        Some((true, HEAD.into(), labels.iter().map(|l| (*l).to_string()).collect()))
    }

    fn row(id: u64, at: DateTime<Utc>, fields: &str) -> Row {
        Row {
            id,
            updated_at: at,
            body: format!("<!-- loom:pr-lease-released role=judge host=h sweep=s{id}{fields} -->"),
        }
    }

    #[test]
    fn a_seat_busy_exit_does_not_stamp_a_judge_no_verdict() {
        let waiting = ["loom:review-requested", "loom:reviewing"];
        assert_eq!(reaped_release_fields(Role::Judge, state(&waiting), "seat_busy"), "");
        assert!(
            reaped_release_fields(Role::Judge, state(&waiting), "exited").contains("verdict=none")
        );
    }

    #[test]
    fn a_pass_that_never_reached_a_review_does_not_stamp_a_judge_no_verdict() {
        use crate::pr_lease_reap::{REASON_LAUNCH_REJECTED, REASON_PROVIDER_EXHAUSTED};
        let waiting = ["loom:review-requested", "loom:reviewing"];
        for reason in [REASON_LAUNCH_REJECTED, REASON_PROVIDER_EXHAUSTED] {
            assert_eq!(reaped_release_fields(Role::Judge, state(&waiting), reason), "", "{reason}");
        }
        // A Judge that ran and ended without a verdict still counts toward the hold.
        for reason in ["exited", "timeout"] {
            let fields = reaped_release_fields(Role::Judge, state(&waiting), reason);
            assert_eq!(
                fields,
                format!(" head={HEAD} verdict=none labels=review-requested reaped={reason}")
            );
        }
    }

    #[test]
    fn only_a_judge_release_without_a_verdict_is_stamped() {
        let waiting = ["loom:review-requested", "loom:reviewing"];
        let stamped = release_fields(Role::Judge, state(&waiting), None, |_| false);
        assert_eq!(stamped, format!(" head={HEAD} verdict=none labels=review-requested"));
        assert_eq!(release_fields(Role::Doctor, state(&waiting), None, |_| false), "");
        assert_eq!(release_fields(Role::Judge, state(&["loom:pr"]), None, |_| false), "");
        assert_eq!(release_fields(Role::Judge, None, None, |_| false), "");
    }

    #[test]
    fn a_pass_deferred_on_pending_checks_is_not_stamped() {
        let waiting = ["loom:review-requested", "loom:reviewing"];
        let mut asked = None;
        let fields = release_fields(Role::Judge, state(&waiting), None, |head| {
            asked = Some(head.to_string());
            true
        });
        assert_eq!(fields, "");
        assert_eq!(asked.as_deref(), Some(HEAD));
        // Never asked when no record would be written anyway.
        let fields = release_fields(Role::Judge, state(&["loom:pr"]), None, |_| panic!("asked"));
        assert_eq!(fields, "");
    }

    #[test]
    fn stamped_releases_hold_the_pr_and_plain_ones_do_not() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let fields =
            release_fields(Role::Judge, state(&["loom:review-requested"]), None, |_| false);
        let earlier = now - chrono::Duration::minutes(10);
        let plain = [row(1, earlier, "")];
        assert_eq!(hold(&plain, state(&["loom:review-requested"]), now), None);
        let stamped = [row(1, earlier, &fields)];
        assert_eq!(
            hold(&stamped, state(&["loom:review-requested", "loom:reviewing"]), now),
            Some(earlier + chrono::Duration::hours(1))
        );
        // A closed PR, or one whose state is unreadable, is never held.
        assert_eq!(hold(&stamped, None, now), None);
        let closed = Some((false, HEAD.to_string(), vec!["loom:review-requested".to_string()]));
        assert_eq!(hold(&stamped, closed, now), None);
    }

    fn runs(total: usize, page: usize, pending_at: Option<&str>) -> serde_json::Value {
        let mut rows: Vec<serde_json::Value> = (0..page)
            .map(|_| serde_json::json!({"status": "completed", "started_at": "2026-10-14T05:00:00Z"}))
            .collect();
        if let Some(at) = pending_at {
            rows.pop();
            rows.push(serde_json::json!({"status": "in_progress", "started_at": at}));
        }
        serde_json::json!({"total_count": total, "check_runs": rows})
    }

    fn status(statuses: serde_json::Value) -> serde_json::Value {
        serde_json::json!({"state": "pending", "statuses": statuses})
    }

    /// Answers `ci_pending`'s reads from `pages` (check-run page N at index
    /// N-1) and `combined`, recording every path asked.
    fn read_ci(
        pages: &[serde_json::Value],
        combined: Option<serde_json::Value>,
        now: DateTime<Utc>,
    ) -> (bool, Vec<String>) {
        let mut asked = Vec::new();
        let pending = ci_pending(HEAD, now, |op, path| {
            asked.push(format!("{op} {path}"));
            if path.contains("/status?") {
                return combined.clone();
            }
            let page = path
                .split("&page=")
                .nth(1)
                .map_or(1, |n| n.parse::<usize>().unwrap());
            pages.get(page - 1).cloned()
        });
        (pending, asked)
    }

    #[test]
    fn a_pending_run_past_the_first_hundred_is_read() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let pages = [
            runs(150, 100, None),
            runs(150, 50, Some("2026-10-14T05:30:00Z")),
        ];
        let (pending, asked) = read_ci(&pages, None, now);
        assert!(pending, "the in-progress run on page 2 exempts the pass");
        let base = format!("repos/{{owner}}/{{repo}}/commits/{HEAD}/check-runs?per_page=100");
        assert_eq!(
            asked,
            vec![
                format!("pr_lease.checks_pending {base}"),
                format!("pr_lease.checks_pending {base}&page=2"),
            ],
            "stops at the exempting page, before the statuses"
        );
        // All 150 completed: both pages read, then the statuses; not pending.
        let pages = [runs(150, 100, None), runs(150, 50, None)];
        let (pending, asked) = read_ci(&pages, Some(status(serde_json::json!([]))), now);
        assert!(!pending);
        assert_eq!(asked.len(), 3);
        assert!(asked[2].starts_with("pr_lease.commit_status "));
        assert!(asked[2].ends_with(&format!("commits/{HEAD}/status?per_page=100")));
    }

    #[test]
    fn check_run_paging_is_bounded() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let pages: Vec<_> = (0..20).map(|_| runs(2000, 100, None)).collect();
        let (pending, asked) = read_ci(&pages, None, now);
        assert!(!pending);
        assert_eq!(asked.len(), backstop::MAX_CHECK_RUN_PAGES + 1, "capped pages + statuses");
        // A short page ends the paging even when total_count says more.
        let (_, asked) = read_ci(&[runs(500, 40, None)], None, now);
        assert_eq!(asked.len(), 2);
    }

    /// #11972: stuck CI (pending, but all of it past the window or
    /// unstamped) is told apart from settled CI, and only a `ci-pending`
    /// skip is exempted by settled CI.
    #[test]
    fn ci_state_tells_stuck_from_settled() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let one =
            |run: serde_json::Value| serde_json::json!({"total_count": 1, "check_runs": [run]});
        let state = |page: serde_json::Value, combined: Option<serde_json::Value>| {
            ci_state(HEAD, now, |_, path| {
                if path.contains("/status?") {
                    combined.clone()
                } else {
                    Some(page.clone())
                }
            })
        };
        let queued = one(serde_json::json!({"status": "queued", "started_at": null}));
        assert_eq!(state(queued, None), CiState::Stuck);
        let stale =
            one(serde_json::json!({"status": "in_progress", "started_at": "2026-10-13T20:00:00Z"}));
        assert_eq!(state(stale, None), CiState::Stuck);
        let young =
            one(serde_json::json!({"status": "in_progress", "started_at": "2026-10-14T05:30:00Z"}));
        assert_eq!(state(young, None), CiState::Young);
        assert_eq!(state(runs(1, 1, None), None), CiState::Settled);
        // A stale pending legacy status is stuck too.
        let stale_status = status(serde_json::json!([
            {"state": "pending", "context": "b", "created_at": "2026-10-13T20:00:00Z"},
        ]));
        assert_eq!(state(runs(1, 1, None), Some(stale_status)), CiState::Stuck);
        for skipped in [false, true] {
            assert!(CiState::Young.exempts(skipped));
            assert!(!CiState::Stuck.exempts(skipped));
            assert_eq!(CiState::Settled.exempts(skipped), skipped);
        }
    }

    #[test]
    fn a_pending_legacy_status_exempts_the_pass() {
        let now = Utc.with_ymd_and_hms(2026, 10, 14, 6, 0, 0).unwrap();
        let done = [runs(1, 1, None)];
        let young = status(serde_json::json!([
            {"state": "success", "context": "a", "created_at": "2026-10-14T05:00:00Z"},
            {"state": "pending", "context": "b", "created_at": "2026-10-14T05:45:00Z"},
        ]));
        assert!(read_ci(&done, Some(young), now).0);
        // A pending status stuck past the window, or none pending at all,
        // does not exempt; nor does the bare combined `pending` GitHub
        // reports for a commit with no statuses.
        let stale = status(serde_json::json!([
            {"state": "pending", "context": "b", "created_at": "2026-10-13T20:00:00Z"},
        ]));
        assert!(!read_ci(&done, Some(stale), now).0);
        assert!(!read_ci(&done, Some(status(serde_json::json!([]))), now).0);
        // Unreadable check runs still consult the statuses.
        let young = status(serde_json::json!([
            {"state": "pending", "context": "b", "created_at": "2026-10-14T05:45:00Z"},
        ]));
        assert!(read_ci(&[], Some(young), now).0);
        assert!(!read_ci(&[], None, now).0);
    }
}
