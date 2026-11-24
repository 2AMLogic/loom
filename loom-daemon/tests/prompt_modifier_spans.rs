//! #15017: a single-step run that names a prompt modifier says so on
//! `loom.dispatch.step`, `loom.role_attempt` and `loom.runtime.usage`, and its
//! hyperparameter digest differs from the same run's without it.
//!
//! `prompt_modifier::current()` is read once per process, so this lives in its
//! OWN integration-test binary with the env set before anything reads it, and
//! holds exactly one test. The unmodified side of every assertion is covered
//! by the lib unit tests, whose process never sets the variable (their exact
//! attribute counts would break if it leaked there).

use chrono::Utc;
use loom_daemon::hyperparams::{digest_of, resolve_effective};
use loom_daemon::observability::ops::dispatch_step::{self, StepDispatch};
use loom_daemon::observability::runtime_usage::{cost::Pricing, spans};
use loom_daemon::prompt_modifier;
use loom_daemon::script_helpers::sweep_experiment::ModelUsageTotals;
use loom_daemon::step_role::StepRole;
use loom_daemon::telemetry::trace::{
    journal::Journal, provenance, SpanName, TraceAttributes, TraceContext,
};

#[test]
fn a_named_modifier_is_on_all_three_spans_and_in_the_digest() {
    std::env::set_var(prompt_modifier::ENV, "  deep  ");
    assert_eq!(prompt_modifier::current(), Some("deep"), "trimmed once, cached");

    // loom.dispatch.step
    let step = StepDispatch {
        item: 15017,
        role: StepRole::Judge,
        result: dispatch_step::RESULT_SPAWNED,
        reason: "ok",
        draw_attempt: 1,
        age_source: "labels_at",
        resumed: false,
        draw: None,
        story_points: None,
        sweep_id: None,
        claim: None,
        assignment_id: None,
        lineage: None,
        captain: None,
        models: None,
        runtime: Some("claude"),
        effort: None,
        role_draw: None,
    };
    let dispatch = dispatch_step::span(&step, None, Utc::now());
    assert_eq!(dispatch.attributes[provenance::PROMPT_MODIFIER], "deep");
    assert_eq!(dispatch.clone().bounded().attributes[provenance::PROMPT_MODIFIER], "deep");

    // loom.runtime.usage
    let parent = TraceContext::derived("prompt-modifier-test", &["15017"]);
    let at = Utc::now();
    let row = ModelUsageTotals {
        model: "claude-sonnet-5".into(),
        speed: "standard".into(),
        service_tier: "standard".into(),
        input: 1,
        ..ModelUsageTotals::default()
    };
    let mut common = TraceAttributes::new();
    common.insert("loom.runtime".into(), "claude".into());
    let usage = spans::model_usage_spans(
        &parent,
        (at, at),
        &[row],
        spans::UsageScope::Execution,
        &common,
        &Pricing::with(None),
    );
    let [usage] = usage.as_slice() else {
        panic!("one model, one span: {usage:?}");
    };
    assert_eq!(usage.name.as_str(), "loom.runtime.usage");
    assert_eq!(usage.attributes[provenance::PROMPT_MODIFIER], "deep");
    assert_eq!(usage.clone().bounded().attributes[provenance::PROMPT_MODIFIER], "deep");

    // loom.role_attempt, opened through the trace journal every attempt uses.
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("trace"));
    let mut attempt_attrs = TraceAttributes::new();
    attempt_attrs.insert("loom.role".into(), "judge".into());
    let active = journal
        .start(
            TraceContext::derived("prompt-modifier-test", &["15017"]),
            None,
            SpanName::RoleAttempt,
            Utc::now(),
            attempt_attrs,
        )
        .unwrap();
    assert_eq!(active.record.attributes[provenance::PROMPT_MODIFIER], "deep");

    // Never overwritten: a span restored from a queue keeps its creator's value.
    let mut restored = TraceAttributes::new();
    restored.insert(provenance::PROMPT_MODIFIER.into(), "from-the-creator".into());
    provenance::stamp(&mut restored);
    assert_eq!(restored[provenance::PROMPT_MODIFIER], "from-the-creator");

    // The digest: same vector, differs only by the modifier.
    let workspace = tempfile::tempdir().unwrap();
    let resolved = resolve_effective(workspace.path());
    assert_eq!(resolved.prompt_modifier.as_deref(), Some("deep"));
    let bare = digest_of(&resolved.params);
    assert_ne!(resolved.digest(), bare, "the modifier changed the digest");
    let mut other = resolved.clone();
    other.prompt_modifier = Some("wide".into());
    assert_ne!(resolved.digest(), other.digest(), "two modifiers, two digests");
    other.prompt_modifier = None;
    assert_eq!(other.digest(), bare, "unset leaves the digest as it was");
}
