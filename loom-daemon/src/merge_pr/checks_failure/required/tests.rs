//! Tests for the synchronous-path required-context conclusion gate (#10688).

use super::*;
use serde_json::json;

fn run(name: &str, status: &str, conclusion: Option<&str>, started: Option<&str>) -> CheckRun {
    CheckRun {
        name: name.into(),
        status: status.into(),
        settled: status == "completed",
        conclusion: conclusion.map(String::from),
        started_at: started.map(|t| t.parse().unwrap()),
    }
}

fn done(name: &str, conclusion: &str) -> CheckRun {
    run(name, "completed", Some(conclusion), Some("2026-10-11T10:00:00Z"))
}

fn status(context: &str, state: &str) -> Status {
    Status {
        context: context.into(),
        state: state.into(),
    }
}

fn v(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}

fn named(items: &[(&str, &str)]) -> Named {
    items
        .iter()
        .map(|(a, b)| ((*a).into(), (*b).into()))
        .collect()
}

#[test]
fn nothing_required_is_clean() {
    assert_eq!(assess(&[], &[done("Lint", "failure")], &[], &[]), Verdict::Clean);
}

#[test]
fn all_required_green_is_clean_and_informational_red_is_ignored() {
    let runs = [
        done("CI Result", "success"),
        done("Lint", "failure"),
        done("Docs", "skipped"),
    ];
    assert_eq!(assess(&v(&["CI Result", "Docs"]), &runs, &[], &[]), Verdict::Clean);
}

#[test]
fn red_required_check_outside_ci_refuses_naming_it() {
    // 2am#3112: a required check from another app concluded failure.
    let runs = [
        done("CI Result", "success"),
        done("GitHub Bypass Check", "failure"),
    ];
    let verdict = assess(&v(&["CI Result", "GitHub Bypass Check"]), &runs, &[], &[]);
    assert_eq!(
        verdict,
        Verdict::Refuse {
            red: named(&[("GitHub Bypass Check", "failure")]),
            bypassed: vec![]
        }
    );
}

#[test]
fn every_bad_conclusion_is_red() {
    for c in [
        "failure",
        "cancelled",
        "timed_out",
        "action_required",
        "startup_failure",
        "stale",
        "weird",
    ] {
        let v1 = assess(&v(&["X"]), &[done("X", c)], &[], &[]);
        assert!(matches!(v1, Verdict::Refuse { .. }), "{c} must refuse, got {v1:?}");
    }
}

#[test]
fn missing_required_context_holds() {
    let verdict = assess(&v(&["CI Result", "Bypass"]), &[done("CI Result", "success")], &[], &[]);
    assert_eq!(
        verdict,
        Verdict::Hold {
            waiting: named(&[("Bypass", "not reported")]),
            bypassed: vec![]
        }
    );
}

#[test]
fn pending_required_context_holds() {
    let runs = [run(
        "Bypass",
        "in_progress",
        None,
        Some("2026-10-11T10:00:00Z"),
    )];
    assert_eq!(
        assess(&v(&["Bypass"]), &runs, &[], &[]),
        Verdict::Hold {
            waiting: named(&[("Bypass", "in_progress")]),
            bypassed: vec![]
        }
    );
}

#[test]
fn red_outranks_pending_elsewhere() {
    let runs = [done("A", "failure"), run("B", "queued", None, None)];
    assert!(matches!(assess(&v(&["A", "B"]), &runs, &[], &[]), Verdict::Refuse { .. }));
}

#[test]
fn rerun_in_flight_supersedes_the_old_red_run() {
    let runs = [done("A", "failure"), run("A", "queued", None, None)];
    assert!(matches!(assess(&v(&["A"]), &runs, &[], &[]), Verdict::Hold { .. }));
}

#[test]
fn latest_completed_run_decides() {
    let old_red = run("A", "completed", Some("failure"), Some("2026-10-11T09:00:00Z"));
    let new_green = run("A", "completed", Some("success"), Some("2026-10-11T10:00:00Z"));
    assert_eq!(
        assess(&v(&["A"]), &[new_green.clone(), old_red.clone()], &[], &[]),
        Verdict::Clean
    );
    let old_green = run("A", "completed", Some("success"), Some("2026-10-11T09:00:00Z"));
    let new_red = run("A", "completed", Some("failure"), Some("2026-10-11T10:00:00Z"));
    assert!(matches!(
        assess(&v(&["A"]), &[old_green, new_red], &[], &[]),
        Verdict::Refuse { .. }
    ));
}

#[test]
fn commit_statuses_count_as_required_evidence() {
    assert_eq!(
        assess(&v(&["ext/ci"]), &[], &[status("ext/ci", "success")], &[]),
        Verdict::Clean
    );
    assert!(matches!(
        assess(&v(&["ext/ci"]), &[], &[status("ext/ci", "error")], &[]),
        Verdict::Refuse { .. }
    ));
    assert!(matches!(
        assess(&v(&["ext/ci"]), &[], &[status("ext/ci", "pending")], &[]),
        Verdict::Hold { .. }
    ));
}

#[test]
fn worst_source_wins() {
    let verdict = assess(&v(&["A"]), &[done("A", "success")], &[status("A", "failure")], &[]);
    assert!(matches!(verdict, Verdict::Refuse { .. }));
}

#[test]
fn allow_red_required_overrides_exactly_the_named_context() {
    let runs = [done("A", "failure"), done("B", "success")];
    assert_eq!(
        assess(&v(&["A", "B"]), &runs, &[], &v(&["A"])),
        Verdict::Bypassed(named(&[("A", "failure")]))
    );
}

#[test]
fn allowing_one_context_does_not_admit_another_red_one() {
    let runs = [done("A", "failure"), done("B", "cancelled")];
    assert_eq!(
        assess(&v(&["A", "B"]), &runs, &[], &v(&["A"])),
        Verdict::Refuse {
            red: named(&[("B", "cancelled")]),
            bypassed: named(&[("A", "failure")])
        }
    );
}

#[test]
fn allow_red_required_never_passes_a_missing_or_pending_context() {
    let runs = [run("A", "in_progress", None, None)];
    assert!(matches!(
        assess(&v(&["A", "B"]), &runs, &[], &v(&["A", "B"])),
        Verdict::Hold { .. }
    ));
}

#[test]
fn messages_name_the_contexts() {
    let red = named(&[("GitHub Bypass Check", "failure")]);
    let msg = refusal("7", "deadbeef", &red);
    assert!(msg.starts_with("Merge blocked: PR #7"));
    assert!(msg.contains("`GitHub Bypass Check` (failure)"));
    assert!(msg.contains("--allow-red-required"));
    assert!(
        hold_reason("deadbeef", &named(&[("B", "not reported")])).contains("`B` (not reported)")
    );
    let body = bypass_comment("7", "deadbeef", &red);
    assert!(body.contains("--allow-red-required"));
    assert!(body.contains("`GitHub Bypass Check` (failure)"));
}

#[test]
fn parses_combined_status_and_fails_closed_on_truncation() {
    let ok = json!({"total_count": 1, "statuses": [{"context": "ext/ci", "state": "success"}]});
    assert_eq!(parse_statuses(&ok).unwrap(), vec![status("ext/ci", "success")]);
    let short = json!({"total_count": 2, "statuses": [{"context": "ext/ci", "state": "success"}]});
    assert!(parse_statuses(&short).is_err());
    assert!(parse_statuses(&json!({})).is_err());
}

#[test]
fn parses_check_run_rows() {
    let rows = json!([
        {"name": "A", "status": "completed", "conclusion": "failure", "started_at": "2026-10-11T10:00:00Z"},
        {"name": "B", "status": "queued", "conclusion": null}
    ]);
    let runs = parse_check_runs(&rows).unwrap();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[1].conclusion, None);
    assert!(parse_check_runs(&json!([{"status": "completed"}])).is_err());
}

#[test]
fn settled_in_progress_green_is_clean_and_red_refuses() {
    let green_rows = json!([{
        "name": "lint",
        "status": "in_progress",
        "conclusion": "success",
        "started_at": "2026-11-24T17:28:46Z",
        "completed_at": "2026-11-24T17:28:53Z"
    }]);
    let green = parse_check_runs(&green_rows).unwrap();
    assert_eq!(assess(&v(&["lint"]), &green, &[], &[]), Verdict::Clean);

    let red_rows = json!([{
        "name": "lint",
        "status": "in_progress",
        "conclusion": "failure",
        "started_at": "2026-11-24T17:28:46Z",
        "completed_at": "2026-11-24T17:28:53Z"
    }]);
    let red = parse_check_runs(&red_rows).unwrap();
    assert_eq!(
        assess(&v(&["lint"]), &red, &[], &[]),
        Verdict::Refuse {
            red: named(&[("lint", "failure")]),
            bypassed: vec![]
        }
    );
    assert_eq!(
        assess(&v(&["lint"]), &red, &[], &v(&["lint"])),
        Verdict::Bypassed(named(&[("lint", "failure")]))
    );

    let ordered_rows = json!([
        {
            "name": "lint",
            "status": "in_progress",
            "conclusion": "failure",
            "started_at": "2026-11-24T17:00:00Z",
            "completed_at": "2026-11-24T17:00:10Z"
        },
        {
            "name": "lint",
            "status": "in_progress",
            "conclusion": "success",
            "started_at": "2026-11-24T17:28:46Z",
            "completed_at": "2026-11-24T17:28:53Z"
        }
    ]);
    let ordered = parse_check_runs(&ordered_rows).unwrap();
    assert_eq!(assess(&v(&["lint"]), &ordered, &[], &[]), Verdict::Clean);
}

#[test]
fn partial_in_progress_rows_still_hold() {
    for row in [
        json!({
            "name": "lint",
            "status": "in_progress",
            "conclusion": null,
            "started_at": "2026-11-24T17:28:46Z",
            "completed_at": "2026-11-24T17:28:53Z"
        }),
        json!({
            "name": "lint",
            "status": "in_progress",
            "conclusion": "success",
            "started_at": "2026-11-24T17:28:46Z",
            "completed_at": null
        }),
        json!({
            "name": "lint",
            "status": "in_progress",
            "conclusion": "success",
            "started_at": "2026-11-24T17:28:46Z"
        }),
    ] {
        let runs = parse_check_runs(&json!([row])).unwrap();
        assert_eq!(
            assess(&v(&["lint"]), &runs, &[], &[]),
            Verdict::Hold {
                waiting: named(&[("lint", "in_progress")]),
                bypassed: vec![]
            },
            "partial row must stay on hold: {row}"
        );
    }
}

#[test]
fn newer_running_attempt_holds_even_after_settled_success() {
    let settled = json!({
        "name": "lint",
        "status": "in_progress",
        "conclusion": "success",
        "started_at": "2026-11-24T17:28:46Z",
        "completed_at": "2026-11-24T17:28:53Z"
    });
    let rerunning = json!({
        "name": "lint",
        "status": "in_progress",
        "conclusion": null,
        "started_at": "2026-11-24T17:35:00Z",
        "completed_at": null
    });
    for pair in [
        json!([settled.clone(), rerunning.clone()]),
        json!([rerunning, settled]),
    ] {
        let runs = parse_check_runs(&pair).unwrap();
        assert_eq!(
            assess(&v(&["lint"]), &runs, &[], &[]),
            Verdict::Hold {
                waiting: named(&[("lint", "in_progress")]),
                bypassed: vec![]
            }
        );
    }
}

#[test]
fn status_disagreement_diagnostic_fires_only_for_settled_non_completed_required_rows() {
    let rows = json!([
        {
            "name": "lint",
            "status": "in_progress",
            "conclusion": "success",
            "started_at": "2026-11-24T17:28:46Z",
            "completed_at": "2026-11-24T17:28:53Z"
        },
        {
            "name": "docs",
            "status": "completed",
            "conclusion": "success",
            "started_at": "2026-11-24T17:28:46Z",
            "completed_at": "2026-11-24T17:28:53Z"
        },
        {
            "name": "informational-stuck",
            "status": "in_progress",
            "conclusion": "success",
            "started_at": "2026-11-24T17:28:46Z",
            "completed_at": "2026-11-24T17:28:53Z"
        },
        {
            "name": "still-running",
            "status": "in_progress",
            "conclusion": null,
            "started_at": "2026-11-24T17:28:46Z",
            "completed_at": "2026-11-24T17:28:53Z"
        }
    ]);
    let runs = parse_check_runs(&rows).unwrap();
    assert_eq!(runs[0].status, "in_progress");
    assert!(runs[0].settled);
    assert!(!runs[3].settled);

    let notes =
        status_disagreements("430", "1e847cee", &v(&["lint", "docs", "still-running"]), &runs);
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("PR #430"), "{}", notes[0]);
    assert!(notes[0].contains("1e847cee"), "{}", notes[0]);
    assert!(notes[0].contains("`lint`"), "{}", notes[0]);
    assert!(notes[0].contains("in_progress"), "{}", notes[0]);
    assert!(notes[0].contains("success"), "{}", notes[0]);
}
