//! #13054: a Builder `noop` / `failed` step posts its reason once.

use super::*;
use crate::step_dispatch::outcome_ledger::StepOutcome;
use crate::step_role::StepRole;
use crate::sweep_registry::test_support::fixture_registry;
use std::time::Duration;
use tempfile::tempdir;

fn run(class: OutcomeClass, reason: &str, sweep: &str) -> StepRun {
    StepRun {
        item: 9,
        step: StepRole::Builder,
        sweep_id: sweep.into(),
        outcome: StepOutcome::new(class, reason),
        ran_for: Duration::from_secs(600),
        failure_class: None,
        attempt: None,
    }
}

#[test]
fn a_noop_notice_carries_the_reason_and_the_marker_excerpt() {
    let body = outcome_notice_body(
        &run(OutcomeClass::Noop, "no_changes_needed", "step-builder-9-1"),
        Some("already fixed on main"),
    )
    .expect("a noop gets a notice");
    assert!(body.starts_with(STEP_OUTCOME_COMMENT_MARKER));
    assert!(body.contains("sweep=step-builder-9-1"));
    assert!(body.contains("`no_changes_needed`"));
    assert!(body.contains("already fixed on main"));
}

#[test]
fn a_failed_notice_names_the_reason_and_a_verdict_gets_none() {
    let body = outcome_notice_body(&run(OutcomeClass::Failed, "unclassified", "s"), None)
        .expect("a failed step gets a notice");
    assert!(body.contains("`unclassified`"));
    assert!(outcome_notice_body(&run(OutcomeClass::Verdict, "pr_opened", "s"), None).is_none());
    assert!(outcome_notice_body(&run(OutcomeClass::Refused, "claimed", "s"), None).is_none());
}

/// #14634: the typed preflight reason flows into the receipt verbatim — the
/// launcher's boundary token, never free-form diagnostic text.
#[test]
fn a_launch_rejected_notice_names_the_preflight_boundary() {
    let body = outcome_notice_body(&run(OutcomeClass::Failed, "preflight_credential", "s"), None)
        .expect("a failed step gets a notice");
    assert!(body.contains("`preflight_credential`"), "{body}");
    assert!(!body.contains("cannot resolve"), "{body}");
    let coarse =
        outcome_notice_body(&run(OutcomeClass::Failed, "preflight_rejected", "s"), None).unwrap();
    assert!(coarse.contains("`preflight_rejected`"), "{coarse}");
}

#[test]
fn the_excerpt_is_the_first_200_chars_of_the_marker_file() {
    let dir = tempdir().unwrap();
    assert_eq!(marker_excerpt(dir.path()), None);
    std::fs::write(dir.path().join(".no-changes-needed"), "x".repeat(500)).unwrap();
    assert_eq!(marker_excerpt(dir.path()).unwrap().len(), 200);
}

#[test]
fn a_step_is_noticed_once_and_an_empty_sweep_id_never() {
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    assert!(reg.claim_outcome_notice("step-builder-9-1"));
    assert!(!reg.claim_outcome_notice("step-builder-9-1"));
    assert!(reg.claim_outcome_notice("step-builder-9-2"));
    assert!(!reg.claim_outcome_notice(""));
}

#[test]
fn recording_a_builder_noop_claims_the_notice() {
    struct NoForge;
    impl crate::step_dispatch::reeval_ladder::BodyForge for NoForge {
        fn body(&mut self, _: u64) -> Result<String, String> {
            Ok(String::new())
        }
        fn set_body(&mut self, _: u64, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let dir = tempdir().unwrap();
    let (mut reg, _) = fixture_registry(dir.path());
    let noop = run(OutcomeClass::Noop, "no_changes_needed", "step-builder-9-1");
    reg.record_step_outcome(&noop, &mut NoForge);
    assert!(
        reg.step_outcome_notices.contains("step-builder-9-1"),
        "noop must post a comment"
    );
    let failed = run(OutcomeClass::Failed, "unclassified", "step-builder-9-2");
    reg.record_step_outcome(&failed, &mut NoForge);
    assert!(
        reg.step_outcome_notices.contains("step-builder-9-2"),
        "failed must post a comment"
    );
    let mut other = run(OutcomeClass::Noop, "no_fix_needed", "step-doctor-9-1");
    other.step = StepRole::Doctor;
    reg.record_step_outcome(&other, &mut NoForge);
    assert!(!reg.step_outcome_notices.contains("step-doctor-9-1"));
}

/// A fake `gh` that keeps the one outcome comment's body: a `issue comment`
/// post creates it, a PATCH replaces it, and the comment listing answers with
/// it as the daemon's `{id, body}` JSON line once it exists.
fn stateful_gh(ws: &std::path::Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let gh = ws.join("fake-gh.sh");
    std::fs::write(
        &gh,
        format!(
            r#"#!/usr/bin/env bash
D="{d}"
case "$*" in
  *"-X PATCH"*)
    echo "$*" >> "$D/patched"
    for a in "$@"; do case "$a" in body=*) printf '%s' "${{a#body=}}" > "$D/body" ;; esac; done ;;
  "api "*"/comments "*)
    if [ -f "$D/readfail" ]; then exit 1; fi
    if [ -f "$D/created" ]; then
      b=$(cat "$D/body"); printf '{{"id":4242,"body":"%s","user":{{"login":"loom-fleet-dispatch[bot]","type":"Bot"}},"author_association":"NONE"}}\n' "${{b//$'\n'/\\n}}"
    fi
    if [ -f "$D/stranger" ]; then
      printf '{{"id":9999,"body":"%s","user":{{"login":"mallory","type":"User"}},"author_association":"NONE"}}\n' "$(cat "$D/stranger")"
    fi ;;
  "issue comment"*)
    echo x >> "$D/created"
    while [ $# -gt 0 ]; do
      if [ "$1" = --body ]; then printf '%s' "$2" > "$D/body"; break; fi
      shift
    done ;;
  *) exit 1 ;;
esac
"#,
            d = ws.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    gh
}

fn registry_with_gh(ws: &std::path::Path) -> crate::sweep_registry::SweepRegistry {
    let mut config = crate::sweep_registry::SweepRegistryConfig::new(ws.to_path_buf());
    config.gh_bin = Some(stateful_gh(ws));
    crate::sweep_registry::SweepRegistry::new(config)
}

#[test]
fn repeated_outcomes_edit_one_comment_in_place() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let mut reg = registry_with_gh(ws);
    for i in 0..5 {
        reg.note_builder_outcome(&run(OutcomeClass::Failed, "unclassified", &format!("s-{i}")));
    }
    let created = std::fs::read_to_string(ws.join("created")).unwrap();
    assert_eq!(created.lines().count(), 1, "one comment posted, the rest edited");
    let patched = std::fs::read_to_string(ws.join("patched")).unwrap();
    assert_eq!(patched.matches("issues/comments/4242").count(), 4);
    assert!(patched.contains("issues/comments/4242"));
    assert!(patched.contains("sweep=s-4"), "latest outcome retained");
}

fn with_attempt(
    mut run: StepRun,
    termination: Termination,
    host: &str,
    log: &str,
    secs: u64,
) -> StepRun {
    run.ran_for = Duration::from_secs(secs);
    run.attempt = Some(AttemptEvidence {
        termination,
        host: host.into(),
        ran_for: Duration::from_secs(secs),
        log: log.into(),
        launch_rejected: false,
    });
    run
}

const LOG: &str = ".loom/logs/step-builder-9.log";

/// #15413: a failed attempt shows the observed status, the elapsed time, and a
/// host-qualified pointer to the dispatch's output.
#[test]
fn a_failed_notice_shows_status_elapsed_host_log_and_dispatch() {
    let failed = with_attempt(
        run(OutcomeClass::Failed, "unclassified", "step-builder-9-77"),
        Termination::Status(1),
        "host-a",
        LOG,
        42,
    );
    let body = outcome_notice_body(&failed, None).unwrap();
    assert!(body.contains("exit status `1`"), "{body}");
    assert!(body.contains("ran 42s on host `host-a`"), "{body}");
    assert!(body.contains(&format!("`{LOG}`")), "{body}");
    assert!(body.contains("sweep `step-builder-9-77`"), "{body}");
    assert!(body.contains("`unclassified`"), "an unexplained exit keeps its cause: {body}");
}

/// #15413: a missing status is stated, a signal-derived status is reported as
/// the observed number, and nothing infers a signal from an ordinary exit.
#[test]
fn the_notice_states_a_missing_status_and_never_infers_a_signal() {
    let render = |termination| {
        outcome_notice_body(
            &with_attempt(
                run(OutcomeClass::Failed, "unclassified", "s"),
                termination,
                "host-a",
                LOG,
                5,
            ),
            None,
        )
        .unwrap()
    };
    assert!(render(Termination::StatusUnavailable).contains("exit status unavailable"));
    let killed = render(Termination::Status(137));
    assert!(killed.contains("exit status `137`"), "{killed}");
    let ordinary = render(Termination::Status(2));
    for body in [&killed, &ordinary] {
        assert!(!body.to_lowercase().contains("signal"), "{body}");
        assert!(!body.contains("SIG"), "{body}");
    }
    assert!(render(Termination::TimedOut).contains("step timeout"));
    assert!(render(Termination::Cancelled).contains("cancelled"));
}

/// #15413: the typed stage, the coarse rejection and an unexplained exit each
/// reach the receipt as their own reason, with diagnostics alongside.
#[test]
fn typed_coarse_and_unknown_reasons_all_show_the_diagnostics() {
    for reason in ["preflight_credential", "launch_rejected", "unclassified"] {
        let body = outcome_notice_body(
            &with_attempt(
                run(OutcomeClass::Failed, reason, "s"),
                Termination::Status(78),
                "host-a",
                LOG,
                3,
            ),
            None,
        )
        .unwrap();
        assert!(body.contains(&format!("(reason: `{reason}`)")), "{body}");
        assert!(body.contains("exit status `78`") && body.contains("ran 3s"), "{body}");
    }
}

/// #15413: attempts on two workers, with two dispatch sections in one worker's
/// log, stay discoverable after the outcome comment has been edited by every
/// later attempt: each attempt's host and section are found from the comment
/// alone, following the lookup the quarantine text describes.
#[test]
fn every_attempt_stays_discoverable_across_hosts_and_edits() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let mut reg = registry_with_gh(ws);
    // Worker logs, one per synthetic host. Host A ran two dispatches into the
    // same append-mode file.
    let header = |sweep: &str| {
        format!("{} sweep_id={sweep} item=9\n", crate::sweep_registry::DISPATCH_HEADER_MARKER)
    };
    for (host, text) in [
        (
            "host-a",
            format!(
                "{}boom A1\n{}boom A2\n",
                header("step-builder-9-1"),
                header("step-builder-9-3")
            ),
        ),
        ("host-b", format!("{}boom B\n", header("step-builder-9-2"))),
    ] {
        std::fs::create_dir_all(ws.join(host).join(".loom/logs")).unwrap();
        std::fs::write(ws.join(host).join(LOG), text).unwrap();
    }
    let attempts = [
        ("step-builder-9-1", "host-a", Termination::Status(1)),
        ("step-builder-9-2", "host-b", Termination::StatusUnavailable),
        ("step-builder-9-3", "host-a", Termination::Status(137)),
    ];
    for (sweep, host, termination) in attempts {
        reg.note_builder_outcome(&with_attempt(
            run(OutcomeClass::Failed, "unclassified", sweep),
            termination,
            host,
            LOG,
            20,
        ));
    }
    assert_eq!(
        std::fs::read_to_string(ws.join("created"))
            .unwrap()
            .lines()
            .count(),
        1,
        "later attempts edit the one comment"
    );
    // The latest edit still lists every attempt.
    let latest = std::fs::read_to_string(ws.join("body")).unwrap();
    assert!(latest.contains("sweep=step-builder-9-3"), "{latest}");
    let listed = prior_attempts(&latest);
    assert_eq!(listed.len(), 3, "{latest}");
    // The quarantine text names this comment and the header to search for.
    let note = attempt_lookup_note();
    assert!(note.contains(STEP_OUTCOME_COMMENT_MARKER), "{note}");
    assert!(note.contains(crate::sweep_registry::DISPATCH_HEADER_MARKER), "{note}");
    // From each listed line alone: host, log, sweep id -> that attempt's text.
    let field = |line: &str, n: usize| line.split('`').nth(n).unwrap().to_string();
    let log_of = |line: &str| line.split('`').rev().nth(1).unwrap().to_string();
    let expected = [
        ("step-builder-9-1", "host-a", "boom A1"),
        ("step-builder-9-2", "host-b", "boom B"),
        ("step-builder-9-3", "host-a", "boom A2"),
    ];
    for (line, (sweep, host, output)) in listed.iter().zip(expected) {
        assert_eq!((field(line, 1).as_str(), field(line, 3).as_str()), (sweep, host), "{line}");
        let text = std::fs::read_to_string(ws.join(field(line, 3)).join(log_of(line))).unwrap();
        match crate::step_dispatch::outcome_ledger::dispatch_section(&text, &field(line, 1)) {
            crate::step_dispatch::outcome_ledger::DispatchSection::Section(section) => {
                assert_eq!(section.trim(), output, "{line}");
            }
            other => panic!("no section for {line}: {other:?}"),
        }
    }
    // A retry of the same dispatch replaces its line instead of duplicating it.
    reg.step_outcome_notices.clear();
    reg.note_builder_outcome(&with_attempt(
        run(OutcomeClass::Failed, "unclassified", "step-builder-9-3"),
        Termination::Status(9),
        "host-a",
        LOG,
        21,
    ));
    let after = std::fs::read_to_string(ws.join("body")).unwrap();
    assert_eq!(prior_attempts(&after).len(), 3, "{after}");
    assert!(after.contains("exit status `9`"), "{after}");
}

/// #15413: a no-op after failures keeps the earlier attempt lines, and the
/// list is bounded.
#[test]
fn a_noop_carries_prior_attempts_and_the_list_is_bounded() {
    let failed = |i: usize| {
        with_attempt(
            run(OutcomeClass::Failed, "unclassified", &format!("s-{i}")),
            Termination::Status(1),
            "host-a",
            LOG,
            1,
        )
    };
    let mut body = String::new();
    for i in 0..12 {
        let prior = prior_attempts(&body);
        body = outcome_notice_body_with(&failed(i), None, &prior).unwrap();
    }
    let kept = prior_attempts(&body);
    assert_eq!(kept.len(), 8, "{body}");
    assert!(kept[0].contains("`s-4`") && kept[7].contains("`s-11`"), "{body}");
    let noop = outcome_notice_body_with(
        &run(OutcomeClass::Noop, "no_changes_needed", "s-12"),
        None,
        &kept,
    )
    .unwrap();
    assert_eq!(prior_attempts(&noop), kept, "{noop}");
}

/// #15413 review: a newer outcome comment from an untrusted author is neither
/// copied nor edited over; the earlier trusted history survives.
#[test]
fn an_untrusted_newer_comment_leaves_the_trusted_history_intact() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let mut reg = registry_with_gh(ws);
    let attempt = |sweep: &str| {
        with_attempt(
            run(OutcomeClass::Failed, "unclassified", sweep),
            Termination::Status(1),
            "host-a",
            LOG,
            5,
        )
    };
    reg.note_builder_outcome(&attempt("s-1"));
    let forged = format!(
        "{STEP_OUTCOME_COMMENT_MARKER} sweep=evil -->\\n{ATTEMPTS_HEADING}\\n- `evil`: host `evil-host`, ran 1s, log `/etc/passwd`"
    );
    std::fs::write(ws.join("stranger"), forged).unwrap();
    reg.note_builder_outcome(&attempt("s-2"));
    let body = std::fs::read_to_string(ws.join("body")).unwrap();
    assert!(!body.contains("evil"), "{body}");
    let ids: Vec<_> = prior_attempts(&body)
        .iter()
        .map(|l| l.split('`').nth(1).unwrap().to_string())
        .collect();
    assert_eq!(ids, ["s-1", "s-2"], "{body}");
    let patched = std::fs::read_to_string(ws.join("patched")).unwrap();
    assert!(!patched.contains("comments/9999"), "{patched}");
}

/// #15413 review: a failed read between two successful writes does not start a
/// second comment; the pending attempt joins the existing history later.
#[test]
fn a_read_failure_between_attempts_does_not_split_the_history() {
    let dir = tempdir().unwrap();
    let ws = dir.path();
    let mut reg = registry_with_gh(ws);
    let attempt = |sweep: &str| {
        with_attempt(
            run(OutcomeClass::Failed, "unclassified", sweep),
            Termination::Status(1),
            "host-a",
            LOG,
            5,
        )
    };
    reg.note_builder_outcome(&attempt("s-1"));
    std::fs::write(ws.join("readfail"), "").unwrap();
    reg.note_builder_outcome(&attempt("s-2"));
    assert_eq!(
        std::fs::read_to_string(ws.join("created"))
            .unwrap()
            .lines()
            .count(),
        1,
        "no fresh comment on an unreadable listing"
    );
    std::fs::remove_file(ws.join("readfail")).unwrap();
    reg.note_builder_outcome(&attempt("s-3"));
    let body = std::fs::read_to_string(ws.join("body")).unwrap();
    let ids: Vec<_> = prior_attempts(&body)
        .iter()
        .map(|l| l.split('`').nth(1).unwrap().to_string())
        .collect();
    assert_eq!(ids, ["s-1", "s-2", "s-3"], "{body}");
    assert_eq!(
        std::fs::read_to_string(ws.join("created"))
            .unwrap()
            .lines()
            .count(),
        1
    );
}

/// #15413 review: retention covers a streak longer than the default bound.
#[test]
fn retention_covers_a_quarantine_threshold_above_eight() {
    assert_eq!(attempts_kept(3), 8);
    assert_eq!(attempts_kept(9), 9);
    let mut body = String::new();
    for i in 0..9 {
        let run = with_attempt(
            run(OutcomeClass::Failed, "unclassified", &format!("run-{i}")),
            Termination::Status(1),
            "host-a",
            LOG,
            1,
        );
        let prior = prior_attempts(&body);
        body = outcome_notice_body_keeping(&run, None, &prior, attempts_kept(9)).unwrap();
    }
    let kept = prior_attempts(&body);
    assert_eq!(kept.len(), 9, "{body}");
    assert!(kept[0].contains("`run-0`"), "{body}");
}

/// #15413 review: the park text also covers a full-issue sweep, which posts no
/// Builder outcome comment.
#[test]
fn the_lookup_note_covers_a_full_issue_sweep() {
    let note = attempt_lookup_note();
    assert!(note.contains("full-issue sweep"), "{note}");
    assert!(note.contains(".loom/logs/sweep-issue-<N>.log"), "{note}");
}
