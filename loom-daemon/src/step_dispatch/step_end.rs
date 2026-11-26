//! A finished step's outcome, recorded (#11909; the ledger is #11900).
//!
//! When a step ends, the registry queues it
//! (`sweep_registry::step_spawn::ended`). Each step tick drains the queue
//! for every root, with the registry lock released:
//!
//! 1. take the read the claim release made before its restore write
//!    (`sweep_registry::step_spawn::ended::take_post`), or — when nothing
//!    released the claim, or the release noted its own read FAILED
//!    (#14899: a fail-open release, #11480) — read the item once from the
//!    forge (the PR for a PR step, else the issue): its labels, and whether
//!    it is closed;
//! 2. compute the role's [`PostState`] from that read ([`post_state`]), or
//!    `None` when no read succeeded, never a default;
//! 3. read the session's own outcome line from its log
//!    ([`outcome_ledger::parse_outcome_line`]);
//! 4. [`outcome_ledger::classify`] the two, and record the result
//!    ([`SweepRegistry::record_step_outcome`]), which arms the cooldowns the
//!    next draw reads;
//! 5. publish `step.item.{N}.ended` ([`SweepRegistry::emit_step_ended`],
//!    #12152), the step's terminal bus event, from which the narration sink
//!    narrates a merged PR's public-feed `completion`.
//!
//! # The post-state per role
//!
//! `SweepRegistry::step_post_state`, the release's own rule: each claim
//! label replaces its role's ready label, so the ready label's absence proves
//! nothing (a crashed step leaves exactly that). The item moved only when it
//! carries a label the role moves it *to* (`advanced_labels`: Curator
//! `loom:curated` / `loom:issue`, Champion promote `loom:issue` /
//! `loom:triage`, Judge `loom:pr` / `loom:changes-requested`, Doctor
//! `loom:review-requested`), a Builder's PR is open (the release's open-PR
//! hold), or a merge step's PR left `loom:pr`. A claim label still on an item
//! that did not move is held: the step died mid-work, which classifies as
//! `failed` and takes the fast-fail ladder, never a verdict.

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::outcome_ledger::{self, PostState};
use super::session_ledger;
use crate::step_role::StepRole;
use crate::sweep_registry::step_ledger::StepRun;
use crate::sweep_registry::step_spawn::ended::{self, EndedStep};
use crate::sweep_registry::step_spawn::{StepRefusal, StepSpawnRequest};
use crate::sweep_registry::SweepRegistry;

/// The role's post-state from one read of the item: the claim release's own
/// rule ([`SweepRegistry::step_post_state`]). A read taken here, after
/// nothing released the claim, cannot see a Builder's PR, so a Builder read
/// here moved only by its labels. Pure.
#[must_use]
pub fn post_state(role: StepRole, labels: &[String], closed: bool) -> PostState {
    SweepRegistry::step_post_state(role, labels, closed, false)
}

/// The item's labels and closed state from a REST issue object (a PR is an
/// issue there; a merged PR is closed).
#[must_use]
pub fn read_object(v: &serde_json::Value) -> Option<(Vec<String>, bool)> {
    let labels = v
        .get("labels")?
        .as_array()?
        .iter()
        .filter_map(|l| l.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    let closed = v.get("state").and_then(serde_json::Value::as_str) == Some("closed");
    Some((labels, closed))
}

/// The post-state `settle` classifies with (#14899): the claim release's own
/// read when it made one; otherwise — including when the release noted its
/// read FAILED (a fail-open release, #11480, notes `Some(None)`) — one read
/// now, supplied by `read_now`. A noted failed read is the absence of an
/// observation, not an observation of failure: a confirmed verdict write on
/// the item survives the release's unavailable readback through the read-now
/// fallback, and `probe_failed` is reserved for the boundary where no read
/// of the item succeeded at all. Pure.
#[must_use]
fn resolve_post(
    noted: Option<Option<PostState>>,
    read_now: impl FnOnce() -> Option<PostState>,
) -> Option<PostState> {
    noted.flatten().or_else(read_now)
}

/// The session's outcome line in its log, read only in its own dispatch's
/// section of the append-mode per-item log (#14874): text after the next
/// dispatch header is a later dispatch's, not this step's. A log that never
/// carried a header is a launch-time refusal's whole text (the headerless
/// fallback [`seat_dirty_line`] exists for); a log whose headers name only
/// other dispatches supplies no evidence at all.
fn outcome_line(step: &EndedStep) -> Option<outcome_ledger::StepOutcome> {
    let text = String::from_utf8_lossy(&std::fs::read(&step.log_path).ok()?).into_owned();
    match outcome_ledger::dispatch_section(&text, &step.sweep_id) {
        outcome_ledger::DispatchSection::Section(section) => {
            outcome_ledger::parse_outcome_line(section, step.role, step.item)
                .or_else(|| seat_dirty_line(section))
        }
        outcome_ledger::DispatchSection::Headerless(all) => seat_dirty_line(all),
        outcome_ledger::DispatchSection::Absent => None,
    }
}

/// A step that never ran because its seat's private clone refused preparation
/// over a dirty leftover worktree wrote no outcome line (#14458): name the
/// failure `seat_dirty` instead of leaving it `unclassified`. Only the
/// refusal's own prefix counts, so a transcript that merely mentions the
/// word does not.
fn seat_dirty_line(text: &str) -> Option<outcome_ledger::StepOutcome> {
    let marker = format!("preparation refused: {}", outcome_ledger::SEAT_DIRTY);
    text.contains(&marker).then(|| {
        outcome_ledger::StepOutcome::new(
            outcome_ledger::OutcomeClass::Failed,
            outcome_ledger::SEAT_DIRTY,
        )
    })
}

/// The run the ledger records for `step`, given its post-state read. A step
/// whose provider feedback said its api-key seat was exhausted
/// (`step.provider_exhausted`, #13966) records a refusal
/// ([`outcome_ledger::PROVIDER_EXHAUSTED`]) whatever the forge post-state:
/// the launch died on an empty seat, so the pair arms no cooldown and the
/// run is not counted for the repeat cap.
///
/// No cancel carve-out is needed here, unlike the two `step_failure`s: the
/// flag is set only by the exit-time provider feedback, which the cancel
/// path (`reaper::finish_cancel`, the only producer of `StepEnd::Cancelled`)
/// never runs, so a cancelled step cannot carry it.
#[must_use]
pub fn run_of(
    step: &EndedStep,
    line: Option<&outcome_ledger::StepOutcome>,
    post: Option<PostState>,
) -> StepRun {
    let outcome = if step.provider_exhausted {
        outcome_ledger::StepOutcome::new(
            outcome_ledger::OutcomeClass::Refused,
            outcome_ledger::PROVIDER_EXHAUSTED,
        )
    } else {
        let outcome = outcome_ledger::classify(line, post);
        // #14634: the observed classification gap. A step whose launcher
        // rejected its preflight (exit 78, no outcome line, nothing else
        // moved the item) classifies `failed`/`unclassified` — the receipt
        // then says `unclassified` though the launcher's own typed marker
        // names the boundary. Spend that typed signal ONLY on the exact
        // shape it proves — the class stays `failed` (same cooldowns, same
        // fast-fail ladder, same release), only the reason names the cause.
        // An agent's line, a verdict, a timeout, a provider exhaustion or a
        // probe failure never reaches this arm.
        //
        // #15413: an untyped fast exit 78 (`never_started_reason`) names the
        // coarse `launch_rejected`; the typed stage wins when both exist.
        // Any other unexplained exit keeps `unclassified`.
        let rejection = step.launch_rejection.or_else(|| {
            step.attempt
                .launch_rejected
                .then_some(crate::pr_lease_reap::REASON_LAUNCH_REJECTED)
        });
        match rejection {
            Some(reason)
                if line.is_none()
                    && outcome.class == outcome_ledger::OutcomeClass::Failed
                    && outcome.reason == outcome_ledger::UNCLASSIFIED =>
            {
                outcome_ledger::StepOutcome::new(outcome_ledger::OutcomeClass::Failed, reason)
            }
            // A sandbox refusal applies even when the agent wrote an outcome line.
            // A verdict confirmed by the forge still wins (#14455).
            _ if step.sandbox_unavailable
                && outcome.class != outcome_ledger::OutcomeClass::Verdict =>
            {
                outcome_ledger::StepOutcome::new(
                    outcome_ledger::OutcomeClass::Failed,
                    outcome_ledger::SANDBOX_UNAVAILABLE,
                )
            }
            _ => outcome,
        }
    };
    StepRun {
        item: step.item,
        step: step.role,
        sweep_id: step.sweep_id.clone(),
        outcome,
        ran_for: step.ran_for,
        failure_class: step
            .timed_out
            .then(|| crate::pr_lease_reap::REASON_TIMEOUT.to_string()),
        attempt: Some(step.attempt.clone()),
    }
}

/// The ledger's refusal reason for a spawn-time refusal
/// (`sweep_registry::step_spawn::StepRefusal::reason`): a child failing token selection and a
/// claim held elsewhere cool the `(item, step)` down; a lost claim is a
/// counted redraw. A standing admission refusal (#14032: the runtime will
/// not take the launch — a rejected runtime, a model/runtime mismatch, or
/// any other role-tick admission failure) also cools the pair down, through
/// the ledger's state-refusal streak ([`outcome_ledger::Effect::Reclassify`]),
/// so a repeat backs off instead of being redrawn every tick. `None` for a
/// refusal that says nothing about the item (a roll pause, a workspace
/// missing the role's command).
#[must_use]
pub fn ledger_reason(spawn_reason: &str) -> Option<&'static str> {
    match spawn_reason {
        // The role mark normally suffices. record_refusal also cools the
        // item when the probe let a marked role retry or a seat was unavailable.
        "pool_exhausted" | "no_token_pool" => None,
        "token_selection_failed" => Some("pool_gate"),
        "claimed" | "claim_unverified" | "backstop_hold" | "peer_active" => Some("claimed"),
        // A failed spawn cools down like a gate, so it is not respawned on
        // every tick. A missing `/loom:<role>` command is refused before the
        // spawn (`workspace_commands_missing`) and says nothing about the
        // item, so it falls through to `None`.
        "spawn_failed" | "label_flip_failed" => Some("pool_gate"),
        "claim_lost" => Some("claim_lost"),
        // #14032: a standing admission refusal is neither a gate nor a
        // claim, so it takes the state-refusal streak (the `Refused`
        // fall-through in `outcome_ledger::effect_of`): the second identical
        // one arms the refusal cooldown, doubling per further one up to
        // `MAX_SHORT_COOLDOWN`.
        "runtime_rejected" | "model_runtime_mismatch" | "admission_refused" => Some("admission"),
        _ => None,
    }
}

fn refusal_ledger_reason(
    refusal: Option<&StepRefusal>,
    retried_pool: bool,
) -> Option<&'static str> {
    let Some(refusal) = refusal else {
        return ledger_reason("spawn_failed");
    };
    let spawn_reason = refusal.reason;
    if matches!(spawn_reason, "pool_exhausted" | "no_token_pool")
        && (retried_pool || refusal.seat_unavailable)
    {
        return Some("pool_gate");
    }
    // Match the start of the static Codex containment rejection and its
    // preparation cause. A path, diagnostic excerpt or preference summary
    // that mentions the marker is still an ordinary admission refusal.
    if spawn_reason == "runtime_rejected"
        && refusal
            .detail
            .strip_prefix(concat!(
                "unmet capabilities: worktreeIsolation; ",
                "verified private-clone containment could not satisfy it: ",
                "private workspace preparation refused: ",
            ))
            .and_then(|cause| cause.strip_prefix(outcome_ledger::SEAT_DIRTY))
            .is_some_and(|detail| detail.starts_with(": private clone/worktree is dirty;"))
    {
        return Some(outcome_ledger::SEAT_DIRTY);
    }
    ledger_reason(spawn_reason)
}

/// Record a spawn-time refusal of `(item, role)` in the ledger, so the next
/// draw skips the pair while it cools down and the `claim_lost` counter
/// counts (the integrated review's M1).
pub fn record_refusal(
    root: &Path,
    registry: &Arc<Mutex<SweepRegistry>>,
    request: &StepSpawnRequest,
    refusal: Option<&StepRefusal>,
    retried_pool: bool,
) {
    let Some(reason) = refusal_ledger_reason(refusal, retried_pool) else {
        return;
    };
    let run = StepRun {
        item: request.item,
        step: request.role,
        sweep_id: String::new(),
        outcome: outcome_ledger::StepOutcome::new(outcome_ledger::OutcomeClass::Refused, reason),
        ran_for: std::time::Duration::ZERO,
        failure_class: None,
        attempt: None,
    };
    let Ok(mut reg) = registry.lock() else {
        return;
    };
    let mut forge = refusal_forge(&reg, root);
    reg.record_step_outcome(&run, &mut forge);
}

/// The forge a spawn-time refusal's ledger write goes through (#12621): the
/// work item's repo is its target (as in [`settle`], #12606), so the repo
/// owner's writer is selected rather than the process credential.
pub(crate) fn refusal_forge(
    reg: &SweepRegistry,
    root: &Path,
) -> crate::operator_decision::cli::GhForge {
    let slug = reg.open_pr_listing_target().map(|(_, _, slug)| slug);
    crate::operator_decision::cli::GhForge::new(root.to_path_buf(), slug)
        .with_call_site("step_ledger.reeval_ladder")
}

/// Record every step of `root` that ended since the last tick (module docs).
pub fn settle(root: &Path, registry: &Arc<Mutex<SweepRegistry>>) {
    let steps = ended::take(root);
    if steps.is_empty() {
        return;
    }
    let target = registry
        .lock()
        .ok()
        .and_then(|reg| reg.open_pr_listing_target());
    let Some((_, _, slug)) = target else {
        // No forge target this tick (the repo slug did not resolve): keep
        // the steps for the next tick rather than drop their outcomes.
        for step in steps {
            ended::push(root, step);
        }
        return;
    };
    for step in steps {
        // The claim release's own read, taken before its restore write; else
        // one read now (nothing restored the claim, so nothing moved it).
        // #14899: the release's own read when it made one. A noted FAILED
        // read (#11480 released fail-open without one) is not a verdict
        // about the item — it is the absence of an observation, so the
        // read-now fallback runs for it exactly as for a step nothing
        // released. Only both reads failing together is `probe_failed`.
        let post = resolve_post(ended::take_post(root, &step.sweep_id), || {
            crate::comment_trust::records::fetch_issue_object(
                &slug,
                &step.item.to_string(),
                root,
                false,
            )
            .as_ref()
            .and_then(read_object)
            .map(|(labels, closed)| post_state(step.role, &labels, closed))
        });
        let run = run_of(&step, outcome_line(&step).as_ref(), post);
        // #11913: the step's session, for a later step on the same item.
        if let Some(noted) = session_ledger::take_session(root, &step.sweep_id) {
            session_ledger::record_ended(root, step.item, step.role, &noted, chrono::Utc::now());
        }
        log::info!(
            "work_finder: {} step on #{} ({}) classified {} {}",
            step.role,
            step.item,
            step.sweep_id,
            run.outcome.class.as_str(),
            run.outcome.reason
        );
        // #12606: the ladder write is vetted under the writer it carries (the
        // item owner's per-owner credential), not the workspace root's.
        let mut forge =
            crate::operator_decision::cli::GhForge::new(root.to_path_buf(), Some(slug.clone()))
                .with_call_site("step_ledger.reeval_ladder");
        if let Ok(mut reg) = registry.lock() {
            reg.record_step_outcome(&run, &mut forge);
            // #12152: the step's terminal bus event (the narration sink's
            // completion trigger for a merged PR).
            reg.emit_step_ended(&run);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::step_dispatch::outcome_ledger::OutcomeClass;

    fn labels(ls: &[&str]) -> Vec<String> {
        ls.iter().map(|l| (*l).to_string()).collect()
    }

    #[test]
    fn each_role_moves_only_to_its_advanced_labels() {
        let moved = |role, ls: &[&str]| post_state(role, &labels(ls), false).moved;
        assert!(moved(StepRole::Curator, &["loom:curated"]));
        assert!(!moved(StepRole::Curator, &["loom:curating"]));
        assert!(moved(StepRole::ChampionPromote, &["loom:issue"]));
        assert!(moved(StepRole::ChampionPromote, &["loom:triage"]));
        assert!(!moved(StepRole::ChampionPromote, &["loom:curated", "loom:blocked"]));
        assert!(moved(StepRole::Judge, &["loom:pr"]));
        assert!(!moved(StepRole::Doctor, &["loom:changes-requested"]));
        assert!(moved(StepRole::ChampionMerge, &["loom:blocked"]));
        // A PR triage (#12499) moved only once it carries a stage or a hold:
        // an unlabelled PR it left alone did not.
        assert!(moved(StepRole::ChampionMerge, &["loom:review-requested"]));
        assert!(moved(StepRole::ChampionMerge, &["loom:operator"]));
        assert!(!moved(StepRole::ChampionMerge, &[]));
        assert!(!moved(StepRole::ChampionMerge, &["loom:pr", "loom:blocked"]));
        // A Builder whose PR is open moved, whatever its labels say.
        let p = SweepRegistry::step_post_state(
            StepRole::Builder,
            &labels(&["loom:building"]),
            false,
            true,
        );
        assert!(p.moved && !p.claim_held);
    }

    /// A crashed or timed-out step left its claim label where its ready label
    /// was: not a verdict, a held claim, which classifies as `failed`.
    #[test]
    fn a_crashed_builder_and_a_crashed_judge_classify_as_failed() {
        let builder = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        assert!(!builder.moved && builder.claim_held);
        let judge = post_state(StepRole::Judge, &labels(&["loom:reviewing"]), false);
        assert!(!judge.moved && judge.claim_held);
        let line = outcome_ledger::StepOutcome::new(OutcomeClass::Verdict, "approved");
        for post in [builder, judge] {
            assert_eq!(
                outcome_ledger::classify(Some(&line), Some(post)).class,
                OutcomeClass::Failed
            );
            assert_eq!(outcome_ledger::classify(None, Some(post)).class, OutcomeClass::Failed);
        }
        // Closed is closed, never a held claim.
        let p = post_state(StepRole::Judge, &labels(&["loom:reviewing"]), true);
        assert!(p.closed && !p.moved && !p.claim_held);
    }

    #[test]
    fn spawn_refusals_map_onto_the_ledgers_reasons() {
        assert_eq!(ledger_reason("pool_exhausted"), None);
        assert_eq!(ledger_reason("no_token_pool"), None);
        assert_eq!(ledger_reason("token_selection_failed"), Some("pool_gate"));
        assert_eq!(ledger_reason("backstop_hold"), Some("claimed"));
        // #12105: another actor working the PR cools the Doctor down.
        assert_eq!(ledger_reason("peer_active"), Some("claimed"));
        assert_eq!(ledger_reason("claim_unverified"), Some("claimed"));
        assert_eq!(ledger_reason("claim_lost"), Some("claim_lost"));
        assert_eq!(ledger_reason("roll_paused"), None);
        assert_eq!(ledger_reason("spawn_failed"), Some("pool_gate"));
        // #12048: says nothing about the item, so no window doubles against it.
        assert_eq!(ledger_reason("workspace_commands_missing"), None);
        // #14032: a standing admission refusal cools the pair down (through
        // the state-refusal streak), so it is not redrawn on every tick.
        assert_eq!(ledger_reason("runtime_rejected"), Some("admission"));
        assert_eq!(ledger_reason("model_runtime_mismatch"), Some("admission"));
        assert_eq!(ledger_reason("admission_refused"), Some("admission"));
    }

    #[test]
    fn pool_refusal_after_an_optimistic_probe_cools_the_item() {
        for reason in ["pool_exhausted", "no_token_pool"] {
            let refusal = StepRefusal::new(reason, "launch refused");
            assert_eq!(refusal_ledger_reason(Some(&refusal), false), None);
            assert_eq!(refusal_ledger_reason(Some(&refusal), true), Some("pool_gate"));
        }
    }

    #[test]
    fn only_the_dirty_private_preparation_refusal_gets_the_specific_ledger_reason() {
        let isolation = "unmet capabilities: worktreeIsolation; verified private-clone \
                         containment could not satisfy it: ";
        let dirty = "private workspace preparation refused: seat_dirty: private \
                     clone/worktree is dirty; retain and recover its work before reuse\n  M file";
        let refusal = |reason, detail: String| {
            refusal_ledger_reason(Some(&StepRefusal::new(reason, detail)), false)
        };
        assert_eq!(
            refusal("runtime_rejected", format!("{isolation}{dirty}")),
            Some(outcome_ledger::SEAT_DIRTY)
        );
        assert_eq!(
            refusal("runtime_rejected", format!("{isolation}failed to open path: {dirty}")),
            Some("admission")
        );
        assert_eq!(
            refusal("runtime_rejected", format!("candidate skipped: {isolation}{dirty}")),
            Some("admission")
        );
        assert_eq!(refusal("roll_paused", format!("{isolation}{dirty}")), None);
        assert_eq!(refusal_ledger_reason(None, false), Some("pool_gate"));
    }

    /// #14458: a step that died on its seat's dirty leftover worktree wrote no
    /// outcome line; its log's refusal names the reason instead of leaving it
    /// `unclassified`, and a real outcome line still wins.
    #[test]
    fn a_dirty_seat_refusal_in_the_log_classifies_as_seat_dirty() {
        let refusal = "Error: private workspace preparation refused: seat_dirty: private \
                       clone/worktree is dirty; retain and recover its work before reuse";
        let line = seat_dirty_line(refusal).expect("the refusal names seat_dirty");
        assert_eq!(line.class, OutcomeClass::Failed);
        assert_eq!(line.reason, "seat_dirty");
        let run = classify_with(Some(&line));
        assert_eq!(run.reason, "seat_dirty");
        // Prose that merely mentions the word is not the refusal.
        assert!(seat_dirty_line("the seat_dirty reason is documented").is_none());
        assert!(seat_dirty_line("private workspace preparation refused: base is missing").is_none());
    }

    #[test]
    fn headerless_routing_accepts_only_the_launch_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let step = EndedStep {
            sweep_id: "step-judge-4-1".into(),
            item: 4,
            role: StepRole::Judge,
            log_path: dir.path().join("step.log"),
            ran_for: std::time::Duration::from_secs(60),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        for (text, evidence, reason) in [
            (
                "LOOM_STEP_OUTCOME role=judge target=4 class=noop reason=ci_pending\n",
                None,
                outcome_ledger::UNCLASSIFIED,
            ),
            (
                "Error: private workspace preparation refused: seat_dirty: dirty\n",
                Some(outcome_ledger::SEAT_DIRTY),
                outcome_ledger::SEAT_DIRTY,
            ),
        ] {
            std::fs::write(&step.log_path, text).unwrap();
            let line = outcome_line(&step);
            assert_eq!(line.as_ref().map(|outcome| outcome.reason.as_str()), evidence);
            let released = post_state(step.role, &labels(&["loom:review-requested"]), false);
            let run = run_of(&step, line.as_ref(), Some(released));
            assert_eq!(run.outcome.class, OutcomeClass::Failed);
            assert_eq!(run.outcome.reason, reason);
            assert!(outcome_ledger::counts_as_run(&run.outcome));
        }
    }

    #[test]
    fn an_explicit_child_outcome_wins_over_the_dirty_seat_marker_in_its_log() {
        let dir = tempfile::tempdir().unwrap();
        let step = EndedStep {
            sweep_id: "step-curator-14521-1".into(),
            item: 14521,
            role: StepRole::Curator,
            log_path: dir.path().join("step.log"),
            ran_for: std::time::Duration::from_secs(1),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        std::fs::write(
            &step.log_path,
            "==== loom-daemon dispatch: t sweep_id=step-curator-14521-1 item=14521 step=curator ====\n\
             Error: private workspace preparation refused: seat_dirty: private clone/worktree is dirty;\n\
             LOOM_STEP_OUTCOME role=curator target=14521 class=noop reason=already-curated\n",
        )
        .unwrap();
        let outcome = outcome_line(&step).unwrap();
        assert_eq!(outcome.class, OutcomeClass::Noop);
        assert_eq!(outcome.reason, "already_curated");
    }

    /// #14874: routing reads its outcome line only in its own dispatch's
    /// section of the append-mode per-item log. A later dispatch's result
    /// for the same item is not this step's, and a log whose headers name
    /// only other dispatches supplies no evidence at all — not another
    /// dispatch's outcome, not its dirty-seat message.
    #[test]
    fn routing_reads_only_its_own_dispatchs_section() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("step.log");
        let header =
            |id: &str| format!("==== loom-daemon dispatch: t sweep_id={id} item=4 step=judge ====");
        let step = |id: &str| EndedStep {
            sweep_id: id.into(),
            item: 4,
            role: StepRole::Judge,
            log_path: log.clone(),
            ran_for: std::time::Duration::from_secs(60),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        std::fs::write(
            &log,
            format!(
                "{}\nLOOM_STEP_OUTCOME role=judge target=4 class=failed reason=crash\n\
                 {}\nLOOM_STEP_OUTCOME role=judge target=4 class=noop reason=ci_pending\n\
                 {}\nError: private workspace preparation refused: seat_dirty: dirty\n",
                header("j-4-1"),
                header("j-4-2"),
                header("j-4-3"),
            ),
        )
        .unwrap();
        // The requested dispatch's own result — the earlier failure stays
        // the earlier sweep's, the later dirty-seat message the later one's.
        let outcome = outcome_line(&step("j-4-2")).unwrap();
        assert_eq!((outcome.class.as_str(), outcome.reason.as_str()), ("noop", "ci_pending"));
        assert_eq!(outcome_line(&step("j-4-1")).unwrap().class, OutcomeClass::Failed);
        // An empty requested section: the later dispatch's dirty-seat text
        // is not evidence, so there is no line at all.
        std::fs::write(
            &log,
            format!(
                "{}\nLOOM_STEP_OUTCOME role=judge target=4 class=failed reason=crash\n\
                 {}\n{}",
                header("j-4-1"),
                header("j-4-2"),
                header("j-4-3"),
            ),
        )
        .unwrap();
        assert!(outcome_line(&step("j-4-2")).is_none());
        assert!(outcome_line(&step("j-4-4")).is_none(), "no such dispatch");
        // The no-line run keeps the ledger's own classification.
        let run = run_of(&step("j-4-2"), None, None);
        assert_eq!(
            (run.outcome.class.as_str(), run.outcome.reason.as_str()),
            ("failed", outcome_ledger::PROBE_FAILED)
        );
    }

    /// #14874: a Champion `requeued` in its own dispatch yields the routing
    /// line `noop/requeued`; with the line only in a later dispatch and the
    /// requested section empty, routing gets no line.
    #[test]
    fn a_champion_requeued_routes_from_its_own_dispatch_only() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("step.log");
        let header = |id: &str| {
            format!("==== loom-daemon dispatch: t sweep_id={id} item=4 step=champion_merge ====")
        };
        let step = |id: &str| EndedStep {
            sweep_id: id.into(),
            item: 4,
            role: StepRole::ChampionMerge,
            log_path: log.clone(),
            ran_for: std::time::Duration::from_secs(60),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        std::fs::write(
            &log,
            format!("{}\nCHAMPION-STEP mode=pr item=4 outcome=requeued\n", header("c-4-1")),
        )
        .unwrap();
        let run = run_of(&step("c-4-1"), outcome_line(&step("c-4-1")).as_ref(), None);
        assert_eq!((run.outcome.class.as_str(), run.outcome.reason.as_str()), ("noop", "requeued"));
        // The `requeued` only in the later dispatch: no line for the
        // requested one.
        std::fs::write(
            &log,
            format!(
                "{}\n{}\nCHAMPION-STEP mode=pr item=4 outcome=requeued\n",
                header("c-4-1"),
                header("c-4-2"),
            ),
        )
        .unwrap();
        assert!(outcome_line(&step("c-4-1")).is_none());
    }

    /// #14874 + #14168: a bare, backticked and JSON-framed current-dispatch
    /// outcome line all classify the same through routing.
    #[test]
    fn wrapped_outcome_lines_read_through_routing() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("step.log");
        let header = "==== loom-daemon dispatch: t sweep_id=j-4-1 item=4 step=judge ====\n";
        for line in [
            "LOOM_STEP_OUTCOME role=judge target=4 class=noop reason=ci-pending",
            "`LOOM_STEP_OUTCOME role=judge target=4 class=noop reason=ci_pending`",
            "{\"type\":\"text\",\"text\":\"LOOM_STEP_OUTCOME role=judge target=4 class=noop \
             reason=ci_pending\\n\"}",
        ] {
            std::fs::write(&log, format!("{header}{line}\n")).unwrap();
            let step = EndedStep {
                sweep_id: "j-4-1".into(),
                item: 4,
                role: StepRole::Judge,
                log_path: log.clone(),
                ran_for: std::time::Duration::from_secs(60),
                timed_out: false,
                provider_exhausted: false,
                launch_rejection: None,
                sandbox_unavailable: false,
                attempt: ended::AttemptEvidence::default(),
            };
            let outcome = outcome_line(&step).unwrap_or_else(|| panic!("no line read: {line}"));
            assert_eq!((outcome.class.as_str(), outcome.reason.as_str()), ("noop", "ci_pending"));
        }
    }

    fn classify_with(line: Option<&outcome_ledger::StepOutcome>) -> outcome_ledger::StepOutcome {
        // An unchanged item whose claim is still held: the dead step's shape.
        let post = post_state(StepRole::Curator, &labels(&["loom:curating"]), false);
        outcome_ledger::classify(line, Some(post))
    }

    /// #13966: a step whose provider feedback said its api-key seat was
    /// exhausted records a refusal, not a run — the fast-fail ladder and the
    /// repeat cap never see it, and no item cooldown is armed.
    #[test]
    fn a_provider_exhausted_step_records_a_refusal_that_charges_nothing() {
        let step = EndedStep {
            sweep_id: "step-builder-13966-1".into(),
            item: 13966,
            role: StepRole::Builder,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(80),
            timed_out: false,
            provider_exhausted: true,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        // The post-state of a crashed step (claim restored, item unchanged)
        // would classify `failed`; the exhaustion override wins.
        let post = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        assert!(post.claim_held, "fixture: a crashed step's shape");
        let run = run_of(&step, None, Some(post));
        assert_eq!(run.outcome.class, OutcomeClass::Refused);
        assert_eq!(run.outcome.reason, outcome_ledger::PROVIDER_EXHAUSTED);
        // And the refusal charges nothing: not a repeat-cap run, and not the
        // state-refusal effect either, whose second identical refusal in a
        // row arms a doubling window.
        assert!(!outcome_ledger::counts_as_run(&run.outcome));
        assert_eq!(
            outcome_ledger::effect_of(
                StepRole::Builder,
                &run.outcome,
                run.ran_for,
                &outcome_ledger::LedgerConfig::default(),
            ),
            outcome_ledger::Effect::Uncharged
        );
        // Measured on the ledger itself, across consecutive exhaustions (the
        // shape of a multi-seat outage, each launch discovering the next
        // seat): the pair is never cooling, whatever the count.
        let mut ledger = outcome_ledger::OutcomeLedger::default();
        let t0 = chrono::Utc::now();
        for k in 0..6 {
            let at = t0 + chrono::Duration::minutes(k);
            let rec = ledger.record(run.item, run.step, &run.outcome, run.ran_for, at);
            assert_eq!(rec.effect, outcome_ledger::Effect::Uncharged, "exhaustion {k}");
            assert_eq!(rec.cooling_until, None, "exhaustion {k} armed a window");
            assert_eq!(ledger.cooling_cause(run.item, run.step, at), None, "exhaustion {k}");
            assert!(!rec.repeat_capped, "exhaustion {k} counted as a run");
        }
    }

    /// #14455: a Curator step whose Codex sandbox ran nothing records
    /// `failed` / `sandbox_unavailable` (not `none`, not an unclassified
    /// failure), and charges the item nothing: no window, no repeat-cap run.
    #[test]
    fn a_sandbox_unavailable_step_records_failed_and_charges_nothing() {
        let step = EndedStep {
            sweep_id: "step-curator-14455-1".into(),
            item: 14455,
            role: StepRole::Curator,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(20),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: true,
            attempt: ended::AttemptEvidence::default(),
        };
        // Exit 0, item unchanged, no outcome line: unflagged this is
        // `failed` / `unclassified`.
        let post = post_state(StepRole::Curator, &labels(&["loom:curating"]), false);
        let unflagged = EndedStep {
            sandbox_unavailable: false,
            ..step.clone()
        };
        assert_eq!(
            run_of(&unflagged, None, Some(post)).outcome.reason,
            outcome_ledger::UNCLASSIFIED
        );

        let run = run_of(&step, None, Some(post));
        assert_eq!(run.outcome.class, OutcomeClass::Failed);
        assert_eq!(run.outcome.reason, outcome_ledger::SANDBOX_UNAVAILABLE);
        assert!(!outcome_ledger::counts_as_run(&run.outcome));
        let mut ledger = outcome_ledger::OutcomeLedger::default();
        let t0 = chrono::Utc::now();
        for k in 0..6 {
            let at = t0 + chrono::Duration::minutes(k);
            let rec = ledger.record(run.item, run.step, &run.outcome, run.ran_for, at);
            assert_eq!(rec.effect, outcome_ledger::Effect::Uncharged, "run {k}");
            assert_eq!(rec.cooling_until, None, "run {k} armed a window");
            assert!(!rec.repeat_capped, "run {k} counted as a run");
        }
    }

    /// The flag never hides a verdict the forge shows: a step that moved its
    /// item did work, whatever its terminal record says.
    #[test]
    fn a_sandbox_flag_does_not_override_a_verdict_the_forge_shows() {
        let step = EndedStep {
            sweep_id: "step-curator-14455-2".into(),
            item: 14455,
            role: StepRole::Curator,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(20),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: true,
            attempt: ended::AttemptEvidence::default(),
        };
        let post = post_state(StepRole::Curator, &labels(&["loom:curated"]), false);
        assert_eq!(run_of(&step, None, Some(post)).outcome.class, OutcomeClass::Verdict);
    }

    #[test]
    fn a_failed_read_classifies_as_a_failed_probe() {
        let step = EndedStep {
            sweep_id: "step-judge-7-1".into(),
            item: 7,
            role: StepRole::Judge,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(60),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        let run = run_of(&step, None, None);
        assert_eq!(run.outcome.class, OutcomeClass::Failed);
        assert_eq!(run.outcome.reason, outcome_ledger::PROBE_FAILED);
        let obj = serde_json::json!({"state": "closed", "labels": [{"name": "loom:pr"}]});
        assert_eq!(read_object(&obj), Some((labels(&["loom:pr"]), true)));
    }

    /// #14634: a step whose launcher rejected its preflight (the typed
    /// marker carried on `EndedStep`) classifies `failed` with the typed
    /// reason instead of `unclassified`. Everything about the effect — the
    /// `failed` class, the fast-fail ladder, the cooldowns — is unchanged;
    /// only the receipt's reason names the boundary.
    #[test]
    fn a_launch_rejected_step_names_the_preflight_boundary_not_unclassified() {
        let step = |rejection| EndedStep {
            sweep_id: "step-builder-14634-1".into(),
            item: 14634,
            role: StepRole::Builder,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(7),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: rejection,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        // The crashed-step shape: claim still held, no outcome line.
        let post = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        assert!(post.claim_held, "fixture: a crashed step's shape");
        let typed = run_of(&step(Some("preflight_credential")), None, Some(post));
        assert_eq!(typed.outcome.class, OutcomeClass::Failed);
        assert_eq!(typed.outcome.reason, "preflight_credential");
        assert_eq!(typed.failure_class, None);
        // The coarse marker (boundary proven, subcause unknown) classifies
        // too.
        let coarse = run_of(&step(Some("preflight_rejected")), None, Some(post));
        assert_eq!(coarse.outcome.reason, "preflight_rejected");
        // Without the typed signal the shape stays `unclassified` — the
        // exact gap this fixes.
        let untyped = run_of(&step(None), None, Some(post));
        assert_eq!(untyped.outcome.class, OutcomeClass::Failed);
        assert_eq!(untyped.outcome.reason, outcome_ledger::UNCLASSIFIED);
        // Same class ⇒ the same ledger effect: the fast-fail ladder the
        // `unclassified` failure took is exactly what the typed reason takes.
        for run in [&typed, &coarse, &untyped] {
            assert_eq!(
                outcome_ledger::effect_of(
                    StepRole::Builder,
                    &run.outcome,
                    run.ran_for,
                    &outcome_ledger::LedgerConfig::default(),
                ),
                outcome_ledger::Effect::FastFailLadder
            );
        }
    }

    /// #15413: an untyped fast exit 78 with no outcome line reports the coarse
    /// `launch_rejected`; the typed stage outranks it; an unexplained exit
    /// keeps `unclassified`; every case carries its termination evidence, and
    /// the ledger effect is the same fast-fail ladder.
    #[test]
    fn an_untyped_fast_exit_78_is_a_coarse_launch_rejection() {
        let step = |rejection, launch_rejected, termination| EndedStep {
            sweep_id: "step-builder-15413-1".into(),
            item: 15413,
            role: StepRole::Builder,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(9),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: rejection,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence {
                termination,
                host: "host-a".into(),
                ran_for: std::time::Duration::from_secs(9),
                log: ".loom/logs/step-builder-15413.log".into(),
                launch_rejected,
            },
        };
        let post = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        let status = |n| ended::Termination::Status(n);
        let coarse = run_of(&step(None, true, status(78)), None, Some(post));
        assert_eq!(coarse.outcome.reason, "launch_rejected");
        let typed = run_of(&step(Some("preflight_credential"), true, status(78)), None, Some(post));
        assert_eq!(typed.outcome.reason, "preflight_credential", "the typed stage wins");
        // A different unexplained fast exit keeps its fallback reason...
        let unknown = run_of(&step(None, false, status(1)), None, Some(post));
        assert_eq!(unknown.outcome.reason, outcome_ledger::UNCLASSIFIED);
        // ...and an unavailable status is carried as such.
        let unavailable =
            run_of(&step(None, false, ended::Termination::StatusUnavailable), None, Some(post));
        assert_eq!(unavailable.outcome.reason, outcome_ledger::UNCLASSIFIED);
        for (run, expected) in [
            (&coarse, status(78)),
            (&typed, status(78)),
            (&unknown, status(1)),
            (&unavailable, ended::Termination::StatusUnavailable),
        ] {
            let attempt = run.attempt.as_ref().expect("the evidence rides the run");
            assert_eq!(attempt.termination, expected);
            assert_eq!((attempt.host.as_str(), run.ran_for.as_secs()), ("host-a", 9));
            assert_eq!(run.outcome.class, OutcomeClass::Failed);
            assert_eq!(
                outcome_ledger::effect_of(
                    StepRole::Builder,
                    &run.outcome,
                    run.ran_for,
                    &outcome_ledger::LedgerConfig::default(),
                ),
                outcome_ledger::Effect::FastFailLadder
            );
        }
        // An agent's own line, a timeout and a provider exhaustion outrank it.
        let line = outcome_ledger::StepOutcome::new(OutcomeClass::Failed, "no_pr");
        let with_line = run_of(&step(None, true, status(78)), Some(&line), Some(post));
        assert_eq!(with_line.outcome.reason, "no_pr");
        let mut exhausted = step(None, true, status(78));
        exhausted.provider_exhausted = true;
        assert_eq!(
            run_of(&exhausted, None, Some(post)).outcome.reason,
            outcome_ledger::PROVIDER_EXHAUSTED
        );
        let mut timed_out = step(None, true, ended::Termination::TimedOut);
        timed_out.timed_out = true;
        assert_eq!(
            run_of(&timed_out, None, Some(post))
                .failure_class
                .as_deref(),
            Some(crate::pr_lease_reap::REASON_TIMEOUT)
        );
    }

    /// #14634: the typed signal is spent only on the shape it proves. An
    /// agent's own line, a moved item, a timeout, a provider exhaustion or a
    /// probe failure never reads as a launcher rejection.
    #[test]
    fn the_launch_rejection_never_overrides_a_more_specific_classification() {
        let base = EndedStep {
            sweep_id: "step-builder-14634-2".into(),
            item: 14634,
            role: StepRole::Builder,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(600),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: Some("preflight_credential"),
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        // An agent's line outranks: the launcher never rejected a launch the
        // agent ran in.
        let held = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        let line = outcome_ledger::StepOutcome::new(OutcomeClass::Failed, "no_pr");
        let agent = run_of(&base, Some(&line), Some(held));
        assert_eq!(agent.outcome.reason, "no_pr");
        // A moved item is a verdict whatever the launcher said.
        let moved = post_state(StepRole::Builder, &labels(&["loom:building"]), false);
        let open_pr = crate::step_dispatch::outcome_ledger::PostState {
            moved: true,
            closed: false,
            claim_held: false,
        };
        assert!(!moved.moved, "fixture: claim-held is not a move");
        let verdict = run_of(&base, None, Some(open_pr));
        assert_eq!(verdict.outcome.class, OutcomeClass::Verdict);
        // Provider exhaustion keeps its own refusal.
        let exhausted = EndedStep {
            provider_exhausted: true,
            ..base.clone()
        };
        let run = run_of(&exhausted, None, Some(held));
        assert_eq!(run.outcome.class, OutcomeClass::Refused);
        assert_eq!(run.outcome.reason, outcome_ledger::PROVIDER_EXHAUSTED);
        // A timeout keeps its failure class.
        let timed_out = EndedStep {
            timed_out: true,
            ..base.clone()
        };
        let run = run_of(&timed_out, None, Some(held));
        assert_eq!(run.failure_class.as_deref(), Some(crate::pr_lease_reap::REASON_TIMEOUT));
        // A probe failure (forge read failed) is not the launcher's word.
        let probe = run_of(&base, None, None);
        assert_eq!(probe.outcome.reason, outcome_ledger::PROBE_FAILED);
    }

    /// #14899: a noted post-state is final — the fresh read never runs for
    /// it (a release that read the item already spoke; a second read now
    /// would see its own restore write).
    #[test]
    fn a_noted_read_is_final_without_a_fresh_one() {
        let moved = post_state(StepRole::Judge, &labels(&["loom:pr"]), false);
        assert!(moved.moved);
        assert_eq!(
            resolve_post(Some(Some(moved)), || panic!("read_now must not run")),
            Some(moved)
        );
    }

    /// #14899: a release that noted its own read FAILED (#11480 released
    /// fail-open without one) noted the absence of an observation, not an
    /// observation of failure — the settle-time read runs for it exactly as
    /// for a step nothing released, and its result classifies.
    #[test]
    fn a_noted_failed_release_read_falls_through_to_a_fresh_read() {
        let unchanged = post_state(StepRole::Judge, &labels(&["loom:review-requested"]), false);
        assert!(!unchanged.moved && !unchanged.claim_held);
        assert_eq!(resolve_post(Some(None), || Some(unchanged)), Some(unchanged));
        assert_eq!(resolve_post(None, || Some(unchanged)), Some(unchanged));
        // No successful read at all: the honest `probe_failed` boundary.
        assert_eq!(resolve_post(Some(None), || None), None);
        assert_eq!(resolve_post(None, || None), None);
    }

    /// #14899, natural population (this host, 2026-11-20/21): a Judge that
    /// wrote its verdict and emitted its line, whose release's veto read
    /// timed out fail-open, classified `failed probe_failed` — the confirmed
    /// write discarded. With the fresh read at settle, the same note and a
    /// readable item make the write survive; with no readable item at all,
    /// the unsupported approval stays `probe_failed` (unverified), and the
    /// session's line alone still never makes the verdict.
    #[test]
    fn a_confirmed_verdict_write_survives_an_unavailable_release_read() {
        let step = EndedStep {
            sweep_id: "step-judge-7957-1795152260".into(),
            item: 7957,
            role: StepRole::Judge,
            log_path: "/nonexistent".into(),
            ran_for: std::time::Duration::from_secs(240),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        let line = outcome_ledger::StepOutcome::new(OutcomeClass::Verdict, "approved");
        let approved = post_state(StepRole::Judge, &labels(&["loom:pr"]), false);
        assert!(approved.moved, "fixture: `loom:pr` is the Judge's write");
        let post = resolve_post(Some(None), || Some(approved));
        let run = run_of(&step, Some(&line), post);
        assert_eq!(run.outcome.class, OutcomeClass::Verdict);
        assert_eq!(run.outcome.reason, "approved");
        // Both reads unavailable: the line is not trusted alone.
        let post = resolve_post(Some(None), || None);
        let run = run_of(&step, Some(&line), post);
        assert_eq!(run.outcome.class, OutcomeClass::Failed);
        assert_eq!(run.outcome.reason, outcome_ledger::PROBE_FAILED);
    }

    /// #14899: sanitized natural final-output fragments, one per boundary of
    /// the sampled population. The Claude tails carry an emitted line — a
    /// markdown-wrapped `noop ci_pending` (the #14814 bounded-CI handback
    /// vocabulary) and a fenced bare `refused forge-read-shed` — and both
    /// PARSE: their `probe_failed` records were readback failures, not
    /// parser rejections. The Codex tail is a launch-time refusal (hook
    /// trust, exit 1) whose line is genuinely ABSENT: no parse rejects it,
    /// there is nothing to reject.
    #[test]
    fn natural_judge_tails_distinguish_an_absent_line_from_a_parser_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("step-judge-9764.log");
        let header = |id: &str, item: u32| {
            format!("==== loom-daemon dispatch: t sweep_id={id} item={item} step=judge ====")
        };
        let step = |id: &str, item: u32| EndedStep {
            sweep_id: id.into(),
            item,
            role: StepRole::Judge,
            log_path: log.clone(),
            ran_for: std::time::Duration::from_secs(60),
            timed_out: false,
            provider_exhausted: false,
            launch_rejection: None,
            sandbox_unavailable: false,
            attempt: ended::AttemptEvidence::default(),
        };
        // Claude (kicad-tools #9764): the wrapper's markdown tail.
        std::fs::write(
            &log,
            format!(
                "{}\n**Not done, and still required before any approval:** `post-verdict.sh`.\n\
                 **Outcome:** `LOOM_STEP_OUTCOME role=judge target=9764 class=noop \
                 reason=ci_pending`\n[2026-11-20T09:33:59Z] [INFO] Claude wrapper exiting with \
                 code 0\n",
                header("step-judge-9764-1795166921", 9764),
            ),
        )
        .unwrap();
        let outcome = outcome_line(&step("step-judge-9764-1795166921", 9764))
            .expect("the wrapped line parses");
        assert_eq!((outcome.class.as_str(), outcome.reason.as_str()), ("noop", "ci_pending"));
        // With the release's read failed fail-open, the fresh read now
        // classifies the item unchanged and the typed line gets its meaning
        // back: a minute-scale CI cooldown, not the fast-fail ladder.
        let unchanged = post_state(StepRole::Judge, &labels(&["loom:review-requested"]), false);
        let run = run_of(
            &step("step-judge-9764-1795166921", 9764),
            Some(&outcome),
            resolve_post(Some(None), || Some(unchanged)),
        );
        assert_eq!(
            (run.outcome.class.as_str(), run.outcome.reason.as_str()),
            ("noop", "ci_pending")
        );
        // Claude (2am #7957): the typed refusal, fenced and bare.
        std::fs::write(
            &log,
            format!(
                "{}\n```\nLOOM_STEP_OUTCOME role=judge target=7957 class=refused \
                 reason=forge-read-shed\n```\n",
                header("step-judge-7957-1795152260", 7957),
            ),
        )
        .unwrap();
        let outcome = outcome_line(&step("step-judge-7957-1795152260", 7957))
            .expect("the fenced line parses");
        assert_eq!(
            (outcome.class.as_str(), outcome.reason.as_str()),
            ("refused", "forge_read_shed")
        );
        // Codex (kicad-tools #9806): the launch refusal — no session ever
        // ran, so no line exists. Not a parser rejection: absent.
        std::fs::write(
            &log,
            format!(
                "{}\nspawn-worker: runtime=codex\nError: role judge merges or issues verdicts, \
                 and Loom's guard hook is not proven for this private session\n\
                 # LOOM_TERMINAL_RESULT v=2 provider=codex category=RECOVERABLE exit_code=1\n",
                header("step-judge-9806-1795230515", 9806),
            ),
        )
        .unwrap();
        let step = step("step-judge-9806-1795230515", 9806);
        assert!(outcome_line(&step).is_none(), "no line was emitted");
        let run = run_of(&step, None, None);
        assert_eq!(
            (run.outcome.class.as_str(), run.outcome.reason.as_str()),
            ("failed", outcome_ledger::PROBE_FAILED)
        );
    }
}
