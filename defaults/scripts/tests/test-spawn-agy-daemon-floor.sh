#!/usr/bin/env bash
# test-spawn-agy-daemon-floor.sh — Mixed-version and fail-closed tests for
# spawn-agy.sh's mandatory-pool daemon version floor (issue #15532).
#
# `0918138e9` (`runtime_launch::pooled_tier3`, first released in v0.20.41) is
# where `loom-daemon spawn-worker` enforces `agy`'s mandatory `credentialPool`.
# An older selected worker binary skips the pool draw and lets an ambient
# GEMINI_API_KEY reach `agy`. `spawn-worker.sh` hands its selected executable
# path to the child via LOOM_SPAWN_WORKER_DAEMON_BIN, and `spawn-agy.sh` checks
# ONLY that executable against >= 0.20.41 before creating the isolated HOME or
# invoking the CLI.
#
# Hermetic: controlled bash fixtures for pins, PATH, and the fake `agy` CLI;
# synthetic dummy strings only; no live model or forge calls.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"

# shellcheck source=lib/assert.sh
source "$SCRIPT_DIR/lib/assert.sh"

TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT

DUMMY_AMBIENT_KEY="dummy-ambient-gemini-key-15532-never-log"
DUMMY_POOL_KEY="dummy-pooled-gemini-key-15532-never-log"

echo "Testing spawn-worker.sh and spawn-agy.sh syntax and permissions..."
for target in "$SCRIPTS_DIR/spawn-worker.sh" "$SCRIPTS_DIR/spawn-agy.sh"; do
    name="$(basename "$target")"
    if bash -n "$target" 2>/dev/null; then
        pass "$name passes bash -n"
    else
        fail "$name fails bash -n"
    fi
    if [[ -x "$target" ]]; then
        pass "$name is executable"
    else
        fail "$name must be executable"
    fi
done

# Build an isolated consumer-layout workspace holding the shipped scripts.
FAKE_ROOT="$TMPROOT/ws"
mkdir -p "$FAKE_ROOT/.loom/scripts" "$TMPROOT/empty-install" "$TMPROOT/empty-target"
git init -q "$FAKE_ROOT" >/dev/null 2>&1
for f in spawn-worker.sh spawn-agy.sh spawn-aider.sh spawn-generic-launch.sh spawn-generic.sh; do
    cp "$SCRIPTS_DIR/$f" "$FAKE_ROOT/.loom/scripts/$f"
    chmod +x "$FAKE_ROOT/.loom/scripts/$f"
done
cp -R "$SCRIPTS_DIR/lib" "$FAKE_ROOT/.loom/scripts/lib"

SHIPPED_WORKER="$FAKE_ROOT/.loom/scripts/spawn-worker.sh"
SHIPPED_AGY="$FAKE_ROOT/.loom/scripts/spawn-agy.sh"

# Fake CLI bin directory (agy + aider shims).
CLI_BIN_DIR="$TMPROOT/cli-bin"
mkdir -p "$CLI_BIN_DIR"
cat >"$CLI_BIN_DIR/agy" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
: "${AGY_SHIM_SENTINEL:?}"
{
    printf 'INVOKED=1\n'
    printf 'HOME=%s\n' "${HOME:-}"
    printf 'HANDOFF=%s\n' "${LOOM_SPAWN_WORKER_DAEMON_BIN-<unset>}"
    if [[ -n "${GEMINI_API_KEY:-}" ]]; then
        if [[ "${GEMINI_API_KEY}" == "${TEST_EXPECTED_POOL_KEY:-}" ]]; then
            printf 'KEY_KIND=pool\n'
        elif [[ "${GEMINI_API_KEY}" == "${TEST_EXPECTED_AMBIENT_KEY:-}" ]]; then
            printf 'KEY_KIND=ambient\n'
        else
            printf 'KEY_KIND=other\n'
        fi
    else
        printf 'KEY_KIND=unset\n'
    fi
} >"$AGY_SHIM_SENTINEL"
EOF
chmod +x "$CLI_BIN_DIR/agy"

cat >"$CLI_BIN_DIR/aider" <<'EOF'
#!/usr/bin/env bash
set -euo pipefail
: "${AIDER_SHIM_SENTINEL:?}"
printf 'AIDER_INVOKED=1\nARGS=%s\n' "$*" >"$AIDER_SHIM_SENTINEL"
EOF
chmod +x "$CLI_BIN_DIR/aider"

# make_fake_daemon <dest> <version_line> <version_rc> <pool_behavior>
#   pool_behavior:
#     skip   — pre-0.20.41 worker: forwards straight to spawn-<rt>.sh with
#              ambient env untouched (no pool draw).
#     inject — >=0.20.41 worker: replaces GEMINI_API_KEY with TEST_EXPECTED_POOL_KEY
#              before exec'ing spawn-agy.sh.
make_fake_daemon() {
    local dest="$1" ver_line="$2" ver_rc="$3" pool_mode="$4"
    mkdir -p "$(dirname "$dest")"
    cat >"$dest" <<EOF
#!/usr/bin/env bash
set -euo pipefail
if [[ "\${1:-}" == "--version" ]]; then
    if [[ -n "\${VERSION_PROBE_LOG:-}" ]]; then
        printf '%s\n' "\$0" >>"\$VERSION_PROBE_LOG"
    fi
    if [[ -n $(printf '%q' "$ver_line") ]]; then
        printf '%s\n' $(printf '%q' "$ver_line")
    fi
    exit $ver_rc
fi
if [[ "\${1:-}" == "runtime-launch-env" ]]; then
    rt=""
    while [[ \$# -gt 0 ]]; do
        case "\$1" in
            --runtime) rt="\$2"; shift 2 ;;
            *) shift ;;
        esac
    done
    case "\$rt" in
        agy)
            printf '[ -n "\${LOOM_GENERIC_CLI_BIN:-}" ] || LOOM_GENERIC_CLI_BIN="agy"; export LOOM_GENERIC_CLI_BIN\n'
            printf '[ -n "\${LOOM_GENERIC_PROMPT_FLAG:-}" ] || LOOM_GENERIC_PROMPT_FLAG="-p"; export LOOM_GENERIC_PROMPT_FLAG\n'
            printf '[ -n "\${LOOM_GENERIC_MODEL_FLAG:-}" ] || LOOM_GENERIC_MODEL_FLAG="--model"; export LOOM_GENERIC_MODEL_FLAG\n'
            exit 0
            ;;
        aider)
            printf '[ -n "\${LOOM_GENERIC_CLI_BIN:-}" ] || LOOM_GENERIC_CLI_BIN="aider"; export LOOM_GENERIC_CLI_BIN\n'
            printf '[ -n "\${LOOM_GENERIC_PROMPT_FLAG:-}" ] || LOOM_GENERIC_PROMPT_FLAG="--message"; export LOOM_GENERIC_PROMPT_FLAG\n'
            exit 0
            ;;
        *)
            exit 1
            ;;
    esac
fi
if [[ "\${1:-}" == "spawn-worker" ]]; then
    shift
    scripts_dir=""
    worker_args=()
    while [[ \$# -gt 0 ]]; do
        case "\$1" in
            --scripts-dir) scripts_dir="\$2"; shift 2 ;;
            --) shift; worker_args=("\$@"); break ;;
            *) worker_args+=("\$1"); shift ;;
        esac
    done
    if [[ -z "\$scripts_dir" ]]; then
        scripts_dir="\${LOOM_WORKSPACE:-.}/.loom/scripts"
    fi
    rt="\${LOOM_RUNTIME:-}"
    if [[ -z "\$rt" && -n "\${LOOM_ROLE:-}" && -n "\${FAKE_ROLE_RUNTIME:-}" ]]; then
        rt="\$FAKE_ROLE_RUNTIME"
    fi
    if [[ -z "\$rt" && -n "\${FAKE_CONFIG_DEFAULT_RUNTIME:-}" ]]; then
        rt="\$FAKE_CONFIG_DEFAULT_RUNTIME"
    fi
    rt="\${rt:-claude}"
    if [[ "$pool_mode" == "inject" && "\$rt" == "agy" ]]; then
        export GEMINI_API_KEY="\${TEST_EXPECTED_POOL_KEY:-}"
    fi
    exec "\$scripts_dir/spawn-\$rt.sh" \${worker_args[@]+"\${worker_args[@]}"}
fi
echo "unexpected fake loom-daemon args: \$*" >&2
exit 2
EOF
    chmod +x "$dest"
}

DAEMON_040="$TMPROOT/pins/daemon-0.20.40/loom-daemon"
DAEMON_041="$TMPROOT/pins/daemon-0.20.41/loom-daemon"
DAEMON_043_PIN="$TMPROOT/pins/daemon-0.20.43/loom-daemon"
PATH_043_DIR="$TMPROOT/path-043"
PATH_040_DIR="$TMPROOT/path-040"

make_fake_daemon "$DAEMON_040" "loom-daemon 0.20.40 (commit 1111111, built 2026-11-26T00:00:00Z)" 0 skip
make_fake_daemon "$DAEMON_041" "loom-daemon 0.20.41 (commit 0918138, built 2026-11-26T12:00:00Z)" 0 inject
make_fake_daemon "$DAEMON_043_PIN" "loom-daemon 0.20.43 (commit 7c6a797, built 2026-11-27T00:00:00Z)" 0 inject
make_fake_daemon "$PATH_043_DIR/loom-daemon" "loom-daemon 0.20.43 (commit 7c6a797, built 2026-11-27T00:00:00Z)" 0 inject
make_fake_daemon "$PATH_040_DIR/loom-daemon" "loom-daemon 0.20.40 (commit 1111111, built 2026-11-26T00:00:00Z)" 0 skip

RUN_SEQ=0
count_agy_homes() {
    local dir="$1" n=0 entry
    for entry in "$dir"/loom-agy-*; do
        [[ -e "$entry" ]] && n=$((n + 1))
    done
    printf '%s' "$n"
}

# Run a command in a clean worker-free environment with dedicated TMPDIR,
# CLI sentinel, and --version probe log. Sets LAST_OUT, LAST_RC, LAST_TMP,
# LAST_SENTINEL, LAST_PROBES.
run_isolated() {
    local path_prefix="$1"
    shift
    RUN_SEQ=$((RUN_SEQ + 1))
    LAST_TMP="$TMPROOT/run-$RUN_SEQ"
    LAST_SENTINEL="$LAST_TMP/agy-sentinel.txt"
    LAST_PROBES="$LAST_TMP/version-probes.txt"
    mkdir -p "$LAST_TMP"
    : >"$LAST_PROBES"
    set +e
    LAST_OUT="$(
        cd "$FAKE_ROOT" && env \
            -u LOOM_DAEMON_SELF_BIN \
            -u LOOM_DAEMON_BIN \
            -u LOOM_SPAWN_WORKER_DAEMON_BIN \
            -u LOOM_PREFER_REPO_BUILD \
            -u LOOM_ROLE \
            -u LOOM_RUNTIME \
            -u LOOM_MODEL \
            -u LOOM_GENERIC_EXTRA_ARGS \
            -u LOOM_SKIP_DAEMON_VERSION_PREFLIGHT \
            -u LOOM_SKIP_SELF_BIN_SANITY_PROBE \
            TMPDIR="$LAST_TMP" \
            LOOM_WORKSPACE="$FAKE_ROOT" \
            LOOM_DAEMON_BIN_DIR="$TMPROOT/empty-install" \
            CARGO_TARGET_DIR="$TMPROOT/empty-target" \
            LOOM_SWEEP_NICE=0 \
            AGY_SHIM_SENTINEL="$LAST_SENTINEL" \
            VERSION_PROBE_LOG="$LAST_PROBES" \
            TEST_EXPECTED_AMBIENT_KEY="$DUMMY_AMBIENT_KEY" \
            TEST_EXPECTED_POOL_KEY="$DUMMY_POOL_KEY" \
            PATH="$path_prefix:$CLI_BIN_DIR:$PATH" \
            "$@" 2>&1
    )"
    LAST_RC=$?
    set -e
}

assert_refused_cleanly() {
    local label="$1"
    assert_eq "78" "$LAST_RC" "$label: exits 78"
    assert_contains "$LAST_OUT" "0.20.41" "$label: names the 0.20.41 floor"
    assert_contains "$LAST_OUT" "REMEDIATION" "$label: includes remediation"
    assert_not_contains "$LAST_OUT" "$DUMMY_AMBIENT_KEY" "$label: never leaks ambient dummy key"
    assert_not_contains "$LAST_OUT" "$DUMMY_POOL_KEY" "$label: never leaks pool dummy key"
    if [[ ! -e "$LAST_SENTINEL" ]]; then
        pass "$label: agy CLI shim was not invoked"
    else
        fail "$label: agy CLI shim ran unexpectedly ($(tr '\n' ' ' <"$LAST_SENTINEL"))"
    fi
    assert_eq "0" "$(count_agy_homes "$LAST_TMP")" "$label: no isolated loom-agy-* HOME created"
}

echo ""
echo "AC1: LOOM_DAEMON_BIN=0.20.40 with 0.20.43 on PATH and ambient GEMINI_API_KEY refuses at 78..."
run_isolated "$PATH_043_DIR" env \
    LOOM_RUNTIME=agy \
    LOOM_DAEMON_BIN="$DAEMON_040" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "env probe"
assert_refused_cleanly "AC1 (0.20.40 pinned via LOOM_DAEMON_BIN, 0.20.43 on PATH)"
assert_contains "$LAST_OUT" "0.20.40" "AC1: diagnostic identifies observed version 0.20.40"
assert_eq "$DAEMON_040" "$(cat "$LAST_PROBES")" \
    "AC1: only the selected 0.20.40 executable was asked for --version (never PATH's 0.20.43)"

echo ""
echo "AC1b: spawn-worker.sh overwrites an inherited LOOM_SPAWN_WORKER_DAEMON_BIN and normalizes relative paths..."
REL_040=".loom/rel-bin/loom-daemon"
make_fake_daemon "$FAKE_ROOT/$REL_040" "loom-daemon 0.20.40" 0 skip
run_isolated "$PATH_043_DIR" env \
    LOOM_RUNTIME=agy \
    LOOM_DAEMON_BIN="$REL_040" \
    LOOM_SPAWN_WORKER_DAEMON_BIN="$DAEMON_043_PIN" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "env probe"
assert_refused_cleanly "AC1b (relative LOOM_DAEMON_BIN overwrites inherited 0.20.43 handoff)"
assert_contains "$LAST_OUT" "0.20.40" "AC1b: checks the relative-pinned 0.20.40 worker, not inherited 0.20.43"
assert_eq "$FAKE_ROOT/$REL_040" "$(cat "$LAST_PROBES")" \
    "AC1b: relative LOOM_DAEMON_BIN was normalized to an absolute path before handoff"

echo ""
echo "AC2a: LOOM_DAEMON_SELF_BIN=0.20.40 outranks LOOM_DAEMON_BIN=0.20.43 and refuses at 78..."
run_isolated "$PATH_043_DIR" env \
    LOOM_RUNTIME=agy \
    LOOM_DAEMON_SELF_BIN="$DAEMON_040" \
    LOOM_DAEMON_BIN="$DAEMON_043_PIN" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "env probe"
assert_refused_cleanly "AC2a (LOOM_DAEMON_SELF_BIN=0.20.40 over LOOM_DAEMON_BIN=0.20.43)"
assert_contains "$LAST_OUT" "0.20.40" "AC2a: identifies 0.20.40 from LOOM_DAEMON_SELF_BIN"
assert_eq "$DAEMON_040" "$(cat "$LAST_PROBES")" \
    "AC2a: only LOOM_DAEMON_SELF_BIN was probed for --version"

echo ""
echo "AC2b: Boundary 0.20.41 and 0.20.43 pass the floor even with 0.20.40 elsewhere..."
run_isolated "$PATH_040_DIR" env \
    LOOM_RUNTIME=agy \
    LOOM_DAEMON_BIN="$DAEMON_041" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "boundary ok"
assert_eq "0" "$LAST_RC" "AC2b (0.20.41 boundary): exits 0"
assert_eq "$DAEMON_041" "$(head -n1 "$LAST_PROBES")" \
    "AC2b (0.20.41 boundary): floor check probed the selected 0.20.41 worker"
assert_grep '^INVOKED=1$' "$LAST_SENTINEL" "AC2b (0.20.41 boundary): agy CLI shim ran"
assert_grep '^KEY_KIND=pool$' "$LAST_SENTINEL" "AC2b (0.20.41 boundary): child received pooled key"
assert_grep '^HANDOFF=<unset>$' "$LAST_SENTINEL" "AC2b (0.20.41 boundary): private handoff unset before child exec"
assert_eq "1" "$(count_agy_homes "$LAST_TMP")" "AC2b (0.20.41 boundary): isolated HOME created on allowed launch"

run_isolated "$PATH_040_DIR" env \
    LOOM_RUNTIME=agy \
    LOOM_DAEMON_SELF_BIN="$DAEMON_043_PIN" \
    LOOM_DAEMON_BIN="$DAEMON_040" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "self-pin 0.20.43 ok"
assert_eq "0" "$LAST_RC" "AC2b (LOOM_DAEMON_SELF_BIN=0.20.43 over LOOM_DAEMON_BIN=0.20.40): exits 0"
assert_eq "$DAEMON_043_PIN" "$(head -n1 "$LAST_PROBES")" \
    "AC2b (0.20.43 self-pin): floor check probed LOOM_DAEMON_SELF_BIN"
assert_grep '^INVOKED=1$' "$LAST_SENTINEL" "AC2b (0.20.43 self-pin): agy CLI shim ran"

echo ""
echo "AC2c: Handoff carries through role-selected and config-selected agy dispatches..."
run_isolated "$PATH_043_DIR" env \
    LOOM_ROLE=curator \
    FAKE_ROLE_RUNTIME=agy \
    LOOM_DAEMON_BIN="$DAEMON_040" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "role dispatch"
assert_refused_cleanly "AC2c (role-selected agy with 0.20.40 worker)"

run_isolated "$PATH_043_DIR" env \
    FAKE_CONFIG_DEFAULT_RUNTIME=agy \
    LOOM_DAEMON_BIN="$DAEMON_040" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "config dispatch"
assert_refused_cleanly "AC2c (config-selected agy with 0.20.40 worker)"

echo ""
echo "AC3: Missing identity, missing executable, malformed version, nonzero exit, and skip switches..."
# 1. Bare spawn-agy.sh invocation (missing identity) even with 0.20.43 on PATH + LOOM_DAEMON_BIN
run_isolated "$PATH_043_DIR" env \
    LOOM_DAEMON_BIN="$DAEMON_043_PIN" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_AGY" -p "bare call"
assert_refused_cleanly "AC3 (missing identity / bare spawn-agy.sh)"
assert_contains "$LAST_OUT" "spawn-worker.sh" "AC3 (bare call): remediation names spawn-worker.sh"
assert_eq "" "$(cat "$LAST_PROBES")" "AC3 (bare call): no fallback binary lookup performed"

# 2. Relative handoff path rejected
run_isolated "$PATH_043_DIR" env \
    LOOM_SPAWN_WORKER_DAEMON_BIN="relative/loom-daemon" \
    LOOM_DAEMON_BIN="$DAEMON_043_PIN" \
    bash "$SHIPPED_AGY" -p "relative handoff"
assert_refused_cleanly "AC3 (relative handoff path)"

# 3. Missing executable path
run_isolated "$PATH_043_DIR" env \
    LOOM_SPAWN_WORKER_DAEMON_BIN="$TMPROOT/does-not-exist/loom-daemon" \
    LOOM_DAEMON_BIN="$DAEMON_043_PIN" \
    bash "$SHIPPED_AGY" -p "missing executable"
assert_refused_cleanly "AC3 (missing executable)"

# 4. Directory path (is +x in POSIX, but not a regular file)
run_isolated "$PATH_043_DIR" env \
    LOOM_SPAWN_WORKER_DAEMON_BIN="$TMPROOT" \
    LOOM_DAEMON_BIN="$DAEMON_043_PIN" \
    bash "$SHIPPED_AGY" -p "directory handoff"
assert_refused_cleanly "AC3 (directory instead of executable file)"

# 5. Plausible supported version string with nonzero exit code
DAEMON_FAIL_043="$TMPROOT/pins/daemon-fail-043/loom-daemon"
make_fake_daemon "$DAEMON_FAIL_043" "loom-daemon 0.20.43 (commit 7c6a797, built 2026-11-27T00:00:00Z)" 1 inject
run_isolated "$PATH_043_DIR" env \
    LOOM_RUNTIME=agy \
    LOOM_DAEMON_BIN="$DAEMON_FAIL_043" \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "nonzero version exit"
assert_refused_cleanly "AC3 (plausible 0.20.43 output with nonzero exit)"

# 6. Malformed version outputs (empty, wrong binary name, stray semver in prose, partial semver)
idx=0
for bad_ver in \
    "" \
    "other-daemon 0.20.43" \
    "error: failed after 0.20.43 seconds" \
    "loom-daemon unknown" \
    "loom-daemon 0.20" \
    "loom-daemon v0.20.43"
do
    idx=$((idx + 1))
    bad_bin="$TMPROOT/pins/bad-$idx/loom-daemon"
    make_fake_daemon "$bad_bin" "$bad_ver" 0 inject
    run_isolated "$PATH_043_DIR" env \
        LOOM_RUNTIME=agy \
        LOOM_DAEMON_BIN="$bad_bin" \
        GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
        bash "$SHIPPED_WORKER" -p "malformed $idx"
    assert_refused_cleanly "AC3 (malformed version #$idx: '$bad_ver')"
done

# 7. Advisory preflight skip switches cannot admit an incompatible worker
run_isolated "$PATH_043_DIR" env \
    LOOM_RUNTIME=agy \
    LOOM_DAEMON_BIN="$DAEMON_040" \
    LOOM_SKIP_DAEMON_VERSION_PREFLIGHT=1 \
    LOOM_SKIP_SELF_BIN_SANITY_PROBE=1 \
    GEMINI_API_KEY="$DUMMY_AMBIENT_KEY" \
    bash "$SHIPPED_WORKER" -p "skip switch attempt"
assert_refused_cleanly "AC3 (LOOM_SKIP_DAEMON_VERSION_PREFLIGHT=1 does not bypass agy floor)"
assert_contains "$LAST_OUT" "0.20.40" "AC3 (skip switch): still reports 0.20.40"

echo ""
echo "AC5: Other runtimes retain their existing launch behavior on 0.20.40..."
AIDER_SENTINEL="$TMPROOT/aider-sentinel.txt"
run_isolated "$PATH_040_DIR" env \
    LOOM_RUNTIME=aider \
    LOOM_DAEMON_BIN="$DAEMON_040" \
    AIDER_SHIM_SENTINEL="$AIDER_SENTINEL" \
    bash "$SHIPPED_WORKER" -p "aider unaffected"
assert_eq "0" "$LAST_RC" "AC5: aider through spawn-worker.sh with 0.20.40 worker still exits 0"
assert_grep '^AIDER_INVOKED=1$' "$AIDER_SENTINEL" "AC5: aider shim ran normally"

echo ""
echo "==================================="
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}$TESTS_PASSED${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}$TESTS_FAILED${NC}"
    exit 1
fi
echo "All tests passed."
