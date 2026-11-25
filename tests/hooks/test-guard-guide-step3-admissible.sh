#!/usr/bin/env bash
# test-guard-guide-step3-admissible.sh — Guide Step 3 / guard drift test
# (issue #15219).
#
# The Guide role prompt's Step 3 code block is pasted into a Bash tool call
# VERBATIM, so every file write its functions perform inline is judged by the
# default destructive-command guard BEFORE anything executes. Up to #15219 the
# block carried four such shapes (an `rm -f` on the variable-built receipt
# path, a `mktemp -d TEMPLATE` scratch plus `rm -rf "$scratch"`, a `>`
# redirect onto `$DOCS_WT/...`, and `cat ... > "$plan"`), each alone enough
# for a deny — which left the #13923 provenance gate unreachable from a
# guarded shell. The fix moved the writes behind `loom-daemon guide
# work-plan-receipt|work-plan-splice`; this suite feeds the verbatim Step 3
# fenced blocks to BOTH the vendored generic guard and the runtime dispatcher
# and asserts allow, so the role file and the guard cannot drift apart again.
#
# Negative controls keep the test honest: a known-denied scratch idiom must
# still deny through the same paths, and the extraction itself must be proved
# non-empty and on-target (an extraction regression that yielded empty
# commands would otherwise pass vacuously).
#
# Usage: ./tests/hooks/test-guard-guide-step3-admissible.sh

set -euo pipefail
# shellcheck source=tests/hooks/lib/guard-destructive-harness.sh
. "$(cd "$(dirname "$0")" && pwd)/lib/guard-destructive-harness.sh"

# The dispatcher is the second judged surface: guard-destructive.sh resolves
# the canonical Repo Skills guard when present and defers to the vendored
# generic otherwise, so feeding it exercises the exact runtime path a Bash
# tool call takes in a repo without Repo Skills.
DISPATCHER="$REPO_ROOT/defaults/hooks/guard-destructive.sh"

GUIDE_SOURCES=(
    "$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
    "$REPO_ROOT/defaults/roles/guide.md"
)

# Extract every ```bash fenced block of the Step 3 section into numbered
# files, and print the count. Non-bash fences inside the section (the
# ```markdown WORK_PLAN template) are skipped by the ^```bash opener.
extract_step3_blocks() {
    local file="$1" outdir="$2"
    awk -v dir="$outdir" '
        /^### Step 3: Update WORK_PLAN\.md$/ {in3=1}
        /^### Step 4: Check README\.md Staleness$/ {in3=0}
        in3 && /^```bash[[:space:]]*$/ {n++; inblock=1; next}
        in3 && /^```[[:space:]]*$/ {inblock=0; next}
        inblock { print > (dir "/block" n ".txt") }
    ' "$file"
    local f count=0
    for f in "$outdir"/block*.txt; do
        [[ -f "$f" ]] && count=$((count + 1))
    done
    echo "$count"
}

run_dispatcher() {
    local cmd="$1" cwd="${2:-$TEST_REPO}"
    make_input "$cmd" "$cwd" | bash "$DISPATCHER" 2>&1
}

assert_dispatcher_allow() {
    local description="$1" cmd="$2" cwd="${3:-$TEST_REPO}"
    TOTAL=$((TOTAL + 1))
    local output exit_code=0
    output=$(run_dispatcher "$cmd" "$cwd") || exit_code=$?
    if [[ $exit_code -eq 0 ]] && \
       ! echo "$output" | jq -e '.hookSpecificOutput.permissionDecision' >/dev/null 2>&1; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: $description"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: $description"
        echo -e "       Command: <Step 3 block, $(wc -c <<<"$cmd" | tr -d ' ') bytes>"
        echo -e "       Expected: dispatcher allow (exit 0, no decision)"
        echo -e "       Exit code: $exit_code"
        echo -e "       Got: $output"
    fi
}

for GUIDE_MD in "${GUIDE_SOURCES[@]}"; do
    if [[ ! -f "$GUIDE_MD" ]]; then
        echo "SKIP: $GUIDE_MD not present in this layout" >&2
        continue
    fi

    BLOCK_DIR=$(mktemp -d 2>/dev/null || mktemp -d -t loom-step3-blocks)
    COUNT="$(extract_step3_blocks "$GUIDE_MD" "$BLOCK_DIR")"

    TOTAL=$((TOTAL + 1))
    if [[ "$COUNT" -ge 2 && -s "$BLOCK_DIR/block1.txt" ]]; then
        PASS=$((PASS + 1))
        echo -e "  ${GREEN}PASS${NC}: ${GUIDE_MD##*defaults/} Step 3 has $COUNT bash blocks, extraction non-empty"
    else
        FAIL=$((FAIL + 1))
        echo -e "  ${RED}FAIL${NC}: extraction found only '$COUNT' blocks in ${GUIDE_MD##*defaults/} — drift test cannot run"
    fi

    # The extraction must be on target: one block defines the receipt writer,
    # one defines update_work_plan(), one defines splice_plan_body().
    ALL_BLOCKS="$(cat "$BLOCK_DIR"/block*.txt)"
    for needle in 'record_work_plan_receipt() {' 'update_work_plan() {' 'splice_plan_body() {'; do
        TOTAL=$((TOTAL + 1))
        if grep -qF -- "$needle" <<<"$ALL_BLOCKS"; then
            PASS=$((PASS + 1))
            echo -e "  ${GREEN}PASS${NC}: ${GUIDE_MD##*defaults/} Step 3 blocks define $needle"
        else
            FAIL=$((FAIL + 1))
            echo -e "  ${RED}FAIL${NC}: ${GUIDE_MD##*defaults/} Step 3 blocks no longer define $needle (extraction target moved?)"
        fi
    done

    # THE drift assertion: each verbatim block, alone, must be admissible
    # under the default policy through BOTH judged surfaces.
    for block in "$BLOCK_DIR"/block*.txt; do
        CMD="$(cat "$block")"
        assert_allow "generic guard allows verbatim Step 3 block ${block##*/} of ${GUIDE_MD##*defaults/} (default rmScope)" \
            "$CMD" "$TEST_REPO"
        assert_dispatcher_allow "dispatcher allows verbatim Step 3 block ${block##*/} of ${GUIDE_MD##*defaults/}" \
            "$CMD" "$TEST_REPO"
    done

    # And so does the whole section concatenated — the shape a paste-happy
    # agent actually submits.
    assert_allow "generic guard allows the concatenated Step 3 blocks of ${GUIDE_MD##*defaults/}" \
        "$ALL_BLOCKS" "$TEST_REPO"
    assert_dispatcher_allow "dispatcher allows the concatenated Step 3 blocks of ${GUIDE_MD##*defaults/}" \
        "$ALL_BLOCKS" "$TEST_REPO"

    rm -rf "$BLOCK_DIR"
done

# ---------------------------------------------------------------------------
# Negative controls — the same surfaces must still deny the shapes the fix
# removed from the block, so this suite can actually fail.
# ---------------------------------------------------------------------------
echo ""
echo -e "${YELLOW}--- Negative controls (the removed shapes still deny) ---${NC}"
assert_deny "mktemp-template scratch + rm -rf still denied (issue row 7)" \
    'd="$(mktemp -d "${TMPDIR:-/tmp}/guide-plan.XXXXXX")"; rm -rf "$d"'
assert_deny "rm on a variable-built two-name target still denied (issue row 1)" \
    'DOCS_WT=/tmp/x; F=name; rm -f "$DOCS_WT/$F"'

print_summary
