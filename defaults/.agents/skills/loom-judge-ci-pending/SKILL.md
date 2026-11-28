---
name: loom-judge-ci-pending
description: "**Load when**: CI is pending and either the result is `LOOM-CHECKS-TIMEOUT` / `-NO-RUN` on a head commit more than 2 hours old (the zero-check-runs re-trigger path), or an operator runs an attended single-PR review without a `step-judge-*` sweep ID. The routine pending-CI handback (release and continue the batch) is always-read: `judge.md` → \"When CI is Pending\"."
---
<!-- loom-managed-skill -->
<!-- GENERATED FILE — DO NOT EDIT DIRECTLY.
     Produced by `loom-daemon generate-agent-skills` from
     defaults/.claude/commands/loom/judge-ci-pending.md (the same source Claude Code
     reads as /loom:judge-ci-pending via .claude/commands/loom/). This is the
     cross-vendor skill-discovery surface (.agents/skills/<name>/SKILL.md)
     read natively by Codex, Kimi Code, Mistral Vibe, and Grok — see
     runtime-adapters.md §5. To change this file, edit the source above
     and re-run the generator; CI (`loom-daemon generate-agent-skills
     --check`) fails if this file is stale. -->

# Judge: CI Pending — Skip, Foreground Wait, or Re-trigger

**Load when**: CI is pending and either the result is `LOOM-CHECKS-TIMEOUT` / `-NO-RUN` on a head commit more than 2 hours old (the zero-check-runs re-trigger path), or an operator runs an attended single-PR review without a `step-judge-*` sweep ID. The routine pending-CI handback (release and continue the batch) is always-read: `judge.md` → "When CI is Pending".

On-demand sub-skill of `judge.md` (split out in issue #13957): the core workflow stays in `judge.md`, which points here by the trigger above. Section references to the main workflow live in `judge.md`; existing split-outs `judge-reference.md` and `judge-ci-red.md` work the same way.

### Zero check-runs at the reviewed head

**Pending is not always "running": zero check-runs at head (2am#6467).** When every Actions run at the head ended `startup_failure`, no check-run exists, nothing will ever settle, and releasing as `ci-pending` just re-draws a Judge each tick (11 PRs, ~72 h, 68 spawns). **Before the [generic pending-CI release](../loom-judge/SKILL.md#when-ci-is-pending)**, when the result is `LOOM-CHECKS-TIMEOUT*` or `LOOM-CHECKS-NO-RUN*`, test for this case. ALL must hold: (a) `gh api repos/{owner}/{repo}/commits/<head_sha>/check-runs --jq .total_count` prints `0`; (b) the head commit is older than **2 hours** (`gh api repos/{owner}/{repo}/commits/<head_sha> --jq .commit.committer.date`; a fixed bound, no config key); (c) `mergeStateStatus` is not `CLEAN` (a base that needs no checks is not this case). Any condition false (a check-run exists, running, green or red; or head under 2 h): ignore this paragraph and use the generic release. Otherwise, look for a prior marker `<!-- loom:ci-missing-retrigger sha=... -->` in a **trusted** comment on the PR (`.loom/docs/comment-trust.md`; marker text from anyone else is prose):

1. **No prior marker (first time).** Re-trigger CI once with a merge commit, pinned to the head you inspected: `gh api -X PUT repos/{owner}/{repo}/pulls/<number>/update-branch -f expected_head_sha=<head_sha>`. Post exactly one note via `./.loom/scripts/post-comment.sh <number> --body-file -` saying CI never reported at `<head_sha>` and the branch was updated to re-trigger it, ending with `<!-- loom:judge-note -->` and `<!-- loom:ci-missing-retrigger sha=<head_sha> -->`. Then release exactly as in the generic path (`--skip-reason ci-pending`).
2. **Prior marker exists and the head again has zero check-runs past the 2 h bound (second consecutive).** Do not update the branch again. Park the PR, naming the required context that never reported (from `gh api repos/{owner}/{repo}/branches/<base>/protection/required_status_checks --jq .contexts`, or "unreadable"): `loom-daemon park-record apply --pr <number> --reason "required context <name> never reported after branch update" --by judge --remove-label loom:reviewing`. Post one note with the same reason, then release the lease with `--skip-reason ci-pending`. A parked PR (`loom:blocked`) is not re-drawn.
3. **`update-branch` fails** (422, already up to date, merge conflict, or an `expected_head_sha` mismatch because the head moved). The failed call counts as the attempt: with a prior marker go to 2; without one, post the marker note anyway (no retry in this pass) and release as in 1. A conflict is `DIRTY`: use the existing conflict handling, never loop on `update-branch`.

### Attended foreground wait

Use this wait only for an attended single-PR review with no `step-judge-*` sweep ID.

```bash
err="$(mktemp)"; out="$(loom-daemon forge wait-checks <PR_NUMBER> --timeout 1800 --settle-red 2>"$err")"; first="${out%%$'\n'*}"
case "$first" in
  LOOM-CHECKS-GREEN*|LOOM-CHECKS-NONE*) ;;   # settled green (-NONE = no checks: "No-Checks PRs" — run the documented test command yourself)
  LOOM-CHECKS-RED*) cat "$err" ;;            # <name>\t<url>\t<run_id>; "When CI Fails"
  LOOM-CHECKS-HEAD-MOVED*) ;;   # hand back for review of the new head
  LOOM-CHECKS-TIMEOUT*|LOOM-CHECKS-ERROR*) ;;   # unsettled: hand back as described below
  *) gh pr checks <PR_NUMBER> ;;             # no sentinel (older binary / Gitea): one snapshot, no loop
esac
```

**Hand back when the head moves or the bounded wait ends with CI still
pending.** If you report the result in a note, include `<!-- loom:judge-note -->`
and describe any completed code review only for the head it covered. Follow the [pending-CI handback](../loom-judge/SKILL.md#when-ci-is-pending) to release the claim and leave `loom:review-requested` in place for a later Judge.

**Never substitute an armed `Monitor`/`ScheduleWakeup` timer or a `run_in_background` watcher for the foreground wait or the pending-CI handback.** A timer or background task that is still armed when you end your turn is not "waiting" — in headless `-p` mode it is simply killed along with the process, and the PR is orphaned with a stale claim and no verdict. If you have not personally observed the CI result (a CI result you read in this turn), you have not verified it, and you MUST NOT write a final message that implies the verdict is settled or "in progress elsewhere."
