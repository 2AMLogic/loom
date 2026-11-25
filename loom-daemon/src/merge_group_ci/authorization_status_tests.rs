use super::{evaluate, mq_rule, org_facts, qualified, required_rule, wf, Prereq};
use crate::forge_merge_queue::authz::REQUIRED_CHECK_CONTEXT as AUTH;
use crate::merge_group_ci::Eligibility;

const TEMPLATE: &str =
    include_str!("../../../defaults/optional/github-workflows/merge-group-check.yml");

fn eligibility(contexts: &[&str], producer: &str) -> Eligibility {
    let facts = org_facts(vec![mq_rule(), required_rule(contexts)]);
    evaluate(&facts, &[qualified(), wf("publisher.yml", producer)], &[])
}

#[test]
fn native_status_and_ci_must_both_have_producers() {
    let e = eligibility(&["Gate", AUTH], TEMPLATE);
    assert!(e.eligible, "{:?}", e.failures);
    assert_eq!(e.required_checks, ["Gate", AUTH]);
    assert_eq!(e.authorization_workflow.as_deref(), Some(".github/workflows/publisher.yml"));
    assert_eq!(e.audit.required.len(), 1);
    assert_eq!(e.audit.required[0].context, "Gate");

    let write_implies_read =
        eligibility(&["Gate", AUTH], &TEMPLATE.replace("checks: read", "checks: write"));
    assert!(write_implies_read.eligible);
    let unknown = eligibility(&["No such job", AUTH], TEMPLATE);
    assert_eq!(unknown.failures[0].prereq, Prereq::MissingRequiredSuite);
    let auth_only = eligibility(&[AUTH], TEMPLATE);
    assert_eq!(auth_only.failures[0].prereq, Prereq::NoRequiredChecks);
}

#[test]
fn publisher_can_read_the_group_sha_from_a_step_env_binding() {
    let expr = "${{ github.event.merge_group.head_sha || github.event.workflow_run.head_sha }}";
    let env_form = TEMPLATE
        .replace(
            "          GH_REPO:",
            &format!("          GROUP_SHA: {expr}\n          GH_REPO:"),
        )
        .replace(&format!("--commit \"{expr}\""), "--commit \"$GROUP_SHA\"");
    let e = eligibility(&["Gate", AUTH], &env_form);
    assert!(e.eligible, "{:?}", e.failures);
}

#[test]
fn a_job_name_or_an_unusable_status_publisher_does_not_satisfy_the_rule() {
    let cmd = "\"$RUNNER_TEMP/loom-daemon\" forge merge-queue group-check";
    for (original, replacement) in [
        (cmd, "echo loom-daemon forge merge-queue group-check"),
        (cmd, "loom-daemon forge merge-queue status"),
        ("--repo \"$GH_REPO\"", "--repo other/project"),
        ("    name: Merge authorization", "    name: loom/merge-authorization\n    permissions: read-all"),
        ("  checks: read\n", ""),
        ("  pull-requests: read\n", ""),
        ("  contents: read\n", ""),
        ("statuses: write", "statuses: read"),
        ("--repo \"$GH_REPO\"", "--repo owner/repo"),
        ("  merge_group:", "  pull_request:"),
        ("      - name: Check merge-group authorization", "      - name: Check merge-group authorization\n        if: false"),
        ("        uses: actions/checkout@", "        if: false\n        uses: actions/checkout@"),
        ("--commit \"${{ github.event.merge_group.head_sha || github.event.workflow_run.head_sha }}\"", "--commit deadbeef"),
    ] {
        let invalid = TEMPLATE.replace(original, replacement);
        assert_ne!(invalid, TEMPLATE);
        let e = eligibility(&["Gate", AUTH], &invalid);
        assert!(e.authorization_workflow.is_none(), "{replacement}");
        assert!(e.failures.iter().any(|f| f.prereq == Prereq::WorkflowNotQualified), "{replacement}: {:?}", e.failures);
    }
}
