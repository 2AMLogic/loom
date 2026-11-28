#!/usr/bin/env bash
# test-forge-helpers-resolve-failure.sh - Regression tests for the config
# resolution failure diagnostic in lib/forge-helpers.sh (#15793).
#
# forge_detect and _load_gitea_config resolve the effective config through
# `loom_resolve_config`. When the resolver itself fails, the only stderr is
# jq's own ("Argument list too long" from a stale installed copy), so a caller
# such as create-issue.sh exited 126 with nothing naming the failed step or
# the root it resolved for -- and a caller that tested the function in an `||`
# context carried on with an empty config instead.
#
# This file tests, with a stub `jq` that prints one line and exits 126:
#   1. forge_detect fails closed: status 126, the stub's own stderr kept, and
#      exactly one diagnostic line naming "config resolution failed" and the
#      root. Same under `set -e` (the issue's reproduction) and in an `||`
#      context, and on the LOOM_FORGE_TYPE=gitea path, which must not leave
#      FORGE_TYPE set behind a failed load.
#   2. _load_gitea_config called directly fails the same way.
#   3. With a working jq, forge_detect is unchanged: forge.type and the Gitea
#      settings still come from the config tiers, and no config still falls
#      through to the github default.
#
# Usage:
#   ./.loom/scripts/tests/test-forge-helpers-resolve-failure.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# Colors
# shellcheck source=lib/assert.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-forge-resolve-failure.XXXXXX")"
# shellcheck disable=SC2329,SC2317  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

# The config root forge_detect resolves: a Gitea legacy tier. The
# private-defaults tier is pinned off so the host's own
# ~/.local/share/loom/config/defaults.json cannot contribute.
CFG_ROOT="$WORKDIR/repo"
mkdir -p "$CFG_ROOT/.loom"
cat > "$CFG_ROOT/.loom/config.json" <<'EOF'
{"forge": {"type": "gitea", "gitea": {"url": "https://gitea.example.com", "token": "tok-fixture", "username": "fixture-user"}}}
EOF

# A jq that is present (`command -v jq` passes, so the resolver is not
# short-circuited to '{}') but fails every call.
STUB_BIN="$WORKDIR/stub-bin"
mkdir -p "$STUB_BIN"
cat > "$STUB_BIN/jq" <<'STUB'
#!/usr/bin/env bash
echo "stub jq: boom" >&2
exit 126
STUB
chmod +x "$STUB_BIN/jq"

RUN_STDOUT="$WORKDIR/stdout.txt"
RUN_STDERR="$WORKDIR/stderr.txt"
RUN_RC=0

# The child process _run starts. It is a fresh bash rather than a subshell:
# `set -e` is ignored inside a subshell that sits on the left of an `||`, even
# when re-enabled explicitly, so the "errexit" mode could not be exercised
# from this suite's own shell.
# shellcheck disable=SC2016  # expanded by the child bash, not here
RUN_DRIVER='
set -euo pipefail
source "$1/lib/forge-helpers.sh"
shift
cd "$RUN_ROOT"
[[ -z "$STUB_BIN" ]] || export PATH="$STUB_BIN:$PATH"
export REPO_ROOT="$RUN_ROOT" LOOM_CONFIG_DEFAULTS_FILE=""
unset LOOM_FORGE_TYPE GITEA_TOKEN GITEA_URL GITEA_USERNAME
fn="$1"
shift
for kv in "$@"; do export "$kv"; done
rc=0
if [[ "$RUN_MODE" == "errexit" ]]; then
    "$fn"
else
    "$fn" || rc=$?
    printf "forge=[%s]\ngitea_url=[%s]\ngitea_token=[%s]\ngitea_user=[%s]\n" \
        "$FORGE_TYPE" "$_GITEA_BASE_URL" "$_GITEA_TOKEN" "$_GITEA_USERNAME" >&2
fi
exit "$rc"
'

# _run <errexit|checked> <stub-bin> <root> <function> [VAR=value ...]
# Runs <function> in the child above with <root> as the config root and the
# extra VAR=value pairs exported. <stub-bin> is prepended to PATH when
# non-empty. "errexit" calls the function as a plain statement under `set -e`;
# "checked" calls it as `|| rc=$?` and reports the resulting forge state on
# stderr. Leaves the status in $RUN_RC, stdout in $RUN_STDOUT and stderr in
# $RUN_STDERR.
_run() {
    local mode="$1" stub_bin="$2" root="$3"
    shift 3
    RUN_RC=0
    RUN_MODE="$mode" RUN_ROOT="$root" STUB_BIN="$stub_bin" \
        bash -c "$RUN_DRIVER" _ "$HELPERS_DIR" "$@" >"$RUN_STDOUT" 2>"$RUN_STDERR" || RUN_RC=$?
}

# _stub_run <errexit|checked> <function> [VAR=value ...] -- _run under the
# stub jq with the Gitea fixture as the config root.
_stub_run() {
    local mode="$1"
    shift
    _run "$mode" "$STUB_BIN" "$CFG_ROOT" "$@"
}

# _has_line <text> <line> -> 0 when <line> is a whole line of <text>
_has_line() {
    local line
    while IFS= read -r line; do
        if [[ "$line" == "$2" ]]; then return 0; fi
    done <<<"$1"
    return 1
}

# _assert_failed_closed <label> -> the last _stub_run failed the way #15793
# requires: status 126, the stub's own stderr kept, exactly one diagnostic
# line carrying that status and the root, and nothing on stdout.
_assert_failed_closed() {
    local label="$1" err out diag=0 line
    err="$(cat "$RUN_STDERR")"
    out="$(cat "$RUN_STDOUT")"
    assert_eq "126" "$RUN_RC" "$label: exits with the resolver's status"
    assert_contains "$err" "stub jq: boom" "$label: the resolver's own stderr is kept"
    while IFS= read -r line; do
        if [[ "$line" == *"config resolution failed"* ]]; then diag=$((diag + 1)); fi
    done <<<"$err"
    assert_eq "1" "$diag" "$label: exactly one diagnostic line names the failed step"
    assert_contains "$err" "config resolution failed (rc=126) for $CFG_ROOT" \
        "$label: the diagnostic carries the status and the root it resolved for"
    assert_eq "" "$out" "$label: nothing on stdout"
}

# --- 1. forge_detect, config-tier path ---
echo "Testing forge_detect when config resolution fails..."

_stub_run errexit forge_detect
_assert_failed_closed "forge_detect under set -e"

_stub_run checked forge_detect
_assert_failed_closed "forge_detect in an || context"
if _has_line "$(cat "$RUN_STDERR")" "forge=[]"; then
    pass "forge_detect in an || context: a failed detect leaves FORGE_TYPE unset"
else
    fail "forge_detect in an || context: a failed detect must leave FORGE_TYPE unset"
fi

# --- 2. forge_detect, LOOM_FORGE_TYPE=gitea path (through _load_gitea_config) ---
echo ""
echo "Testing LOOM_FORGE_TYPE=gitea when config resolution fails..."

_stub_run errexit forge_detect LOOM_FORGE_TYPE=gitea
_assert_failed_closed "LOOM_FORGE_TYPE=gitea under set -e"

_stub_run checked forge_detect LOOM_FORGE_TYPE=gitea
_assert_failed_closed "LOOM_FORGE_TYPE=gitea in an || context"
if _has_line "$(cat "$RUN_STDERR")" "forge=[]"; then
    pass "LOOM_FORGE_TYPE=gitea in an || context: a failed detect leaves FORGE_TYPE unset"
else
    fail "LOOM_FORGE_TYPE=gitea in an || context: a failed detect must leave FORGE_TYPE unset"
fi

# --- 3. _load_gitea_config directly ---
echo ""
echo "Testing _load_gitea_config when config resolution fails..."

_stub_run errexit _load_gitea_config
_assert_failed_closed "_load_gitea_config under set -e"

_stub_run checked _load_gitea_config
_assert_failed_closed "_load_gitea_config in an || context"

# --- 4. Working jq: forge_detect is unchanged ---
echo ""
echo "Testing forge_detect with a working jq..."

_run checked "" "$CFG_ROOT" forge_detect
assert_eq "0" "$RUN_RC" "forge_detect succeeds against the Gitea config"
err="$(cat "$RUN_STDERR")"
for state in "forge=[gitea]" "gitea_url=[https://gitea.example.com]" \
    "gitea_token=[tok-fixture]" "gitea_user=[fixture-user]"; do
    if _has_line "$err" "$state"; then
        pass "forge.type and the Gitea settings come from the config tiers: $state"
    else
        fail "forge.type and the Gitea settings come from the config tiers: expected line $state"
    fi
done
assert_eq "" "$(cat "$RUN_STDOUT")" "forge_detect prints nothing on stdout"

EMPTY_ROOT="$WORKDIR/empty"
mkdir -p "$EMPTY_ROOT"
_run checked "" "$EMPTY_ROOT" forge_detect
assert_eq "0" "$RUN_RC" "forge_detect succeeds with no config tiers"
if _has_line "$(cat "$RUN_STDERR")" "forge=[github]"; then
    pass "no config tiers still falls through to the github default"
else
    fail "no config tiers must still fall through to the github default"
fi

# --- Summary ---
echo ""
echo "────────────────────────────────"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"

if [[ $TESTS_FAILED -gt 0 ]]; then
    exit 1
fi
exit 0
