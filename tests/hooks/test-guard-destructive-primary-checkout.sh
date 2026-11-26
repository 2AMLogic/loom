#!/usr/bin/env bash
# Test suite for defaults/hooks/guard-destructive-generic.sh — primary-checkout
# mutation guard (#13589): git commit / switch / checkout <branch> in a
# workspace's PRIMARY checkout during an agent step is denied.
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
# #15003: quoted pathspec targets ('*.md'), quoted paths with spaces (present
# AND deleted-from-worktree tracked), and a tracked file literally named `-`
# (git still treats a bare `git checkout -` as the @{-1} switch even when that
# file exists — only `git checkout -- -` restores it).
echo doc > "$PC_REPO/doc.md"
git -C "$PC_REPO" add doc.md
echo present > "$PC_REPO/space file"
git -C "$PC_REPO" add "space file"
echo gone > "$PC_REPO/space gone"
git -C "$PC_REPO" add "space gone"
echo dash > "$PC_REPO/-"
git -C "$PC_REPO" add ./-
git -C "$PC_REPO" -c user.email=loom@test -c user.name=loom \
    commit -q -m files >/dev/null 2>&1
rm "$PC_REPO/gonefile"
rm "$PC_REPO/space gone"

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

# --- #15003 gap 1: a quoted positional is unquoted before classification ---
assert_allow_env "git checkout '<quoted pathspec>' is allowed" "$AGENT" "git checkout '*.md'" "$PC_REPO"
assert_allow_env "git checkout \"<quoted pathspec>\" is allowed" "$AGENT" 'git checkout "*.md"' "$PC_REPO"
assert_allow_env "git checkout \"<path with spaces>\" (present) is allowed" "$AGENT" 'git checkout "space file"' "$PC_REPO"
assert_allow_env "git checkout '<deleted tracked path with spaces>' is allowed" "$AGENT" "git checkout 'space gone'" "$PC_REPO"
assert_allow_env "git checkout 'main' (quoted default branch) is allowed" "$AGENT" "git checkout 'main'" "$PC_REPO"
assert_allow_env "git checkout \"main\" (quoted default branch) is allowed" "$AGENT" 'git checkout "main"' "$PC_REPO"
assert_allow_env "git switch 'main' (quoted default branch) is allowed" "$AGENT" "git switch 'main'" "$PC_REPO"
assert_deny_env "git checkout 'other' (quoted non-default branch) is denied" "$AGENT" "git checkout 'other'" "$PC_REPO"
assert_deny_env "git checkout \"nosuchref\" (quoted unknown target) is denied" "$AGENT" 'git checkout "nosuchref"' "$PC_REPO"

# --- #15003 gap 2: gh global flags before `pr checkout` ---
assert_deny_env "gh --repo owner/repo pr checkout in primary is denied" "$AGENT" \
    "gh --repo 2AMLogic/loom pr checkout 7" "$PC_REPO"
assert_deny_env "gh -R owner/repo pr checkout in primary is denied" "$AGENT" \
    "gh -R 2AMLogic/loom pr checkout 7" "$PC_REPO"
assert_deny_env "gh --repo=owner/repo pr checkout in primary is denied" "$AGENT" \
    "gh --repo=2AMLogic/loom pr checkout 7" "$PC_REPO"
assert_deny_env "gh -Rowner/repo (attached) pr checkout in primary is denied" "$AGENT" \
    "gh -R2AMLogic/loom pr checkout 7" "$PC_REPO"
assert_deny_env "cd <primary> && gh --repo owner/repo pr checkout from a worktree cwd is denied" "$AGENT" \
    "cd $PC_REPO && gh --repo 2AMLogic/loom pr checkout 7" "$PC_WT"
assert_allow_env "gh -h before pr checkout is help, allowed" "$AGENT" "gh -h pr checkout 7" "$PC_REPO"
assert_allow_env "gh ... pr checkout --help is allowed" "$AGENT" \
    "gh -R 2AMLogic/loom pr checkout 7 --help" "$PC_REPO"
assert_allow_env "gh --repo owner/repo pr checkout inside a worktree is allowed" "$AGENT" \
    "gh --repo 2AMLogic/loom pr checkout 7" "$PC_WT"
assert_allow_env "operator session gh --repo ... pr checkout (no agent env) is untouched" "" \
    "gh --repo 2AMLogic/loom pr checkout 7" "$PC_REPO"

# --- #15003 gap 3: bare `git checkout -` is a branch switch, not a flag ---
assert_deny_env "git checkout - (previous-branch switch) is denied" "$AGENT" "git checkout -" "$PC_REPO"
assert_deny_env "git checkout - stays denied with a tracked file named -" "$AGENT" "git checkout -" "$PC_REPO"
assert_allow_env "git checkout -- - (path restore) is allowed" "$AGENT" "git checkout -- -" "$PC_REPO"
assert_allow_env "git checkout - inside a worktree is allowed" "$AGENT" "git checkout -" "$PC_WT"

# --- #15003: the new deny shapes log their documented decision-log tags ---
# (the tag is the JSONL record's pattern field, #3772 — same seam the
# decision-log suite asserts on, see test-guard-destructive-decision-log.sh)
for _pcm_probe in "gh --repo 2AMLogic/loom pr checkout 7:primary-checkout-mutation:gh-pr-checkout" \
                  "git checkout -:primary-checkout-mutation:checkout"; do
    _pcm_cmd="${_pcm_probe%%:*}"
    _pcm_want="${_pcm_probe#*:}"
    _pcm_dl="$(mktemp)"
    make_input "$_pcm_cmd" "$PC_REPO" | \
        env LOOM_SWEEP_ID=step-builder-1-1 LOOM_GUARD_DECISION_LOG=1 \
            LOOM_GUARD_DECISION_LOG_FILE="$_pcm_dl" "$GUARD" >/dev/null 2>&1 || true
    TOTAL=$((TOTAL + 1))
    if [[ "$(tail -1 "$_pcm_dl" 2>/dev/null | jq -r '.pattern' 2>/dev/null)" == "$_pcm_want" ]]; then
        PASS=$((PASS + 1)); echo -e "  ${GREEN}PASS${NC}: $_pcm_cmd denies with tag $_pcm_want"
    else
        FAIL=$((FAIL + 1)); echo -e "  ${RED}FAIL${NC}: $_pcm_cmd denies with tag $_pcm_want"
        echo "       Record: $(cat "$_pcm_dl" 2>/dev/null)"
    fi
    rm -f "$_pcm_dl"
done
unset _pcm_probe _pcm_cmd _pcm_want _pcm_dl


# --- #15003: same-command literal assignment resolves a "$VAR" checkout target ---
assert_allow_env "BR=main; git checkout \"\$BR\" matches the literal branch" "$AGENT" \
    'BR=main; git checkout "$BR"' "$PC_REPO"
assert_allow_env "BR=main && git switch \"\$BR\" matches the literal branch" "$AGENT" \
    'BR=main && git switch "$BR"' "$PC_REPO"
assert_allow_env "BR=main; git checkout \$BR (unquoted) matches the literal branch" "$AGENT" \
    'BR=main; git checkout $BR' "$PC_REPO"
assert_deny_env "BR=other; git checkout \"\$BR\" (non-default) is denied" "$AGENT" \
    'BR=other; git checkout "$BR"' "$PC_REPO"
assert_deny_env "BR=nosuchref; git checkout \"\$BR\" (unknown value) stays denied" "$AGENT" \
    'BR=nosuchref; git checkout "$BR"' "$PC_REPO"
assert_deny_env "single-quoted '\$BR' stays a literal, unresolved target" "$AGENT" \
    'BR=main; git checkout '"'"'$BR'"'"'' "$PC_REPO"
assert_deny_env "env-prefix BR=main git checkout \"\$BR\" stays unresolved" "$AGENT" \
    'BR=main git checkout "$BR"' "$PC_REPO"

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
