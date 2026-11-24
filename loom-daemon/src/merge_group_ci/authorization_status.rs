//! Recognize Loom's native producer of the merge-group authorization commit status.
//! CI jobs still require exact check-run name coverage in the ordinary audit.

use std::collections::BTreeMap;

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
        if !has_group_permissions(permissions.map(String::as_str))
            || states.get(&job.id) != Some(&RunState::Runs)
        {
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

fn has_group_permissions(permissions: Option<&str>) -> bool {
    let Some(permissions) = permissions else {
        return false;
    };
    if permissions == "write-all" {
        return true;
    }
    let grants: BTreeMap<_, _> = permissions
        .split(", ")
        .filter_map(|s| s.split_once(": "))
        .collect();
    grants.get("statuses") == Some(&"write")
        && ["contents", "checks", "pull-requests"]
            .iter()
            .all(|key| matches!(grants.get(key).copied(), Some("read" | "write")))
}

/// Split the direct command's bare or double-quoted arguments. Preserve a
/// quoted Actions expression as one argument so its value can be proved.
fn command_words(mut text: &str) -> Option<Vec<&str>> {
    let mut words = Vec::new();
    text = text.trim();
    while !text.is_empty() {
        let (word, tail) = if let Some(quoted) = text.strip_prefix('"') {
            let (word, tail) = quoted.split_once('"')?;
            if !tail.is_empty() && !tail.starts_with(char::is_whitespace) {
                return None;
            }
            (word, tail)
        } else {
            text.split_at(text.find(char::is_whitespace).unwrap_or(text.len()))
        };
        words.push(word);
        text = tail.trim_start();
    }
    Some(words)
}

fn event_argument(
    arg: &str,
    builtin: &str,
    expected: &str,
    step: &Step,
    scope: &JobScope<'_>,
) -> bool {
    // A literal which happens to equal an example value in EventContext is
    // not evidence that the workflow uses the event's value at run time.
    let expr = if arg.starts_with("${{") {
        arg
    } else {
        let Some(var) = arg
            .strip_prefix("${")
            .and_then(|v| v.strip_suffix('}'))
            .or_else(|| arg.strip_prefix('$'))
        else {
            return false;
        };
        if var == builtin {
            return true;
        }
        let Some((_, value)) = step.env.iter().find(|(k, _)| k == var) else {
            return false;
        };
        value.as_str()
    };
    let Some(inner) = expr
        .trim()
        .strip_prefix("${{")
        .and_then(|e| e.strip_suffix("}}"))
    else {
        return false;
    };
    let paths = match builtin {
        "GITHUB_SHA" => &[
            "github.sha",
            "github.event.merge_group.head_sha",
            "github.event.workflow_run.head_sha",
        ][..],
        _ => &["github.repository"][..],
    };
    inner.split("||").all(|path| paths.contains(&path.trim()))
        && interpolate(expr, scope).render().as_deref() == Some(expected)
}

/// Recognize the direct CLI invocation used by the shipped workflow. Unknown
/// shell wrappers cannot establish coverage. Resolve the commit from the
/// merge-group event, including a step-level env binding as in consumer repos.
fn publishes_to_group(step: &Step, scope: &JobScope<'_>) -> bool {
    let Some(script) = step.run.as_deref() else {
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
    let Some(words) = command_words(cmd) else {
        return false;
    };
    let mut words = words.into_iter().peekable();
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
    let mut group_target = false;
    while let Some(flag) = words.next() {
        match (flag, words.next()) {
            ("--commit", Some(arg))
                if !group_target && event_argument(arg, "GITHUB_SHA", MG_HEAD_SHA, step, scope) =>
            {
                group_target = true
            }
            ("--repo", Some(arg))
                if event_argument(arg, "GITHUB_REPOSITORY", "owner/repo", step, scope) => {}
            _ => return false,
        }
    }
    group_target
}
