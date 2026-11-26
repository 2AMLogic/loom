#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — primary-checkout
# mutation guard (#13589): git commit / switch / checkout <branch> in a
# workspace's PRIMARY checkout during an agent step is denied.
# #15483: with LOOM_WORKSPACE exported the workspace's own primary checkout is
# the only one in scope (an independent repository is allowed); without a
# usable value the guard fails closed and denies any primary checkout.
#
# Usage: ./tests/hooks/test-guard-destructive-primary-checkout.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

# Hermetic: a dispatched sweep exports these; the "operator session" cases need them unset.
unset LOOM_SWEEP_ID LOOM_WORKTREE_PATH
AGENT="LOOM_SWEEP_ID=step-builder-1-1"

echo -e "${YELLOW}--- Primary-checkout mutation guard (#13589) ---${NC}"

PC_REPO=$(make_wt_repo_linked)
PC_WT="$PC_REPO/.loom/worktrees/issue-1"
git -C "$PC_REPO" branch other >/dev/null 2>&1
DEFAULT_BR=$(git -C "$PC_REPO" symbolic-ref --short HEAD)
# The guard exempts only main/master as "return to default branch".
if [[ "$DEFAULT_BR" != "main" && "$DEFAULT_BR" != "master" ]]; then
    git -C "$PC_REPO" branch -m main >/dev/null 2>&1
    DEFAULT_BR=main
fi
# A tracked, present file (a `git checkout <path>` restore target) and a
# tracked file deleted from the worktree (the classic restore shape): both
# must stay allowed when a checkout target is not a ref (#14973).
echo present > "$PC_REPO/somefile"
git -C "$PC_REPO" add somefile
echo gone > "$PC_REPO/gonefile"
git -C "$PC_REPO" add gonefile
git -C "$PC_REPO" -c user.email=loom@test -c user.name=loom \
    commit -q -m files >/dev/null 2>&1
rm "$PC_REPO/gonefile"

# --- DENY in the primary checkout during an agent step ---
assert_deny_env "git commit in primary is denied" "$AGENT" "git commit -m x" "$PC_REPO"
assert_deny_env "git checkout -b in primary is denied" "$AGENT" "git checkout -b x" "$PC_REPO"
assert_deny_env "git checkout -B (non-default) in primary is denied" "$AGENT" "git checkout -B x origin/main" "$PC_REPO"
assert_deny_env "git switch <branch> in primary is denied" "$AGENT" "git switch other" "$PC_REPO"
assert_deny_env "git switch -c in primary is denied" "$AGENT" "git switch -c x" "$PC_REPO"
assert_deny_env "git checkout <branch> in primary is denied" "$AGENT" "git checkout other" "$PC_REPO"
assert_deny_env "git checkout --detach in primary is denied" "$AGENT" "git checkout --detach" "$PC_REPO"
assert_deny_env "git checkout <sha> (detached) in primary is denied" "$AGENT" \
    "git checkout $(git -C "$PC_REPO" rev-parse HEAD)" "$PC_REPO"
assert_deny_env "git -C <primary> commit from a worktree cwd is denied" "$AGENT" \
    "git -C $PC_REPO commit -m x" "$PC_WT"
assert_deny_env "cd <primary> && git commit from a worktree cwd is denied" "$AGENT" \
    "cd $PC_REPO && git commit -m x" "$PC_WT"
assert_deny_env "LOOM_WORKTREE_PATH also marks an agent step" "LOOM_WORKTREE_PATH=$PC_WT" \
    "git commit -m x" "$PC_REPO"

# --- #14973: a ref the same command creates (fetch :<ref> chains), and gh pr checkout ---
assert_deny_env "git fetch :prN && git checkout prN in primary is denied" "$AGENT" \
    "git fetch origin pull/7/head:pr7 && git checkout pr7" "$PC_REPO"
assert_deny_env "git fetch :refs/review/prN && git checkout refs/review/prN in primary is denied" "$AGENT" \
    "git fetch origin pull/7/head:refs/review/pr7 && git checkout refs/review/pr7" "$PC_REPO"
assert_deny_env "git checkout FETCH_HEAD chained after a fetch in primary is denied" "$AGENT" \
    "git fetch origin main && git checkout FETCH_HEAD" "$PC_REPO"
assert_deny_env "git checkout of a target that is no ref and no path in primary is denied" "$AGENT" \
    "git checkout nosuchref" "$PC_REPO"
assert_deny_env "git -C <primary> fetch-chain from a worktree cwd is denied" "$AGENT" \
    "git -C $PC_REPO fetch origin pull/7/head:pr7 && git -C $PC_REPO checkout pr7" "$PC_WT"
assert_deny_env "gh pr checkout in primary is denied" "$AGENT" "gh pr checkout 7" "$PC_REPO"
assert_deny_env "cd <primary> && gh pr checkout from a worktree cwd is denied" "$AGENT" \
    "cd $PC_REPO && gh pr checkout 7" "$PC_WT"
assert_allow_env "gh pr checkout inside a worktree is allowed" "$AGENT" "gh pr checkout 7" "$PC_WT"
assert_allow_env "quoted text mentioning gh pr checkout is inert" "$AGENT" "echo 'gh pr checkout 7'" "$PC_REPO"
assert_allow_env "operator session gh pr checkout (no agent env) is untouched" "" "gh pr checkout 7" "$PC_REPO"
PC_OUT=$(run_guard_env "$AGENT" "git commit -m x" "$PC_REPO") || true
TOTAL=$((TOTAL + 1))
if echo "$PC_OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason' | grep -q 'worktree.sh <issue-number>' \
   && echo "$PC_OUT" | jq -r '.hookSpecificOutput.permissionDecisionReason' | grep -q 'guards.primaryCheckoutMutation'; then
    PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: deny names worktree.sh and the toggle"
else
    FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: deny names worktree.sh and the toggle: $PC_OUT"
fi

# --- ALLOW: worktrees unaffected ---
assert_allow_env "git commit inside a worktree is allowed" "$AGENT" "git commit -m x" "$PC_WT"
assert_allow_env "git checkout -b inside a worktree is allowed" "$AGENT" "git checkout -b y" "$PC_WT"
assert_allow_env "git switch inside a worktree is allowed" "$AGENT" "git switch other" "$PC_WT"
assert_allow_env "cd <worktree> && git commit from primary cwd is allowed" "$AGENT" \
    "cd $PC_WT && git commit -m x" "$PC_REPO"

# --- ALLOW: legitimate primary-clone shapes ---
assert_allow_env "git checkout main (Auditor) in primary is allowed" "$AGENT" "git checkout main" "$PC_REPO"
assert_allow_env "git checkout -B main origin/main (Auditor) is allowed" "$AGENT" "git checkout -B main origin/main" "$PC_REPO"
assert_allow_env "git switch main in primary is allowed" "$AGENT" "git switch main" "$PC_REPO"
assert_allow_env "git checkout -- <file> restore is allowed" "$AGENT" "git checkout -- somefile" "$PC_REPO"
assert_allow_env "git checkout <path> (not a ref) is allowed" "$AGENT" "git checkout somefile" "$PC_REPO"
assert_allow_env "git checkout of a tracked file deleted from the worktree is allowed" "$AGENT" \
    "git checkout gonefile" "$PC_REPO"
assert_allow_env "git switch --help is allowed" "$AGENT" "git switch --help" "$PC_REPO"
assert_allow_env "read-only git status is allowed" "$AGENT" "git status" "$PC_REPO"
assert_allow_env "read-only git log is allowed" "$AGENT" "git log --oneline" "$PC_REPO"
assert_allow_env "git commit-tree is not git commit" "$AGENT" "git commit-tree HEAD^{tree} -m x" "$PC_REPO"
assert_allow_env "quoted text mentioning git commit is inert" "$AGENT" "echo 'git commit -m x'" "$PC_REPO"
assert_allow_env "worktree.sh invocation is allowed" "$AGENT" "./.loom/scripts/worktree.sh 5" "$PC_REPO"
assert_allow_env "merge-pr.sh invocation is allowed" "$AGENT" "./.loom/scripts/merge-pr.sh 5" "$PC_REPO"
assert_allow_env "resync-installed.sh invocation is allowed" "$AGENT" "./.loom/scripts/resync-installed.sh" "$PC_REPO"

# --- #15483: LOOM_WORKSPACE narrows the guard to the workspace's own clone ---
# The dispatcher exports LOOM_SWEEP_ID and LOOM_WORKSPACE together. run_guard_env
# takes one assignment, so this block exports the sweep id for its duration and
# passes the workspace per invocation. Every case above ran with LOOM_WORKSPACE
# unset — the fail-closed half of the rule — so the two halves together cover
# each shape with and without the signal.
export LOOM_SWEEP_ID=step-builder-1-1
PC_WS="LOOM_WORKSPACE=$PC_REPO"
PC_FIXTURE=$(make_standalone_repo)
PC_OUTSIDE=$(mktemp -d 2>/dev/null)   # a cwd inside no repository at all
PC_NESTED="$PC_REPO/throwaway-clone"  # a second repository INSIDE the workspace dir
mkdir -p "$PC_NESTED"
git -C "$PC_NESTED" init -q >/dev/null 2>&1

# ALLOW: a repository other than the workspace's is out of scope
assert_allow_env "A: git commit in an independent repository (tool-call cwd) is allowed" "$PC_WS" \
    "git commit -q -m second" "$PC_FIXTURE"
assert_allow_env "cd <independent> && git commit from the workspace cwd is allowed" "$PC_WS" \
    "cd $PC_FIXTURE && git commit -q -m second" "$PC_REPO"
assert_allow_env "cd <independent> && git commit from a worktree cwd is allowed" "$PC_WS" \
    "cd $PC_FIXTURE && git commit -q -m second" "$PC_WT"
assert_allow_env "B: git -C <independent> commit from a cwd outside any repository is allowed" "$PC_WS" \
    "git -C $PC_FIXTURE commit -q -m second" "$PC_OUTSIDE"
assert_allow_env "git -C <independent> commit from the workspace cwd is allowed" "$PC_WS" \
    "git -C $PC_FIXTURE commit -q -m second" "$PC_REPO"
assert_allow_env "D: mkdir + init + commit of a directory that already exists is allowed" "$PC_WS" \
    "mkdir -p $PC_FIXTURE && cd $PC_FIXTURE && git init -q -b main && git commit --allow-empty -q -m first" "$PC_OUTSIDE"
assert_allow_env "git checkout -b in an independent repository is allowed" "$PC_WS" "git checkout -b x" "$PC_FIXTURE"
assert_allow_env "git switch -c in an independent repository is allowed" "$PC_WS" "git switch -c x" "$PC_FIXTURE"
assert_allow_env "a second repository nested inside the workspace directory is allowed" "$PC_WS" \
    "git commit -q -m second" "$PC_NESTED"

# DENY: the workspace's own primary checkout stays protected
assert_deny_env "F: git commit in the workspace primary is still denied" "$PC_WS" "git commit -m x" "$PC_REPO"
assert_deny_env "git checkout -b in the workspace primary is still denied" "$PC_WS" "git checkout -b x" "$PC_REPO"
assert_deny_env "git switch <branch> in the workspace primary is still denied" "$PC_WS" "git switch other" "$PC_REPO"
assert_deny_env "fetch-chain checkout in the workspace primary is still denied" "$PC_WS" \
    "git fetch origin pull/7/head:pr7 && git checkout pr7" "$PC_REPO"
assert_deny_env "gh pr checkout in the workspace primary is still denied" "$PC_WS" "gh pr checkout 7" "$PC_REPO"
assert_deny_env "git -C <workspace primary> commit from a worktree cwd is still denied" "$PC_WS" \
    "git -C $PC_REPO commit -m x" "$PC_WT"
assert_deny_env "git -C <workspace primary> commit from an independent repository is still denied" "$PC_WS" \
    "git -C $PC_REPO commit -m x" "$PC_FIXTURE"
assert_deny_env "cd <workspace primary> && git commit from a worktree cwd is still denied" "$PC_WS" \
    "cd $PC_REPO && git commit -m x" "$PC_WT"

# The workspace value may itself be a linked worktree (`loom-daemon spawn
# --worktree` exports the worktree): the clone behind it stays protected.
PC_WSWT="LOOM_WORKSPACE=$PC_WT"
assert_deny_env "LOOM_WORKSPACE naming a linked worktree still denies a commit in its clone" "$PC_WSWT" \
    "git commit -m x" "$PC_REPO"
assert_deny_env "LOOM_WORKSPACE naming a linked worktree still denies git -C <clone> commit" "$PC_WSWT" \
    "git -C $PC_REPO commit -m x" "$PC_WT"
assert_allow_env "LOOM_WORKSPACE naming a linked worktree allows a commit in that worktree" "$PC_WSWT" \
    "git commit -m x" "$PC_WT"
assert_allow_env "LOOM_WORKSPACE naming a linked worktree allows an independent repository" "$PC_WSWT" \
    "git commit -q -m second" "$PC_FIXTURE"
assert_deny_env "LOOM_WORKSPACE naming a repository makes THAT repository the protected one" \
    "LOOM_WORKSPACE=$PC_FIXTURE" "git commit -q -m second" "$PC_FIXTURE"

# FAIL CLOSED: an unusable LOOM_WORKSPACE keeps the wider rule (any primary checkout)
assert_deny_env "LOOM_WORKSPACE unset: a commit in an independent repository is still denied" "" \
    "git commit -q -m second" "$PC_FIXTURE"
assert_deny_env "LOOM_WORKSPACE unset: git -C <independent> commit is still denied" "" \
    "git -C $PC_FIXTURE commit -q -m second" "$PC_OUTSIDE"
assert_deny_env "LOOM_WORKSPACE empty: a commit in an independent repository is still denied" "LOOM_WORKSPACE=" \
    "git commit -q -m second" "$PC_FIXTURE"
assert_deny_env "LOOM_WORKSPACE naming a missing directory: an independent repository is still denied" \
    "LOOM_WORKSPACE=$PC_OUTSIDE/no-such-dir" "git commit -q -m second" "$PC_FIXTURE"
assert_deny_env "LOOM_WORKSPACE relative: an independent repository is still denied" "LOOM_WORKSPACE=." \
    "git commit -q -m second" "$PC_FIXTURE"
assert_deny_env "LOOM_WORKSPACE naming a directory outside any repository: an independent repository is still denied" \
    "LOOM_WORKSPACE=$PC_OUTSIDE" "git commit -q -m second" "$PC_FIXTURE"
PC_BARE="$PC_OUTSIDE/bare.git"   # a repository with no working tree at all
git init -q --bare "$PC_BARE" >/dev/null 2>&1
assert_deny_env "LOOM_WORKSPACE naming a bare repository: an independent repository is still denied" \
    "LOOM_WORKSPACE=$PC_BARE" "git commit -q -m second" "$PC_FIXTURE"
assert_deny_env "LOOM_WORKSPACE naming a bare repository: a workspace's primary checkout is still denied" \
    "LOOM_WORKSPACE=$PC_BARE" "git commit -m x" "$PC_REPO"
assert_allow_env "a directory the same command creates does not exist at scan time: not scanned" "" \
    "mkdir -p $PC_OUTSIDE/fresh && cd $PC_OUTSIDE/fresh && git init -q -b main && git commit --allow-empty -q -m first" "$PC_OUTSIDE"

unset LOOM_SWEEP_ID
rm -rf "$PC_OUTSIDE" "$PC_FIXTURE" "$PC_NESTED"

# --- not an agent step / toggle off ---
assert_allow_env "operator session (no agent env) is untouched" "" "git commit -m x" "$PC_REPO"
TOTAL=$((TOTAL + 1))
PC_OUT=$(make_input "git commit -m x" "$PC_REPO" | env LOOM_SWEEP_ID=s LOOM_GUARD_PRIMARY_CHECKOUT_MUTATION=0 "$GUARD" 2>&1) || true
if ! echo "$PC_OUT" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
    PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: toggle off via env allows"
else
    FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: toggle off via env allows: $PC_OUT"
fi

PC_CFG=$(make_wt_repo_linked)
mkdir -p "$PC_CFG/.loom"
printf '%s' '{"guards":{"primaryCheckoutMutation":false}}' > "$PC_CFG/.loom/config.json"
assert_allow_env "toggle off via config allows" "$AGENT" "git commit -m x" "$PC_CFG"

rm -rf "$PC_REPO" "$PC_CFG"
echo ""
print_summary
