#!/usr/bin/env bash
# test-spawn-agy.sh — Tests for spawn-agy.sh, the tier-3 adapter for Google's
# Antigravity CLI (issue #15109): the per-launch isolated HOME it creates, and
# the launch-time sweep that reclaims abandoned ones.
#
# Style matches test-spawn-generic-launch.sh — plain bash, hand-rolled
# assertions, hermetic: a stub `agy` on PATH (no live CLI, no network), a
# scratch TMPDIR holding every per-launch HOME, and the real
# `loom-daemon runtime-launch-env` pinned via loom_test_require_daemon_bin,
# because spawn-generic-launch.sh resolves the launch shape through it.
#
# Covers:
#   1. syntax + executable
#   2. a launch creates ONE isolated HOME under TMPDIR holding the gemini
#      settings file and a `.loom-pid` marker naming a pid that is alive for
#      the launch and gone once it ends — through the default `nice` re-exec
#      as well as with it disabled
#   3. the sweep removes ONLY a same-owner, past-cutoff directory whose valid
#      marker names a process confirmed gone. Everything else is kept: a HOME
#      whose marker names a running process, a live legacy HOME with no
#      marker, an abandoned marker-less HOME, an empty / malformed / unreadable
#      marker, a fresh dead-pid HOME, and a foreign-owner directory (asserted
#      only under root)
#   4. the stubs whose HOME survived the sweep can still open settings.json
#   5. a failing sweep neither fails nor changes the launch (`find` shadowed)
#   6. N launches leave no abandoned HOME past the cutoff
#   7. the two adapter-doc claims the issue found stale
#
# Usage:
#   bash defaults/scripts/tests/test-spawn-agy.sh

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=lib/assert.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"
# shellcheck source=lib/bg-proc-trap.sh
source "$SCRIPT_DIR/lib/bg-proc-trap.sh"

TMPROOT="$(mktemp -d)"
# `set +e` leads each trap: bg_proc_reap's `kill` fails for a fixture that has
# already exited, and under this suite's `set -e` that would abort the trap
# before `rm -rf` runs and leak the scratch tree.
trap 'set +e; bg_proc_reap; rm -rf "$TMPROOT"' EXIT
trap 'set +e; bg_proc_reap; rm -rf "$TMPROOT"; exit 1' INT TERM

# The port under test is `loom-daemon runtime-launch-env` (#8671), which
# spawn-generic-launch.sh calls to resolve agy's launch shape; same rationale
# as test-spawn-generic-launch.sh (see lib/require-daemon-bin.sh).
source "$SCRIPT_DIR/lib/require-daemon-bin.sh"
loom_test_require_daemon_bin --self-only "$SCRIPTS_DIR" runtime-launch-env

# backdate <path> <seconds-ago> — portable mtime backdating: `touch -d @ts` on
# GNU (Linux), `touch -t` on BSD/macOS.
backdate() {
    local path="$1" epoch stamp
    epoch=$(( $(date +%s) - $2 ))
    touch -d "@$epoch" "$path" 2>/dev/null && return 0
    stamp="$(date -r "$epoch" +%Y%m%d%H%M.%S 2>/dev/null || true)"
    [[ -n "$stamp" ]] || { echo "FATAL: cannot backdate $path" >&2; exit 1; }
    touch -t "$stamp" "$path"
}

# count_homes <dir> — number of loom-agy-* directories directly under <dir>.
count_homes() {
    local n=0 f
    for f in "$1"/loom-agy-*; do
        [[ -d "$f" ]] && n=$((n + 1))
    done
    echo "$n"
}

# mode_of <path> — permission bits as an octet (GNU `stat -c`, BSD `stat -f`),
# or "missing" for a path that does not exist.
mode_of() {
    stat -c '%a' "$1" 2>/dev/null || stat -f '%Lp' "$1" 2>/dev/null || echo "missing"
}

# dead_pid — prints a pid that has exited AND been reaped, so `kill -0` on it
# is ESRCH. Not handed to bg_proc_track: it is waited on right here, and a
# tracked pid that is already dead would be signalled again by the EXIT trap
# after the host may have reused it.
dead_pid() {
    local pid
    sleep 0.2 &
    pid=$!
    wait "$pid" || true
    echo "$pid"
}

# make_home <dir> [marker] — a same-owner loom-agy-* HOME holding the settings
# file. No marker unless <marker> is given; the literal "-" writes an EMPTY
# marker.
make_home() {
    mkdir -p "$1"
    printf '{"modelProvider": "gemini"}\n' >"$1/settings.json"
    if [[ "${2-}" == "-" ]]; then
        : >"$1/.loom-pid"
    elif [[ -n "${2-}" ]]; then
        printf '%s\n' "$2" >"$1/.loom-pid"
    fi
}

# live_stub <home> <trigger> <report> — a stand-in for a running agy whose HOME
# is <home>. It waits until <trigger> appears, records whether it can still open
# $HOME/settings.json into <report> ("ok" or "unreadable"), and then stays
# alive for the rest of the suite: the trap reaps a LIVE pid, never one that
# has already exited and that the host may have reused. The final wait is an
# `exec`, so the tracked pid IS the process the trap kills and no child is
# left behind. Its pid is left in LIVE_STUB_PID.
#
# The pid is NOT returned on stdout: a background job started inside a `$(...)`
# inherits the capture pipe, which would block the capture until the job exits.
LIVE_STUB_PID=""
live_stub() {
    HOME="$1" bash -c '
        trigger="$1" report="$2"
        while [[ ! -e "$trigger" ]]; do sleep 0.1; done
        if grep -q "\"modelProvider\": \"gemini\"" "$HOME/settings.json" >/dev/null 2>&1; then
            printf "ok\n" >"$report"
        else
            printf "unreadable\n" >"$report"
        fi
        exec sleep 300
    ' _ "$2" "$3" >/dev/null 2>&1 &
    LIVE_STUB_PID=$!
    bg_proc_track "$LIVE_STUB_PID"
}

# wait_for_reports <file>... — bounded wait (20s) until every file is non-empty.
wait_for_reports() {
    local i f
    for i in $(seq 1 200); do
        for f in "$@"; do
            [[ -s "$f" ]] || { sleep 0.1; continue 2; }
        done
        return 0
    done
    return 1
}

# report_of <file> — a stub's probe result, or "no-report" if it never answered.
report_of() {
    if [[ -s "$1" ]]; then cat "$1"; else echo "no-report"; fi
}

echo "Testing spawn-agy.sh syntax and permissions..."
TESTS_RUN=$((TESTS_RUN + 1))
if bash -n "$SCRIPTS_DIR/spawn-agy.sh" 2>/dev/null; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: spawn-agy.sh passes bash -n"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: spawn-agy.sh fails bash -n"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if [[ -x "$SCRIPTS_DIR/spawn-agy.sh" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: spawn-agy.sh is executable"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: spawn-agy.sh must be executable"
fi

# A fake repo with the adapter chain installed at `.loom/scripts/` — the layout
# of a real consumer install, the same fixture shape as
# test-spawn-generic-launch.sh (#8700): spawn-generic-launch.sh resolves the
# manifest root from its OWN location, so the installed copy and the manifests
# must be one tree.
FAKE_ROOT="$TMPROOT/fakerepo"
mkdir -p "$FAKE_ROOT/.loom/runtimes" "$FAKE_ROOT/.loom/scripts"
git init -q "$FAKE_ROOT" >/dev/null 2>&1 || {
    echo "FATAL: could not git init the test fixture at $FAKE_ROOT" >&2
    exit 1
}
cp "$SCRIPTS_DIR/spawn-agy.sh" "$SCRIPTS_DIR/spawn-generic-launch.sh" \
    "$SCRIPTS_DIR/spawn-generic.sh" "$FAKE_ROOT/.loom/scripts/"
cp -R "$SCRIPTS_DIR/lib" "$FAKE_ROOT/.loom/scripts/lib"
chmod +x "$FAKE_ROOT/.loom/scripts/spawn-agy.sh" \
    "$FAKE_ROOT/.loom/scripts/spawn-generic-launch.sh" "$FAKE_ROOT/.loom/scripts/spawn-generic.sh"
# The shipped manifest when this suite runs from the source tree; an installed
# copy has none beside it and relies on the daemon's compiled-in fallback.
AGY_MANIFEST="$SCRIPTS_DIR/../../runtimes/agy.json"
if [[ -f "$AGY_MANIFEST" ]]; then
    cp "$AGY_MANIFEST" "$FAKE_ROOT/.loom/runtimes/agy.json"
fi
# Everything below drives the INSTALLED copy; the syntax/permission checks
# above deliberately kept asserting against the tracked source file.
SPAWN_AGY="$FAKE_ROOT/.loom/scripts/spawn-agy.sh"

# The stub CLI: reports its HOME and argv, whether the `--settings` file it was
# handed is the gemini settings file, and whether the HOME's pid marker names a
# live process at the moment the CLI is running.
MOCK_BIN="$TMPROOT/mock-bin"
mkdir -p "$MOCK_BIN"
cat >"$MOCK_BIN/agy" <<'MOCK'
#!/usr/bin/env bash
echo "stub-agy: argv=$*"
echo "stub-agy: home=$HOME"
settings=""
want_settings=0
for arg in "$@"; do
    if [[ "$want_settings" -eq 1 ]]; then
        settings="$arg"
        want_settings=0
    fi
    [[ "$arg" == "--settings" ]] && want_settings=1
done
if [[ -n "$settings" && -r "$settings" ]] && \
    grep -q '"modelProvider": "gemini"' "$settings" 2>/dev/null; then
    echo "stub-agy: settings=ok"
else
    echo "stub-agy: settings=MISSING"
fi
marker="$HOME/.loom-pid"
if [[ -r "$marker" ]] && kill -0 "$(cat "$marker" 2>/dev/null)" 2>/dev/null; then
    echo "stub-agy: marker=live"
else
    echo "stub-agy: marker=absent"
fi
MOCK
chmod +x "$MOCK_BIN/agy"

# launch <tmpdir> [path-prepend] — run the INSTALLED spawn-agy.sh against the
# stub agy with every per-launch HOME under <tmpdir>. Prints the launch's
# combined output and exits with the launch's status. LOOM_SWEEP_NICE defaults
# to 0 here, keeping the chain free of the `nice` re-exec as in
# test-spawn-generic-launch.sh; set AGY_TEST_SWEEP_NICE=1 to take that re-exec
# the way a production launch does.
launch() {
    local path="$MOCK_BIN:$PATH"
    if [[ -n "${2:-}" ]]; then
        path="$2:$path"
    fi
    (cd "$FAKE_ROOT" && env -u LOOM_GENERIC_EXTRA_ARGS -u LOOM_GENERIC_CLI_BIN \
        -u LOOM_GENERIC_PROMPT_FLAG PATH="$path" TMPDIR="$1" \
        LOOM_SWEEP_NICE="${AGY_TEST_SWEEP_NICE:-0}" bash "$SPAWN_AGY" -p "hi" 2>&1)
}

echo ""
echo "Testing a launch creates one isolated HOME whose pid marker lives and dies with the launch..."
HOMES="$TMPROOT/homes"
mkdir -p "$HOMES"
set +e
out="$(launch "$HOMES")"
rc=$?
set -e
assert_eq "0" "$rc" "the launch reaches the stub agy and exits with its status"
assert_contains "$out" "stub-agy: settings=ok" \
    "the stub agy is handed a readable gemini settings file out of its HOME"
assert_contains "$out" "--sandbox" "--sandbox is on the stub agy's argv"
assert_contains "$out" "stub-agy: home=$HOMES/loom-agy-" \
    "the CLI's HOME is a loom-agy-* directory under TMPDIR"
assert_contains "$out" "stub-agy: marker=live" \
    "the HOME's .loom-pid names a pid that is alive while the CLI is running"
assert_eq "1" "$(count_homes "$HOMES")" "exactly one loom-agy-* HOME is created per launch"
HOME_DIR="$(echo "$HOMES"/loom-agy-*)"
assert_eq "700" "$(mode_of "$HOME_DIR")" "the HOME is owner-only"
assert_eq "600" "$(mode_of "$HOME_DIR/.loom-pid")" "the pid marker is owner-only"
MARKER_PID="$(cat "$HOME_DIR/.loom-pid" 2>/dev/null || true)"
if [[ "$MARKER_PID" =~ ^[1-9][0-9]*$ ]]; then
    pass "the pid marker holds one positive integer"
else
    fail "the pid marker holds one positive integer (got: '${MARKER_PID:-<none>}')"
fi
set +e
if [[ "$MARKER_PID" =~ ^[1-9][0-9]*$ ]]; then
    kill -0 "$MARKER_PID" 2>/dev/null
    marker_alive=$?
else
    marker_alive="n/a (no valid marker)"
fi
set -e
assert_eq "1" "$marker_alive" "the marker pid is gone once the launch has exited"

echo ""
echo "Testing the default scheduling re-exec keeps the marker's pid..."
# A production launch leaves LOOM_SWEEP_NICE unset, so spawn-generic.sh re-execs
# itself through `nice`. The marker is only trustworthy if that hop keeps the
# pid it names, so this launch takes it.
NICE_HOMES="$TMPROOT/homes-niced"
mkdir -p "$NICE_HOMES"
set +e
out="$(AGY_TEST_SWEEP_NICE=1 launch "$NICE_HOMES")"
rc=$?
set -e
assert_eq "0" "$rc" "a launch through the nice re-exec reaches the stub agy and exits 0"
assert_contains "$out" "stub-agy: settings=ok" "the re-exec'd launch still hands over a readable settings file"
assert_contains "$out" "stub-agy: marker=live" \
    "the HOME's .loom-pid still names a live pid while the CLI runs behind the nice re-exec"
NICE_PID="$(cat "$(echo "$NICE_HOMES"/loom-agy-*)/.loom-pid" 2>/dev/null || true)"
set +e
if [[ "$NICE_PID" =~ ^[1-9][0-9]*$ ]]; then
    kill -0 "$NICE_PID" 2>/dev/null
    nice_alive=$?
else
    nice_alive="n/a (no valid marker)"
fi
set -e
assert_eq "1" "$nice_alive" "the re-exec'd launch's marker pid is gone once the launch has exited"

echo ""
echo "Testing the sweep removes only a dead launch's HOME and keeps every unknown-liveness one..."
# #15109. The rule under test: remove a same-owner loom-agy-* directory only
# when it is older than the cutoff AND its valid .loom-pid names a process
# confirmed gone. Anything whose liveness is unknown is kept — above all the
# HOME of a worker launched before the marker existed.
SWEEP="$TMPROOT/sweep"
LIVE="$TMPROOT/live"
mkdir -p "$SWEEP" "$LIVE"
TRIGGER="$LIVE/trigger"
STALE_SECONDS=172800 # 2 days: past the adapter's 1440-minute cutoff

DEAD_PID="$(dead_pid)"
LEGACY="$SWEEP/loom-agy-legacy"       # live, written before the marker existed
MARKED="$SWEEP/loom-agy-marked"       # live, marker names a running process
DEAD="$SWEEP/loom-agy-dead"           # the launch is over
ABANDONED="$SWEEP/loom-agy-nomarker"  # no marker, no process: liveness unknown
MALFORMED="$SWEEP/loom-agy-malformed"
EMPTY="$SWEEP/loom-agy-empty"
ZERO="$SWEEP/loom-agy-zero"
FRESH="$SWEEP/loom-agy-fresh"         # dead, but inside the cutoff

make_home "$LEGACY"
make_home "$MARKED"
make_home "$DEAD" "$DEAD_PID"
make_home "$ABANDONED"
make_home "$MALFORMED" "not-a-pid"
make_home "$EMPTY" -
make_home "$ZERO" "0"
make_home "$FRESH" "$DEAD_PID"

live_stub "$LEGACY" "$TRIGGER" "$LIVE/report-legacy"
STUB_LEGACY="$LIVE_STUB_PID"
live_stub "$MARKED" "$TRIGGER" "$LIVE/report-marked"
STUB_MARKED="$LIVE_STUB_PID"
printf '%s\n' "$STUB_MARKED" >"$MARKED/.loom-pid"

# A mode-000 marker is unreadable only for a non-root user.
UNREADABLE=""
if [[ "$EUID" -ne 0 ]]; then
    UNREADABLE="$SWEEP/loom-agy-unreadable"
    make_home "$UNREADABLE" "$DEAD_PID"
    chmod 000 "$UNREADABLE/.loom-pid"
else
    loom_test_skip "unreadable-marker case needs a non-root user (root reads a mode-000 marker; EUID=0)"
fi

# Creating a directory owned by ANOTHER user needs root. Never a silent pass:
# a host that cannot build the fixture says so and the skip is counted.
FOREIGN=""
if [[ "$EUID" -eq 0 ]] && command -v chown >/dev/null 2>&1; then
    FOREIGN="$SWEEP/loom-agy-foreign"
    make_home "$FOREIGN" "$DEAD_PID"
    chown 54321 "$FOREIGN"
else
    loom_test_skip "foreign-owner case needs root to create another user's directory (EUID=$EUID)"
fi

# Everything but FRESH is backdated AFTER its contents are written, since each
# write would refresh the directory's mtime.
for d in "$LEGACY" "$MARKED" "$DEAD" "$ABANDONED" "$MALFORMED" "$EMPTY" "$ZERO" \
    ${UNREADABLE:+"$UNREADABLE"} ${FOREIGN:+"$FOREIGN"}; do
    backdate "$d" "$STALE_SECONDS"
done
set +e
find "$SWEEP" -mindepth 1 -maxdepth 1 -name 'loom-agy-dead' -mmin +1440 | grep -q .
fixture_stale=$?
set -e
assert_eq "0" "$fixture_stale" "fixture precondition: the backdated HOME is past the cutoff"

set +e
out="$(launch "$SWEEP")"
rc=$?
set -e
assert_eq "0" "$rc" "the launch succeeds with stale HOMEs beside it"
assert_contains "$out" "stub-agy: settings=ok" "the launch's own new HOME is intact"

if [[ -e "$DEAD" ]]; then
    fail "a backdated HOME whose marker names a dead pid is removed"
else
    pass "a backdated HOME whose marker names a dead pid is removed"
fi

kept() { # <dir> <label>
    if [[ -d "$1" && -r "$1/settings.json" ]]; then
        pass "$2 is kept"
    else
        fail "$2 is kept"
    fi
}
kept "$LEGACY" "a live legacy HOME with no marker"
kept "$MARKED" "a live HOME whose marker names a running process"
kept "$ABANDONED" "a marker-less abandoned HOME (liveness unknown)"
kept "$MALFORMED" "a HOME with a malformed marker"
kept "$EMPTY" "a HOME with an empty marker"
kept "$ZERO" "a HOME whose marker is 0"
kept "$FRESH" "a fresh dead-pid HOME (inside the cutoff)"
if [[ -n "$UNREADABLE" ]]; then
    kept "$UNREADABLE" "a HOME with an unreadable marker"
fi
if [[ -n "$FOREIGN" ]]; then
    kept "$FOREIGN" "a HOME owned by another user"
fi

set +e
kill -0 "$STUB_LEGACY" 2>/dev/null
legacy_alive=$?
kill -0 "$STUB_MARKED" 2>/dev/null
marked_alive=$?
set -e
assert_eq "0" "$legacy_alive" "the legacy stub is still running after the sweep"
assert_eq "0" "$marked_alive" "the marked stub is still running after the sweep"

echo ""
echo "Testing the surviving stubs can still open their settings.json after the sweep..."
: >"$TRIGGER"
if wait_for_reports "$LIVE/report-legacy" "$LIVE/report-marked"; then
    pass "both live stubs answered the settings probe"
else
    fail "both live stubs answered the settings probe (timed out)"
fi
assert_eq "ok" "$(report_of "$LIVE/report-legacy")" \
    "the still-running legacy stub can open its settings.json after the sweep"
assert_eq "ok" "$(report_of "$LIVE/report-marked")" \
    "the still-running marked stub can open its settings.json after the sweep"

echo ""
echo "Testing a failing sweep neither fails nor changes the launch..."
# `find` shadowed by a stub that fails loudly. The launch must reach the stub
# agy and produce exactly what it produces without the shadow.
SHADOW="$TMPROOT/shadow-bin"
mkdir -p "$SHADOW"
cat >"$SHADOW/find" <<'STUB'
#!/usr/bin/env bash
echo "find-stub: simulated failure" >&2
echo "garbage without a NUL terminator"
exit 1
STUB
chmod +x "$SHADOW/find"
PLAIN="$TMPROOT/plain"
SHADOWED="$TMPROOT/shadowed"
mkdir -p "$PLAIN" "$SHADOWED"
# The raw output goes to a file first (`$(...)` would report the normaliser's
# status, not the launch's). Only the stub lines are compared: they are
# identical between two launches once the random mktemp suffix and this
# test's own scratch-directory names are erased, whereas the adapter's own log
# lines carry a timestamp.
norm() { grep '^stub-agy:' | sed -E 's#loom-agy-[A-Za-z0-9]{6}#loom-agy-XXXXXX#g; s#/(plain|shadowed)/#/#'; }
set +e
launch "$PLAIN" >"$LIVE/control.raw"
control_rc=$?
launch "$SHADOWED" "$SHADOW" >"$LIVE/shadow.raw"
shadow_rc=$?
set -e
control_out="$(norm <"$LIVE/control.raw")"
shadow_out="$(norm <"$LIVE/shadow.raw")"
shadow_raw="$(cat "$LIVE/shadow.raw")"
assert_eq "0" "$shadow_rc" "the launch exits 0 with a failing find on PATH"
assert_eq "$control_rc" "$shadow_rc" "the launch exits as it does without the shadowed find"
assert_eq "$control_out" "$shadow_out" "the stub agy sees the same argv, HOME and marker as in the unshadowed run"
assert_contains "$shadow_out" "stub-agy: settings=ok" "the launch still reaches the stub agy"
assert_not_contains "$shadow_raw" "find-stub" "the sweep's own failure text never reaches the launch output"
assert_eq "1" "$(count_homes "$SHADOWED")" "the launch still gets its own fresh HOME"

echo ""
echo "Testing repeated launches leave no abandoned HOME past the cutoff..."
ACC="$TMPROOT/accumulate"
mkdir -p "$ACC"
for i in 1 2 3; do
    set +e
    launch "$ACC" >/dev/null
    step_rc=$?
    set -e
    assert_eq "0" "$step_rc" "launch $i succeeds"
done
assert_eq "3" "$(count_homes "$ACC")" "three fresh HOMEs are all kept (none is past the cutoff yet)"
for d in "$ACC"/loom-agy-*; do
    backdate "$d" "$STALE_SECONDS"
done
set +e
launch "$ACC" >/dev/null
rc=$?
set -e
assert_eq "0" "$rc" "the launch after three abandoned HOMEs succeeds"
assert_eq "1" "$(count_homes "$ACC")" \
    "only the newest launch's HOME remains: all three backdated dead-pid HOMEs were reclaimed"

echo ""
echo "Testing the adapter docs no longer call agy poolless..."
DOC="$SCRIPTS_DIR/../docs/runtime-adapters.md"
assert_doc_contains "$DOC" 'composes the binary gate with the' \
    "runtime-adapters.md says agy's tap composes the binary gate with a pool check"
assert_doc_lacks "$DOC" 'operator-defined) has no credential pool, so' \
    "runtime-adapters.md no longer states that every tier-3 runtime has no credential pool"
# The table runs from its heading to the next same-level heading (a blank line
# separates the heading from the rows, so a blank-line range would be empty).
ADAPTER_TABLE="$(sed -n '/^### Adapter status/,/^### Tier 3/p' "$DOC")"
assert_contains "$ADAPTER_TABLE" 'defaults/scripts/spawn-agy.sh' \
    "the Adapter status table has an agy row"

echo ""
echo "==================================="
echo "Tests run: $TESTS_RUN, Passed: $TESTS_PASSED, Failed: $TESTS_FAILED, Skipped: ${TESTS_SKIPPED:-0}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo "Some tests failed."
    exit 1
fi
echo "All tests passed."
