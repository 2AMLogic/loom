//! Recognize Loom's native producer of the merge-group authorization commit status.
//! CI jobs still require exact check-run name coverage in the ordinary audit.

use super::audit::{eval_condition, job_states, needs_of, triggered};
use super::context::{Event, EventContext, JobScope, RunState, MG_HEAD_SHA};
use super::expr::{interpolate, Truth};
use super::workflow::{Step, Workflow};

/// The workflow must be able to run the publisher on checks_requested. The
/// ordinary audit independently checks its checkout and workflow_run trust gate.
pub(super) fn producer(workflows: &[Workflow]) -> Option<&str> {
    workflows
        .iter()
        .find(|wf| produces_status(wf))
        .map(|wf| wf.file.as_str())
}

fn produces_status(wf: &Workflow) -> bool {
    if !triggered(wf, Event::MergeGroup) {
        return false;
    }
    let event = EventContext::new(Event::MergeGroup, &wf.name);
    let states = job_states(wf, &event);
    wf.jobs.iter().any(|job| {
        let permissions = job.permissions.as_ref().or(wf.permissions.as_ref());
        let can_post = permissions.is_some_and(|p| {
            p == "write-all"
                || p.split(", ")
                    .any(|permission| permission == "statuses: write")
        });
        if !can_post || states.get(&job.id) != Some(&RunState::Runs) {
            return false;
        }
        let scope = JobScope {
            event: &event,
            needs: needs_of(job, &states),
            job_level: false,
        };
        let mut checked_out = false;
        job.steps
            .iter()
            .filter(|s| eval_condition(s.if_cond.as_deref(), &scope).truth() == Truth::True)
            .any(|step| {
                if step.is_checkout() {
                    checked_out = true;
                }
                checked_out && publishes_to_group(step, &scope)
            })
    })
}

/// Recognize the direct CLI invocation used by the shipped workflow. Unknown
/// shell wrappers cannot establish coverage. Resolve the commit from the
/// merge-group event, including a step-level env binding as in consumer repos.
fn publishes_to_group(step: &Step, scope: &JobScope<'_>) -> bool {
    let Some(script) = step.run.as_deref() else {
        return false;
    };
    let Some(script) = interpolate(script, scope).render() else {
        return false;
    };
    let script = script.replace("\\\n", " ");
    let mut lines = script
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'));
    let Some(cmd) = lines.next() else {
        return false;
    };
    if lines.next().is_some() {
        return false;
    }
    let mut words = cmd
        .split_whitespace()
        .map(|s| s.trim_matches('"'))
        .peekable();
    words.next_if_eq(&"exec");
    let Some(bin) = words.next() else {
        return false;
    };
    if bin.rsplit('/').next() != Some("loom-daemon")
        || !words
            .by_ref()
            .take(3)
            .eq(["forge", "merge-queue", "group-check"])
    {
        return false;
    }
    let bound_to = |arg: &str, value: &str, builtin: &str| {
        arg == value
            || arg == builtin
            || step.env.iter().any(|(name, expr)| {
                arg.strip_prefix('$') == Some(name.as_str())
                    && interpolate(expr, scope).render().as_deref() == Some(value)
            })
    };
    let mut group_target = false;
    while let Some(flag) = words.next() {
        match (flag, words.next()) {
            ("--commit", Some(arg))
                if !group_target && bound_to(arg, MG_HEAD_SHA, "$GITHUB_SHA") =>
            {
                group_target = true
            }
            ("--repo", Some(arg)) if bound_to(arg, "owner/repo", "$GITHUB_REPOSITORY") => {}
            _ => return false,
        }
    }
    group_target
}
