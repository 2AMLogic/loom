#!/usr/bin/env bash
# Test suite for the hermetic baseline in
# tests/hooks/lib/guard-destructive-harness.sh (issue #15512).
#
# Usage: ./tests/hooks/test-guard-destructive-harness-baseline.sh
#
# The harness unsets the ambient guard-behavior overrides before any suite
# runs an assertion (#4325, #15496). CI never exports those variables, so a
# name dropped from that `unset` passes every other suite and fails only for a
# dispatched agent, whose session exports the agent-step markers.
#
# This suite builds the dirty environment itself: an `env -i` bash exports a
# nonempty sentinel for every expected name, sources the real harness, and
# reports each name that is still set. Exit 0 = all tests pass, 1 = failures.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HARNESS="$SCRIPT_DIR/lib/guard-destructive-harness.sh"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'
PASS=0
FAIL=0

check() {
  local desc="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    echo -e "  ${GREEN}PASS${NC}: $desc"
    PASS=$((PASS + 1))
  else
    echo -e "  ${RED}FAIL${NC}: $desc (expected '$expected', got '$actual')"
    FAIL=$((FAIL + 1))
  fi
}

# The harness's hermetic baseline. Test data on purpose (#15512): reading it
# from the harness's own `unset` would make this suite agree with any
# regression in that statement. A name added to the harness needs an entry here.
BASELINE_NAMES=(
  LOOM_FORCE_SCOPE
  LOOM_DEFAULT_BRANCH
  LOOM_GUARD_SQL
  LOOM_GUARD_CLOUD
  LOOM_GUARD_REVERSIBLE_GH
  LOOM_RM_SCOPE
  LOOM_GUARD_READONLY_FASTPATH
  LOOM_GUARD_WORKTREE_ISOLATION
  LOOM_WORKTREE_PATH
  LOOM_WORKTREE_ROOT
  LOOM_WORKSPACE
  LOOM_GUARD_DECISION_LOG
  LOOM_GUARD_DECISION_LOG_FILE
  LOOM_GUARD_STASH_SCOPE
  LOOM_ROLE
  LOOM_GUARD_CARGO_CLEAN
  CARGO_TARGET_DIR
  LOOM_GUARD_SCRATCH_ROOT
  LOOM_SWEEP_ID
)

[[ -r "$HARNESS" ]] || { echo "harness not readable: $HARNESS" >&2; exit 1; }

WORK="$(mktemp -d 2>/dev/null || mktemp -d -t loom-harness-baseline)"
trap 'rm -rf "$WORK"' EXIT
GUARD_FIXTURE="$WORK/guard-fixture.sh"
printf '#!/usr/bin/env bash\nexit 0\n' > "$GUARD_FIXTURE"

# The probe runs with only the environment named below, so nothing inherited
# from the caller can stand in for a sentinel or satisfy a check.
cat > "$WORK/probe.sh" <<'PROBE'
# <harness> <guard fixture> <expected baseline name>...
HARNESS="$1"
GUARD_FIXTURE="$2"
shift 2
set -euo pipefail

for name in "$@"; do
  export "$name=harness-baseline-sentinel"
done
export LOOM_GUARD_OVERRIDE="$GUARD_FIXTURE"

# Sourcing adopts the harness's shell options, traps and throwaway repo. Only
# its banner goes to stdout, so drop that and keep the diagnostics on stderr.
. "$HARNESS" >/dev/null

for name in "$@"; do
  TOTAL=$((TOTAL + 1))
  if [[ -n "${!name+x}" ]]; then
    FAIL=$((FAIL + 1))
    echo -e "  ${RED}FAIL${NC}: harness unsets $name"
    if [[ -n "${!name}" ]]; then
      echo "       Still set, to '${!name}'"
    else
      echo "       Still set, to an empty string"
    fi
  else
    PASS=$((PASS + 1))
    echo -e "  ${GREEN}PASS${NC}: harness unsets $name"
  fi
done

TOTAL=$((TOTAL + 1))
if [[ "$GUARD" == "$GUARD_FIXTURE" ]]; then
  PASS=$((PASS + 1))
  echo -e "  ${GREEN}PASS${NC}: LOOM_GUARD_OVERRIDE is selected as GUARD"
else
  FAIL=$((FAIL + 1))
  echo -e "  ${RED}FAIL${NC}: LOOM_GUARD_OVERRIDE is selected as GUARD"
  echo "       Expected: $GUARD_FIXTURE"
  echo "       Got: ${GUARD:-<unset>}"
fi

print_summary
PROBE

probe_rc=0
# The parent's EXIT trap also removes the test repo if harness setup aborts.
probe_out="$(env -i PATH="$PATH" HOME="${HOME:-/tmp}" TMPDIR="$WORK" \
    "$BASH" "$WORK/probe.sh" "$HARNESS" "$GUARD_FIXTURE" "${BASELINE_NAMES[@]}" 2>&1)" \
    || probe_rc=$?

echo ""
echo -e "=== Testing guard-destructive-harness.sh hermetic baseline ==="
echo ""
printf '%s\n' "$probe_out"
echo ""

check "probe exit code (0 = every baseline name unset, LOOM_GUARD_OVERRIDE selected)" \
    "0" "$probe_rc"

# Fail closed: a probe that died before its summary is not a pass, whatever
# its exit code says.
TOTAL_RE='Total:[[:space:]]+([0-9]+)'
checked="none"
if [[ "$probe_out" =~ $TOTAL_RE ]]; then
  checked="${BASH_REMATCH[1]}"
fi
check "probe asserted every expected name plus LOOM_GUARD_OVERRIDE" \
    "$(( ${#BASELINE_NAMES[@]} + 1 ))" "$checked"

echo ""
echo "========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed"
echo "========================================="
[[ "$FAIL" -eq 0 ]] || exit 1
