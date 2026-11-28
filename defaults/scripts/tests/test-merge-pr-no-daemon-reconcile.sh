#!/usr/bin/env bash
# test-merge-pr-no-daemon-reconcile.sh - an attended `merge-pr.sh` on a host
# with no live loom-daemon runs its own post-merge follow-up (#15714).
#
# Since #14905 the three post-merge issue passes (the partial-increment reset,
# the branch-derived issue close and the closed-issue label strip) and the
# default GitHub worktree cleanup belong to the daemon's merge-observation
# pass. That pass exists only inside a RUNNING loom-daemon, so on a host with
# none -- hand-run roles, which label-state-machine.md supports -- a merged
# `Part of #N` PR left #N at `loom:building` with no Builder, and nothing
# re-queued it.
#
# merge-pr.sh now decides at the confirmed-merge choke point: a daemon pid file
# naming a live process means the pass will do the follow-up (behaviour
# unchanged); anything else means it runs the same three passes inline through
# the very verb the daemon replays (`merge-pr post-merge-bookkeeping`).
#
# Strategy, like test-merge-pr-partial-increment.sh: extract the REAL functions
# from merge-pr.sh and source them, stub `gh` on PATH to serve canned issue
# JSON and record mutating calls, and drive the REAL `loom-daemon merge-pr
# post-merge-bookkeeping` through the REAL lib/forge-helpers.sh writers. The
# daemon-observation probe is the real lib/live-daemon-guard.sh with its
# LOOM_CI_DAEMON_PIDFILE_CANDIDATES seam pointed at a fixture pid file.
#
# Usage:
#   ./.loom/scripts/tests/test-merge-pr-no-daemon-reconcile.sh

# SC2034: several globals (PR_JSON, REPO_NWO, PR_NUMBER, FORGE_TYPE, GH) are
# read only by the functions extracted+sourced from merge-pr.sh, which the
# linter cannot see — every such assignment looks "unused" to it.
# shellcheck disable=SC2034

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
MERGE_PR_SRC="$HELPERS_DIR/merge-pr.sh"

# The one-shot IS `loom-daemon merge-pr post-merge-bookkeeping`; the pre-merge
# guard that publishes the two facts it consumes is `merge-pr-refs` /
# `merge-pr partial-conflict`; its audit-comment bodies are `merge-pr
# partial-comment`. Pin the binary and verify it has each verb — a stale
# binary would make the verb "not run" and every pass below warn instead.
# shellcheck source=lib/require-daemon-bin.sh
source "$SCRIPT_DIR/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin "$HELPERS_DIR" "merge-pr-refs" "merge-pr partial-conflict" "merge-pr post-merge-bookkeeping" "merge-pr partial-comment" "merge-pr-refs loom-issue-trailer-warnings"
# shellcheck source=lib/write-scope-fixture.sh
source "$SCRIPT_DIR/lib/write-scope-fixture.sh"

# Colors
# shellcheck source=lib/assert.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# --- Minimal logging shims the extracted functions call ---
info()    { echo "INFO: $*"; }
success() { echo "OK: $*"; }
warning() { echo "WARN: $*" >&2; }
error()   { echo "ERROR: $*" >&2; exit 1; }

# --- Real forge-helpers.sh (for the #4856 rate-limit-safe mutation wrappers) ---
# Sourced BEFORE the shared globals are assigned: forge-helpers.sh initializes
# FORGE_TYPE="" at load time, which would otherwise clobber the
# FORGE_TYPE="github" the tests rely on.
# shellcheck source=../lib/forge-helpers.sh
source "$HELPERS_DIR/lib/forge-helpers.sh"
# The daemon-observation probe under test: the shared lib, unmodified.
# shellcheck source=../lib/live-daemon-guard.sh
source "$HELPERS_DIR/lib/live-daemon-guard.sh"

# --- Extract the functions under test from merge-pr.sh and source them ---
# One span, bounded by anchor lines that are NOT part of it — the same span
# test-merge-pr-partial-increment.sh extracts: the #4569 pre-merge guard and
# the loom-daemon-backed ref extractor it depends on, so the two globals the
# one-shot consumes below are published by the REAL guard, never typed in.
# Stopping at the INVOCATION line keeps that top-level call out of the sourced
# file (the tests drive the guard explicitly). `_mp_daemon_roll_hint` and the
# `# requires-daemon:` / `# daemon-verb-hint:` markers travel with it (the
# hint reads them back out of ${BASH_SOURCE[0]}).
# The post-merge functions are pulled one each, header line to the first
# column-0 `}`, the way test-merge-pr-daemon-version-floor.sh extracts
# `_warn_if_daemon_lacks_merge_observation`.
FUNCS_FILE="$(mktemp)"
STUB_DIR=""
trap 'rm -rf "$FUNCS_FILE" "$STUB_DIR" 2>/dev/null || true' EXIT
awk '
  /^# requires-daemon:/                                  { print; next }
  /^# daemon-verb-hint:/                                 { print; next }
  /^_mp_daemon_roll_hint\(\) \{/                         { print; next }
  /^_mp_refs\(\) \{/                                     { capture=1 }
  /^_check_partial_increment_close_conflict \|\| true/   { capture=0 }
  capture { print }
' "$MERGE_PR_SRC" > "$FUNCS_FILE"
extract_fn() { # <name> -> that function, verbatim
    awk -v fn="$1" '$0 == fn "() {" { grab=1 } grab { print } grab && /^}/ { exit }' "$MERGE_PR_SRC"
}
for _fn in _warn_if_daemon_lacks_merge_observation _daemon_follows_up _daemon_watches_this_workspace _post_merge_bookkeeping _post_merge_follow_up; do
    if ! extract_fn "$_fn" | grep -q "^${_fn}() {"; then
        echo -e "${RED}FATAL${NC}: could not extract $_fn from $MERGE_PR_SRC" >&2
        exit 2
    fi
    extract_fn "$_fn" >> "$FUNCS_FILE"
done
for _fn in _mp_refs _partial_increment_refs \
           _pr_commit_messages _check_partial_increment_close_conflict; do
    if ! grep -q "^${_fn}() {" "$FUNCS_FILE"; then
        echo -e "${RED}FATAL${NC}: could not extract $_fn from $MERGE_PR_SRC" >&2
        exit 2
    fi
done
# shellcheck disable=SC1090
source "$FUNCS_FILE"

# --- Stub gh on PATH ---
#   gh api repos/OWNER/REPO/issues/N        -> cat $STUB_DIR/issue-N.json (or {})
#   gh api repos/OWNER/REPO/pulls/N/commits -> cat $STUB_DIR/pr-commits.json (or [])
#   gh api .../issues/N/comments --input F  -> record to $STUB_DIR/gh-calls.log
#   gh issue edit|reopen|close N ...        -> record to $STUB_DIR/gh-calls.log
STUB_DIR="$(mktemp -d)"
cat > "$STUB_DIR/gh" <<'STUB'
#!/usr/bin/env bash
# Stub gh for test-merge-pr-no-daemon-reconcile.sh.
STUB_DIR_FROM_ENV="${LOOM_TEST_STUB_DIR:?stub gh: LOOM_TEST_STUB_DIR not set}"
LOG="$STUB_DIR_FROM_ENV/gh-calls.log"

if [[ "$1" == "api" ]]; then
  # The first positional is the endpoint; flags that take a value may precede it.
  shift
  path=""
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --jq|-q|--field|-f|--raw-field|-F|--header|-H|--method|-X)
        shift
        [[ $# -gt 0 ]] && shift
        continue
        ;;
      -*) shift; continue ;;
      *) path="$1"; break ;;
    esac
  done

  case "$path" in
    */pulls/*/commits)
      canned="$STUB_DIR_FROM_ENV/pr-commits.json"
      if [[ -f "$canned" ]]; then cat "$canned"; else echo '[]'; fi
      exit 0
      ;;
    */comments)
      _rest="${path#repos/}"; _repo="${_rest%%/issues/*}"
      _num="${_rest#*/issues/}"; _num="${_num%%/*}"
      echo "issue comment $_num --repo $_repo --body $(jq -r .body "$3" 2>/dev/null)" >> "$LOG"
      exit 0
      ;;
  esac

  num="${path##*/}"
  canned="$STUB_DIR_FROM_ENV/issue-$num.json"
  if [[ -f "$canned" ]]; then cat "$canned"; else echo '{}'; fi
  exit 0
fi

if [[ "$1" == "issue" ]]; then
  # Record the full argv (issue edit / reopen / close) for assertions.
  echo "$*" >> "$LOG"
  exit 0
fi

echo "stub gh: unhandled args: $*" >&2
exit 3
STUB
chmod +x "$STUB_DIR/gh"
export LOOM_TEST_STUB_DIR="$STUB_DIR"
export PATH="$STUB_DIR:$PATH"
# #9548: merge-pr.sh vets its write target through the write scope before it
# writes. It runs from a checkout registered as owner/repo (origin, .loom/, push
# reported to the permission probe), so the real decision admits it.
write_scope_register "$STUB_DIR/checkout" owner/repo
cd "$STUB_DIR/checkout"
# Hermeticity on a fleet host: an agent session exports its live reader pool
# into every child it spawns. The daemon's reads are reader-only and never
# fall back to the writer (#12394), so with LOOM_GH_READER_ROOT inherited a
# read for the fixture repo goes to a reader that cannot serve it and is shed
# -- every pass would then read "state unknown" and change nothing. CI has
# none of these set; here they must not route around the stub either.
unset LOOM_GH_READER_ROOT LOOM_GH_GATEWAY_DIR GH_CONFIG_DIR LOOM_STALE_CLAIM_RECONCILE

# --- Shared globals the functions read (consumed indirectly by the sourced
# functions; see the file-level SC2034 disable at the top). ---
REPO_ROOT="$STUB_DIR/checkout"
REPO_NWO="owner/repo"
PR_NUMBER="999"
PR_BRANCH="feature/issue-555"
FORGE_TYPE="github"
GH="gh"
PARTIAL_OPEN_BEFORE_MERGE=""
PARTIAL_CONFLICT_ISSUES=""

# Shim for the GraphQL-backed close-target helper. Driven by
# $FORGE_CLOSE_TARGETS so tests can hand the passes any close-target set; the
# wrapper only supplies what the verb asks for, the decision is the verb's.
FORGE_CLOSE_TARGETS=""
forge_pr_close_targets() { printf '%s' "$FORGE_CLOSE_TARGETS"; }

# The daemon-observation probe reads ONLY this pid file (the lib's own
# LOOM_CI_DAEMON_PIDFILE_CANDIDATES seam), so the suite answers both ways
# regardless of whether its host runs a real daemon.
PID_FILE="$STUB_DIR/daemon.pid"
export LOOM_CI_DAEMON_PIDFILE_CANDIDATES="$PID_FILE"
: > "$PID_FILE"

# The workspace registry a machine-level daemon visits on every tick
# (`loom-daemon workspace list`'s store), isolated from the host's. By
# default it lists this fixture checkout, so from here on "a daemon is live"
# alone reads as "it watches this workspace"; the P-cases below change that.
REGISTRY="$STUB_DIR/workspaces.json"
export LOOM_WORKSPACES_PATH="$REGISTRY"
write_registry() { # <root...> -> the fixture registry lists exactly these roots
    local sep="" root body
    body='{"version":1,"workspaces":['
    for root in "$@"; do body+="$sep{\"root\":\"$root\"}"; sep=","; done
    printf '%s]}\n' "$body" > "$REGISTRY"
}
OTHER_CHECKOUT="$STUB_DIR/other-checkout"
mkdir -p "$OTHER_CHECKOUT/.loom"
write_registry "$STUB_DIR/checkout"

# Canned issue fixtures.
#   123  the partial-increment issue: open + loom:building   -> reset
#   124  the same issue AFTER a reset (open + loom:issue)    -> idempotent skip
#   555  feature/issue-555's own issue, open, no closing ref -> branch close
#   556  the same, already closed                             -> idempotent skip
#   777  a closed issue still carrying both labels            -> strip
#   778  the same, already stripped                           -> idempotent skip
issue_json() { # <state> <label...> -> canned issue body
    local state="$1"; shift
    local labels="" sep="" l
    for l in "$@"; do labels+="$sep{\"name\":\"$l\"}"; sep=","; done
    printf '{"state":"%s","labels":[%s]}\n' "$state" "$labels"
}
restore_issues() {
    issue_json open loom:building loom:epic > "$STUB_DIR/issue-123.json"
    issue_json open loom:issue                 > "$STUB_DIR/issue-124.json"
    issue_json open                            > "$STUB_DIR/issue-555.json"
    issue_json closed                          > "$STUB_DIR/issue-556.json"
    issue_json closed loom:building loom:issue > "$STUB_DIR/issue-777.json"
    issue_json closed                          > "$STUB_DIR/issue-778.json"
    echo '[]' > "$STUB_DIR/pr-commits.json"
}
restore_issues

reset_case() { # <partial-issue> <branch-issue> [close-targets] -> fresh state
    : > "$STUB_DIR/gh-calls.log"
    PARTIAL_OPEN_BEFORE_MERGE=""; PARTIAL_CONFLICT_ISSUES=""
    FORGE_CLOSE_TARGETS="${3:-}"
    PR_BRANCH="feature/issue-$2"
    PR_JSON="$(jq -n --arg body "Part of #$1" '{body: $body}')"
    unset STACKED_CHILDREN_PIN_WRITTEN
    unset DAEMON_FOLLOWS_UP
}
read_log() { cat "$STUB_DIR/gh-calls.log" 2>/dev/null || true; }
# log_line_of <prefix> -> 1-based number of the first recorded gh write that
# starts with <prefix>, or "" when there is none (grep's exit 1 must not abort
# the suite under `set -e -o pipefail`).
log_line_of() {
    local n=""
    n="$(grep -n -- "^$1" "$STUB_DIR/gh-calls.log" 2>/dev/null | head -1 | cut -d: -f1)" || true
    printf '%s' "${n:-}"
}
# Run the follow-up IN THE CURRENT SHELL (it publishes DAEMON_FOLLOWS_UP),
# with stdout+stderr captured for assertions. Never fails the suite: the
# function's own return code is asserted where it matters.
run_follow_up() {
    _post_merge_follow_up >"$STUB_DIR/out.log" 2>&1 || true
    OUT="$(cat "$STUB_DIR/out.log")"
}

echo "Testing the daemon-observation probe (_daemon_follows_up)..."
# P1: no daemon at all -- the lib's `none` seam.
probe_rc() { if _daemon_follows_up; then echo 0; else echo $?; fi; }
assert_eq "1" "$(LOOM_CI_DAEMON_PIDFILE_CANDIDATES=none probe_rc)" \
    "P1: no pid file anywhere -> no daemon follows up"

# P2: a stale pid file (the process it names is gone).
sleep 0.1 &
STALE_PID=$!
wait "$STALE_PID"
printf '%s\n' "$STALE_PID" > "$PID_FILE"
assert_eq "1" "$(probe_rc)" "P2: a stale pid file -> no daemon follows up (not 'skip the work')"

# P3: an unreadable pid file.
printf 'not a pid\n' > "$PID_FILE"
assert_eq "1" "$(probe_rc)" "P3: an unreadable pid file -> no daemon follows up (fail closed)"

# P4: a pid file naming a live process, and a registry that lists this
# checkout -- this very suite.
printf '%s\n' "$$" > "$PID_FILE"
assert_eq "0" "$(probe_rc)" "P4: a live daemon whose registry lists this checkout -> its pass follows up"

# P5: the shared lib missing (a partially resynced .loom/) reads as "no daemon".
assert_eq "1" "$(unset -f live_daemon_pidfiles_present; probe_rc)" \
    "P5: no probe lib -> no daemon follows up (the inline work runs)"

# P6: the merge-observation pass is GitHub-only; on another forge nothing observes.
assert_eq "1" "$(FORGE_TYPE=gitea probe_rc)" "P6: a non-GitHub forge -> no daemon follows up"

# P7: a live MACHINE-level daemon that does not visit this checkout (its
# registry lists other roots only) is not observing this merge.
write_registry "$OTHER_CHECKOUT"
assert_eq "1" "$(probe_rc)" "P7: a live daemon whose registry lists only other workspaces -> no daemon follows up"

# P8: an EMPTY registry. The daemon then falls back to its own cwd, which no
# other process can see: unknown, and unknown runs the work inline.
write_registry
assert_eq "1" "$(probe_rc)" "P8: a live daemon with an empty registry (its cwd is unknowable) -> no daemon follows up"

# P9: no registry at all, or an unreadable one.
rm -f "$REGISTRY"
assert_eq "1" "$(probe_rc)" "P9: a live daemon with no registry -> no daemon follows up"
printf 'not json' > "$REGISTRY"
assert_eq "1" "$(probe_rc)" "P9: a live daemon with an unreadable registry -> no daemon follows up"

# P10: a pid file INSIDE a checkout is that checkout's own (repo-mode)
# daemon: it watches this workspace whatever the machine registry says, and
# another checkout's own daemon does not watch this one.
REPO_PID_FILE="$STUB_DIR/checkout/.loom/.daemon.pid"
OTHER_PID_FILE="$OTHER_CHECKOUT/.loom/.daemon.pid"
printf '%s\n' "$$" > "$REPO_PID_FILE"
printf '%s\n' "$$" > "$OTHER_PID_FILE"
assert_eq "0" "$(LOOM_CI_DAEMON_PIDFILE_CANDIDATES="$REPO_PID_FILE" probe_rc)" \
    "P10: this checkout's own daemon -> its pass follows up"
write_registry "$OTHER_CHECKOUT"
assert_eq "0" "$(LOOM_CI_DAEMON_PIDFILE_CANDIDATES="$REPO_PID_FILE" probe_rc)" \
    "P10: ...whatever the machine registry lists"
assert_eq "1" "$(LOOM_CI_DAEMON_PIDFILE_CANDIDATES="$OTHER_PID_FILE" probe_rc)" \
    "P10: another checkout's own daemon -> no daemon follows up"
rm -f "$REPO_PID_FILE" "$OTHER_PID_FILE"
write_registry "$STUB_DIR/checkout"

# P11: the reconciliation pass's own kill switch (`reconciliation_enabled`),
# inherited by a daemon-spawned session.
assert_eq "1" "$(LOOM_STALE_CLAIM_RECONCILE=0 probe_rc)" "P11: LOOM_STALE_CLAIM_RECONCILE=0 -> the pass is off, no daemon follows up"
assert_eq "1" "$(LOOM_STALE_CLAIM_RECONCILE=OFF probe_rc)" "P11: ...case-insensitively"
assert_eq "0" "$(LOOM_STALE_CLAIM_RECONCILE=1 probe_rc)" "P11: an explicit 'on' changes nothing"
write_registry "$STUB_DIR/checkout"

: > "$PID_FILE"

echo ""
echo "Testing the attended one-shot: no daemon -> the three passes run inline (#15714)..."
reset_case 123 555 "777"
# The pre-merge guard runs exactly as merge-pr.sh runs it (top level, not in a
# command substitution, so its `error` exit can act), so the two facts the
# one-shot consumes are published by the real guard for this PR body.
_check_partial_increment_close_conflict 2>/dev/null || true
assert_eq "123" "$PARTIAL_OPEN_BEFORE_MERGE" "M0: the pre-merge guard records #123 as open before the merge"
assert_eq "" "$PARTIAL_CONFLICT_ISSUES" "M0: a plain 'Part of #123' body declares no closing reference"
run_follow_up
LOG="$(read_log)"
assert_eq "false" "${DAEMON_FOLLOWS_UP:-<unset>}" "M1: no daemon -> DAEMON_FOLLOWS_UP=false (the cleanup gate cleans up inline)"
assert_contains "$OUT" "No running loom-daemon watches this workspace" "M1: the operator is told the follow-up runs here"
assert_contains "$LOG" "issue edit 123 --repo owner/repo --remove-label loom:building --add-label loom:issue" \
    "M1: the partial-increment issue goes back to the ready queue (loom:building -> loom:issue)"
assert_contains "$LOG" "issue comment 123 --repo owner/repo" \
    "M1: the reset posts its audit comment"
assert_contains "$LOG" "issue close 555 --repo owner/repo --reason completed" \
    "M2: a feature/issue-N PR with no closing keyword closes #N (branch-derived close, #12615)"
assert_contains "$LOG" "issue edit 777 --repo owner/repo --remove-label loom:building" \
    "M3: a closed issue's loom:building is stripped (#6199)"
assert_contains "$LOG" "issue edit 777 --repo owner/repo --remove-label loom:issue" \
    "M3: a closed issue's loom:issue is stripped (#11517)"
# M4: the order is load-bearing (reset, branch close, strip) -- pin it by the
# first recorded line of each pass.
FIRST_EDIT_123="$(log_line_of 'issue edit 123 ')"
FIRST_CLOSE_555="$(log_line_of 'issue close 555 ')"
FIRST_EDIT_777="$(log_line_of 'issue edit 777 ')"
if [[ -n "$FIRST_EDIT_123" && -n "$FIRST_CLOSE_555" && -n "$FIRST_EDIT_777" \
      && "$FIRST_EDIT_123" -lt "$FIRST_CLOSE_555" && "$FIRST_CLOSE_555" -lt "$FIRST_EDIT_777" ]]; then
    pass "M4: reset precedes branch close precedes strip (the daemon's order)"
else
    fail "M4: pass order wrong (edit123=$FIRST_EDIT_123 close555=$FIRST_CLOSE_555 edit777=$FIRST_EDIT_777)"
fi
assert_not_contains "$OUT" "reconciliation tick of a running loom-daemon" \
    "M5: the daemon path's advisory is not printed on the inline path"
assert_contains "$OUT" "reconcile-stack.sh <child-pr> feature/issue-555" \
    "M6: the stacked-child restack (not run here) names its manual command"
assert_not_contains "$OUT" "WARN: Stacked children" \
    "M6: with no stacked child discovered the stack note stays informational"

# M7: the pre-merge guard found open stacked children (it pinned the parent
# tip), so the restack that did not run is called out as a warning.
reset_case 123 555 "777"
STACKED_CHILDREN_PIN_WRITTEN=true
run_follow_up
assert_contains "$OUT" "WARN: Stacked children of 'feature/issue-555' were not restacked" \
    "M7: with a stacked child discovered the stack note is a warning"

echo ""
echo "Testing the #4569 premature-close revert through the one-shot..."
# The incident: a body that declares `Part of #123` but also says "close #123"
# in prose. GitHub closes #123 anyway; the real guard records the conflict,
# and the one-shot reopens the issue before returning it to the queue.
reset_case 123 555 ""
PR_JSON="$(jq -n --arg body $'Summary.\n\nPart of #123\n\nOperator follow-up (after merge): then close #123.' '{body: $body}')"
# Pre-merge: #123 is still open when the guard runs.
_check_partial_increment_close_conflict 2>/dev/null || true
assert_eq "123" "$PARTIAL_CONFLICT_ISSUES" "R0: the pre-merge guard records the closing reference to #123"
assert_eq "123" "$PARTIAL_OPEN_BEFORE_MERGE" "R0: ...and #123 as open before the merge"
# The merge lands: GitHub honors the stray keyword and closes #123.
issue_json closed loom:building > "$STUB_DIR/issue-123.json"
run_follow_up
LOG="$(read_log)"
assert_contains "$LOG" "issue reopen 123 --repo owner/repo" \
    "R1: an issue this merge closed against its declaration is reopened"
assert_contains "$LOG" "issue edit 123 --repo owner/repo --remove-label loom:building --add-label loom:issue" \
    "R1: ...then returned to the ready queue"
assert_contains "$OUT" "reopened (premature auto-close reverted)" "R1: the reopen is reported"
REOPEN_LINE="$(log_line_of 'issue reopen 123 ')"
SWAP_LINE="$(log_line_of 'issue edit 123 ')"
if [[ -n "$REOPEN_LINE" && -n "$SWAP_LINE" && "$REOPEN_LINE" -lt "$SWAP_LINE" ]]; then
    pass "R1: the reopen precedes the swap"
else
    fail "R1: reopen/swap order wrong (reopen=$REOPEN_LINE swap=$SWAP_LINE)"
fi

# R2: a deliberate close the PR did not cause is NOT reverted: open before the
# merge, closed after, but no closing reference on this PR.
reset_case 123 555 ""
PARTIAL_OPEN_BEFORE_MERGE="123"
run_follow_up
LOG="$(read_log)"
assert_not_contains "$LOG" "issue reopen 123" "R2: an unattributed close is left alone"
assert_not_contains "$LOG" "issue edit 123" "R2: ...and a closed issue is not swapped"
assert_contains "$OUT" "NOT reopening automatically" "R2: the operator is told why"
restore_issues

echo ""
echo "Testing idempotence: a follow-up that already ran changes nothing..."
# What a daemon's tick (earlier or later) leaves behind: every pass re-reads
# the issue and skips what is already done, so script-then-tick converges.
reset_case 124 556 "778"
run_follow_up
LOG="$(read_log)"
assert_eq "" "$LOG" "I1: an already-reset issue, an already-closed issue and an already-stripped issue draw no write"
assert_contains "$OUT" "is not loom:building — skipping (idempotent)" "I1: the verb reports the idempotent reset skip"

echo ""
echo "Testing a live daemon: behaviour is exactly what it was (#14905)..."
reset_case 123 555 "777"
PARTIAL_OPEN_BEFORE_MERGE="123"
printf '%s\n' "$$" > "$PID_FILE"
run_follow_up
assert_eq "true" "${DAEMON_FOLLOWS_UP:-<unset>}" "D1: a live daemon watching this workspace -> DAEMON_FOLLOWS_UP=true (cleanup stays the daemon's)"
assert_eq "" "$(read_log)" "D1: no second pass -- not one issue write is made inline"
assert_contains "$OUT" "reconciliation tick of a running loom-daemon" "D1: the daemon path's own advisory is the one printed"
: > "$PID_FILE"

echo ""
echo "Testing a daemon too old for the verb: the merge is unaffected, the remedy is named..."
make_fake_daemon() { # <path> <script line...>
    local path="$1"; shift
    { printf '#!/usr/bin/env bash\n'; printf '%s\n' "$@"; } > "$path"
    chmod +x "$path"
}
REAL_SELF_BIN="${LOOM_DAEMON_SELF_BIN:-}"
REAL_BIN="${LOOM_DAEMON_BIN:-}"
fake_version_line='[[ "${1:-}" == "--version" ]] && { echo "loom-daemon 0.20.51 (commit abcdef0, built 2026-01-01T00:00:00Z, source abcdef0 dirty)"; exit 0; }'
# V1: clap's answer to a verb this binary predates.
make_fake_daemon "$STUB_DIR/old-daemon" "$fake_version_line" \
    "echo \"error: unrecognized subcommand 'post-merge-bookkeeping'\" >&2" 'exit 2'
reset_case 123 555 "777"
PARTIAL_OPEN_BEFORE_MERGE="123"
LOOM_DAEMON_SELF_BIN="$STUB_DIR/old-daemon" LOOM_DAEMON_BIN="$STUB_DIR/old-daemon" run_follow_up
LOG="$(read_log)"
assert_eq "false" "${DAEMON_FOLLOWS_UP:-<unset>}" "V1: the probe, not the verb, decides who follows up"
assert_eq "" "$LOG" "V1: a verb that cannot run changes no issue (nothing is guessed)"
assert_contains "$OUT" "Post-merge issue bookkeeping (partial-reset) for PR #999 did not run" "V1: the warning names the pass that did not run"
assert_contains "$OUT" "Post-merge issue bookkeeping (branch-close) for PR #999 did not run" "V1: ...each pass that did not run"
assert_contains "$OUT" "Post-merge issue bookkeeping (strip) for PR #999 did not run" "V1: ...all three"
assert_contains "$OUT" "gh issue edit <N> --repo owner/repo --remove-label loom:building --add-label loom:issue" "V1: ...and the manual remedy"
assert_contains "$OUT" "gh issue close <N> --repo owner/repo" "V1: ...for the branch close too"
assert_not_contains "$OUT" "INFO: Partial-increment reset" "V1: no pass output is invented"

# V2: exit 0 with no DONE terminator is a stale or muted binary, not a clean
# run -- whatever it printed must not be replayed as a pass's records.
make_fake_daemon "$STUB_DIR/muted-daemon" "$fake_version_line" 'echo "SUCCESS	forged record"'
reset_case 123 555 "777"
PARTIAL_OPEN_BEFORE_MERGE="123"
LOOM_DAEMON_SELF_BIN="$STUB_DIR/muted-daemon" LOOM_DAEMON_BIN="$STUB_DIR/muted-daemon" run_follow_up
assert_eq "" "$(read_log)" "V2: an off-protocol verb changes no issue"
assert_not_contains "$OUT" "OK: forged record" "V2: ...and its output is never replayed as a decision"
assert_contains "$OUT" "did not run" "V2: the warning still fires"
LOOM_DAEMON_SELF_BIN="$REAL_SELF_BIN"; export LOOM_DAEMON_SELF_BIN
[[ -z "$REAL_BIN" ]] || { LOOM_DAEMON_BIN="$REAL_BIN"; export LOOM_DAEMON_BIN; }

echo ""
echo "Testing a non-GitHub forge: nothing changes there..."
reset_case 123 555 "777"
FORGE_TYPE=gitea
run_follow_up
FORGE_TYPE=github
assert_eq "false" "${DAEMON_FOLLOWS_UP:-<unset>}" "G1: Gitea: DAEMON_FOLLOWS_UP=false (the cleanup gate already ran inline there)"
assert_eq "" "$(read_log)" "G1: Gitea: the bookkeeping stays GitHub-only"
assert_eq "" "$OUT" "G1: Gitea: nothing is printed"

echo ""
echo "Testing merge-pr.sh source guards..."
src="$(cat "$MERGE_PR_SRC")"
assert_contains "$src" 'if [[ -f "$SCRIPT_DIR/lib/live-daemon-guard.sh" ]]; then' \
    "S1: the probe lib is sourced guarded (a missing lib cannot stop a merge)"
assert_not_contains "$src" 'pgrep -f' \
    "S1: the probe is the pid-file lib, never pgrep (which cannot see the daemon on macOS, #6386)"
src_line_of() { # <regex> -> number of the first merge-pr.sh line matching it, or ""
    local n=""
    n="$(grep -n -- "$1" "$MERGE_PR_SRC" | head -1 | cut -d: -f1)" || true
    printf '%s' "${n:-}"
}
follow_line="$(src_line_of '^_post_merge_follow_up || true$')"
merged_line="$(src_line_of '^success "PR #\$PR_NUMBER merged successfully"$')"
notify_line="$(src_line_of '^_notify_cleared_blockers || true$')"
if [[ -n "$follow_line" && -n "$merged_line" && -n "$notify_line" \
      && "$merged_line" -lt "$follow_line" && "$follow_line" -lt "$notify_line" ]]; then
    pass "S2: the follow-up runs once, after the confirmed merge and before the cleared-blocker advisory"
else
    fail "S2: follow-up call site moved (merged=$merged_line follow=$follow_line notify=$notify_line)"
fi
assert_eq "0" "$(grep -c '^_warn_if_daemon_lacks_merge_observation$' "$MERGE_PR_SRC" || true)" \
    "S2: the old top-level advisory call is gone (it runs inside _post_merge_follow_up now)"
assert_contains "$src" '"${DAEMON_FOLLOWS_UP:-false}" != "true"' \
    "S3: the cleanup gate runs inline when the daemon does not follow up"
if grep -A 3 '^_daemon_follows_up() {' "$MERGE_PR_SRC" | grep -q 'live_daemon_pidfiles_present'; then
    pass "S4: the probe delegates to the shared lib rather than a second pid-file list"
else
    fail "S4: the probe no longer uses live_daemon_pidfiles_present"
fi
assert_contains "$(extract_fn _daemon_watches_this_workspace)" 'LOOM_WORKSPACES_PATH' \
    "S5: workspace scoping reads the registry through the daemon's own LOOM_WORKSPACES_PATH seam"

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
