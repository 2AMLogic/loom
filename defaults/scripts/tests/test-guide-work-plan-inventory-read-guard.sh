#!/usr/bin/env bash
set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

if [[ -f "$REPO_ROOT/.claude/commands/loom/guide.md" ]]; then
    GUIDE_MD="$REPO_ROOT/.claude/commands/loom/guide.md"
else
    GUIDE_MD="$REPO_ROOT/defaults/.claude/commands/loom/guide.md"
fi
if [[ -f "$REPO_ROOT/.agents/skills/loom-guide/SKILL.md" ]]; then
    SKILL_MD="$REPO_ROOT/.agents/skills/loom-guide/SKILL.md"
elif [[ -f "$REPO_ROOT/defaults/.agents/skills/loom-guide/SKILL.md" ]]; then
    SKILL_MD="$REPO_ROOT/defaults/.agents/skills/loom-guide/SKILL.md"
else
    SKILL_MD=""
fi

source "$SCRIPT_DIR/lib/assert.sh"

if [[ ! -f "$GUIDE_MD" ]]; then
    echo -e "${RED}FATAL${NC}: guide.md not found"
    exit 1
fi
if ! command -v jq >/dev/null 2>&1; then
    echo "SKIP: jq not available"
    exit 0
fi

COPIES=("guide.md:$GUIDE_MD")
if [[ -n "$SKILL_MD" ]]; then
    COPIES+=("SKILL.md:$SKILL_MD")
else
    echo "SKIP: no loom-guide SKILL.md mirror in this layout" >&2
fi

UNREADABLE_MODES="fail empty blank garbage null object stream truncated"
WELL_FORMED_MODES="ok partial empty-array"

extract_fn() { awk -v re="^$1\\\\(\\\\) \\\\{" '$0 ~ re {f=1} f{print} /^\}/{if(f) exit}' "$2"; }

strip_shape_guard() {
    awk '/^  if \[ -n "\$inventory_ok" \] && ! /{skip=1; next} skip && /^  fi$/{skip=0; next} !skip{print}' <<<"$1"
}

section_of() { awk -v h="## $1" '$0 ~ "^"h {f=1; next} /^## /{f=0} f' <<<"$2"; }
rows() { grep '^- ' <<<"$1" || true; }
balance_row() { grep -F -- "| $1 |" <<<"$2" || true; }

TMPROOT="$(mktemp -d)"
trap 'rm -rf "$TMPROOT"' EXIT
FIX="$TMPROOT/fixtures"
mkdir -p "$FIX" "$TMPROOT/bin"

cat > "$TMPROOT/gh-stub" <<'STUB'
#!/usr/bin/env bash
set -uo pipefail
kind="${1:-}"; shift || true
label=""; search=""; jqexpr=""; state=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --label)  label="${2:-}"; shift 2 ;;
    --search) search="${2:-}"; shift 2 ;;
    --jq)     jqexpr="${2:-}"; shift 2 ;;
    --state)  state="${2:-}"; shift 2 ;;
    *)        shift ;;
  esac
done
fixture="$LOOM_FIXTURE_DIR/${kind}-${label//:/_}.json"
if [[ "$kind" == pr && -z "$label" ]]; then
  case "${LOOM_INVENTORY_MODE:-ok}" in
    fail)        exit 1 ;;
    empty)       exit 0 ;;
    blank)       printf ' \n\t\n'; exit 0 ;;
    garbage)     printf 'upstream cache: connection reset by peer\n'; exit 0 ;;
    null)        printf 'null'; exit 0 ;;
    object)      printf '{"message":"cache: no body for this key"}'; exit 0 ;;
    stream)      printf '[][]'; exit 0 ;;
    truncated)   printf '[{"number":8001,"title":"cut mid-docum'; exit 0 ;;
    ok)          json="$(cat "$fixture")" ;;
    partial)     json="$(jq -c '[.[] | select(.number != 8002)]' "$fixture")" ;;
    empty-array) json='[]' ;;
    *)           echo "gh-stub: unknown LOOM_INVENTORY_MODE='${LOOM_INVENTORY_MODE:-}'" >&2; exit 9 ;;
  esac
  if [[ -n "$jqexpr" ]]; then printf '%s' "$json" | jq -r "$jqexpr"; else printf '%s' "$json"; fi
  exit 0
fi
if [[ -f "$fixture" ]]; then json="$(cat "$fixture")"; else json='[]'; fi
if [[ -n "$state" ]]; then
  json="$(printf '%s' "$json" | jq --arg s "$state" '[.[] | select((.state // "open") == $s)]')"
fi
for tok in $search; do
  case "$tok" in
    -label:*)
      excl="${tok#-label:}"
      json="$(printf '%s' "$json" | jq --arg l "$excl" '[.[] | select(((.labels // []) | map(.name) | index($l)) | not)]')"
      ;;
  esac
done
if [[ -n "$jqexpr" ]]; then printf '%s' "$json" | jq -r "$jqexpr"; else printf '%s' "$json"; fi
STUB
chmod +x "$TMPROOT/gh-stub"

cat > "$TMPROOT/bin/loom-daemon" <<'STUB'
#!/usr/bin/env bash
if [[ "${1:-}" == park-record && "${2:-}" == parse ]]; then cat >/dev/null; fi
exit 1
STUB
chmod +x "$TMPROOT/bin/loom-daemon"

cat > "$FIX/issue-loom_issue.json" <<'JSON'
[
  {"number":7100,"title":"Plain ready issue","labels":[{"name":"loom:issue"}]},
  {"number":7101,"title":"Issue closed by an approved PR","labels":[{"name":"loom:issue"}]},
  {"number":7103,"title":"Issue held by a partial PR","labels":[{"name":"loom:issue"}]}
]
JSON

cat > "$FIX/pr-.json" <<'JSON'
[
  {"number":8001,"title":"Approved PR closing an issue","state":"open","labels":[{"name":"loom:pr"}],"closingIssuesReferences":[{"number":7101}],"body":"Closes #7101"},
  {"number":8002,"title":"Held partial PR","state":"open","labels":[{"name":"loom:pr"},{"name":"loom:operator"}],"closingIssuesReferences":[],"body":"Part of #7103"},
  {"number":8003,"title":"Approved chore PR","state":"open","labels":[{"name":"loom:pr"}],"closingIssuesReferences":[],"body":""}
]
JSON

RPB_RUN='eval "$HELPERS_SRC"; eval "$RPB_SRC"; render_plan_body'
GATE_RUN=''

render() {
    LOOM_INVENTORY_MODE="$1" LOOM_FIXTURE_DIR="$FIX" PATH="$TMPROOT/bin:$PATH" GH_READ="$TMPROOT/gh-stub" \
    HELPERS_SRC="$HELPERS_BODY" RPB_SRC="${2:-$RPB_BODY}" bash -c "$RPB_RUN" 2>"$TMPROOT/stderr"
}

gate_run() {
    LOOM_INVENTORY_MODE="$1" LOOM_FIXTURE_DIR="$FIX" PATH="$TMPROOT/bin:$PATH" GH_READ="$TMPROOT/gh-stub" \
    HELPERS_SRC="$HELPERS_BODY" RPB_SRC="${2:-$RPB_BODY}" bash -c "$GATE_RUN" 2>"$TMPROOT/gate-stderr"
}

echo "Work-plan open-PR inventory read guard (issue #15785): exit 0 with no usable JSON is unreadable"

for copy in "${COPIES[@]}"; do
    tag="${copy%%:*}"
    src="${copy#*:}"
    echo ""
    echo "Copy: $tag ($src)"

    RPB_BODY="$(extract_fn render_plan_body "$src")"
    UWP_BODY="$(extract_fn update_work_plan "$src")"
    HELPERS_BODY="$(awk -f "$SCRIPT_DIR/lib/guide-plan-helpers.awk" "$src")" || HELPERS_BODY=""
    GATE_SRC=""
    if [[ -n "$UWP_BODY" ]]; then
        GATE_SRC="$(awk '/new_body="\$\(render_plan_body\)"/{f=1} f{print} f&&/^  fi$/&&seen{exit} f&&/inventory-unreadable/{seen=1}' <<<"$UWP_BODY")"
    fi
    if [[ -z "$RPB_BODY" || -z "$HELPERS_BODY" || -z "$GATE_SRC" ]]; then
        fail "$tag: could not extract render_plan_body(), the plan helpers and the inventory gate from update_work_plan() (harness broken)"
        continue
    fi
    GATE_RUN='eval "$HELPERS_SRC"; eval "$RPB_SRC"; record_work_plan_receipt() { echo "RECEIPT=$1"; }
  gate() { '"$GATE_SRC"'
    echo PASSED_GATE; return 0; }
  gate; echo "RC=$?"'
    pass "$tag: render_plan_body(), the plan helpers and the update_work_plan() inventory gate extracted"

    echo ""
    echo "Test 0 ($tag): the stub's mode switch targets the one inventory read"
    INVENTORY_QUERY_COUNT="$(grep -cF '"$GH_READ" pr list --state open' <<<"$RPB_BODY" || true)"
    assert_eq "1" "$INVENTORY_QUERY_COUNT" "$tag: exactly one unlabelled 'pr list --state open' inventory read in render_plan_body()"

    FAIL_RENDER="$(render fail)"
    if [[ -z "$FAIL_RENDER" ]]; then
        fail "$tag: render_plan_body() produced no output against the fixtures (harness broken)"
        continue
    fi
    pass "$tag: render_plan_body() executed against the stubbed \$GH_READ"

    echo ""
    echo "Test 1 ($tag): every unreadable stdout shape renders the in-band note and degrades like a non-zero exit"
    for mode in $UNREADABLE_MODES; do
        out="$(render "$mode")"
        err="$(cat "$TMPROOT/stderr")"
        note_count="$(grep -c 'loom:inventory-unreadable' <<<"$out" || true)"
        if [[ "$note_count" -eq 1 ]] && grep -qF 'Open-PR inventory unreadable — Ready is NOT net of open-PR admission this tick' <<<"$out"; then
            pass "$tag: [$mode] renders the 'Open-PR inventory unreadable' note exactly once"
        else
            fail "$tag: [$mode] must render the 'Open-PR inventory unreadable' note exactly once (found $note_count)"
        fi
        if [[ "$out" == "$FAIL_RENDER" ]]; then
            pass "$tag: [$mode] degrades exactly like a non-zero-exit read"
        else
            fail "$tag: [$mode] must degrade exactly like a non-zero-exit read"
        fi
        if grep -qE 'integer expression expected|jq: (error|parse)' <<<"$err"; then
            fail "$tag: [$mode] leaks a shell/jq error to stderr instead of failing visibly: $(head -n 1 <<<"$err")"
        else
            pass "$tag: [$mode] no shell/jq error leaks to stderr"
        fi
    done

    echo ""
    echo "Test 2 ($tag): the unreadable render fails visibly, not silently"
    FAIL_PILEUP="$(section_of 'Operator Attention' "$FAIL_RENDER")"
    FAIL_APPROVED="$(section_of 'Approved' "$FAIL_RENDER")"
    FAIL_READY="$(section_of 'Ready' "$FAIL_RENDER")"
    UNFILTERED_READY='- **#7100**: Plain ready issue
- **#7101**: Issue closed by an approved PR
- **#7103**: Issue held by a partial PR'
    if [[ -z "$(rows "$FAIL_PILEUP")" && -z "$(rows "$FAIL_APPROVED")" ]] \
        && grep -qF '_None._' <<<"$FAIL_PILEUP" && grep -qF '_None._' <<<"$FAIL_APPROVED"; then
        pass "$tag: an unreadable read renders no Pileup and no Approved row (visible _None._ under the note)"
    else
        fail "$tag: an unreadable read must render no Pileup and no Approved row"
    fi
    if [[ -n "$(balance_row 'Operator merge-risk holds' "$FAIL_RENDER")" && -n "$(balance_row 'Approved PRs awaiting merge' "$FAIL_RENDER")" ]] \
        && grep -qF '| Operator merge-risk holds | 0 |' <<<"$FAIL_RENDER" \
        && grep -qF '| Approved PRs awaiting merge | 0 |' <<<"$FAIL_RENDER"; then
        pass "$tag: the unreadable render counts Pileup and Approved as 0"
    else
        fail "$tag: the unreadable render must count Pileup and Approved as 0"
    fi
    assert_eq "$UNFILTERED_READY" "$(rows "$FAIL_READY")" \
        "$tag: an unreadable read leaves Ready UNFILTERED (fail-open, the existing non-zero-exit contract)"

    echo ""
    echo "Test 3 ($tag): a well-formed inventory renders exactly as before — no note, rows intact"
    for mode in $WELL_FORMED_MODES; do
        out="$(render "$mode")"
        if grep -q 'loom:inventory-unreadable' <<<"$out" || grep -qF 'hit its 200-PR read limit' <<<"$out"; then
            fail "$tag: [$mode] a well-formed inventory must render neither the unreadable nor the truncation note"
        else
            pass "$tag: [$mode] a well-formed inventory renders neither the unreadable nor the truncation note"
        fi
    done

    OK_RENDER="$(render ok)"
    OK_PILEUP="$(section_of 'Operator Attention' "$OK_RENDER")"
    OK_APPROVED="$(section_of 'Approved' "$OK_RENDER")"
    OK_READY="$(section_of 'Ready' "$OK_RENDER")"
    OK_OPENPR="$(section_of 'Issues with Open PRs' "$OK_RENDER")"
    assert_eq '- **#8002**: Held partial PR *(held)*' "$(rows "$OK_PILEUP")" "$tag: [ok] Pileup row intact"
    assert_eq '- **#8001**: Approved PR closing an issue — closes #7101
- **#8002**: Held partial PR *(held)*
- **#8003**: Approved chore PR' "$(rows "$OK_APPROVED")" "$tag: [ok] Approved rows intact"
    assert_eq '- **#7100**: Plain ready issue' "$(rows "$OK_READY")" "$tag: [ok] Ready stays net of open-PR admission"
    assert_eq '- **#7101**: Issue closed by an approved PR — open PR #8001 (approved)
- **#7103**: Issue held by a partial PR — open PR #8002 (held)' "$(rows "$OK_OPENPR")" \
        "$tag: [ok] Issues with Open PRs rows intact"
    assert_eq '| Operator merge-risk holds | 1 |' "$(balance_row 'Operator merge-risk holds' "$OK_RENDER")" "$tag: [ok] Pileup balance row"
    assert_eq '| Approved PRs awaiting merge | 3 |' "$(balance_row 'Approved PRs awaiting merge' "$OK_RENDER")" "$tag: [ok] Approved balance row"

    PARTIAL_RENDER="$(render partial)"
    PARTIAL_PILEUP="$(section_of 'Operator Attention' "$PARTIAL_RENDER")"
    PARTIAL_APPROVED="$(section_of 'Approved' "$PARTIAL_RENDER")"
    if [[ -z "$(rows "$PARTIAL_PILEUP")" ]] && grep -qF '_None._' <<<"$PARTIAL_PILEUP"; then
        pass "$tag: [partial] an absent held PR renders an empty Pileup"
    else
        fail "$tag: [partial] an absent held PR must render an empty Pileup"
    fi
    assert_eq '- **#8001**: Approved PR closing an issue — closes #7101
- **#8003**: Approved chore PR' "$(rows "$PARTIAL_APPROVED")" "$tag: [partial] the present Approved rows render unchanged"

    EMPTY_ARRAY_RENDER="$(render empty-array)"
    EMPTY_ARRAY_PILEUP="$(section_of 'Operator Attention' "$EMPTY_ARRAY_RENDER")"
    EMPTY_ARRAY_APPROVED="$(section_of 'Approved' "$EMPTY_ARRAY_RENDER")"
    if grep -qF '_None._' <<<"$EMPTY_ARRAY_PILEUP" && grep -qF '_None._' <<<"$EMPTY_ARRAY_APPROVED" \
        && [[ -z "$(rows "$EMPTY_ARRAY_PILEUP")" && -z "$(rows "$EMPTY_ARRAY_APPROVED")" ]]; then
        pass "$tag: [empty-array] a readable empty inventory renders _None._ with no unreadable note"
    else
        fail "$tag: [empty-array] a readable empty inventory must render _None._ with no unreadable note"
    fi
    assert_eq "$UNFILTERED_READY" "$(rows "$(section_of 'Ready' "$EMPTY_ARRAY_RENDER")")" \
        "$tag: [empty-array] no open PR means every ready issue is dispatchable"

    echo ""
    echo "Test 4 ($tag): update_work_plan() refuses to write over every unreadable shape"
    for mode in $UNREADABLE_MODES; do
        g="$(gate_run "$mode")"
        if grep -qF 'RECEIPT=inventory-unreadable' <<<"$g" && grep -qF 'RC=1' <<<"$g" && ! grep -qF 'PASSED_GATE' <<<"$g"; then
            pass "$tag: [$mode] the write gate REFUSES (inventory-unreadable receipt, rc 1)"
        else
            fail "$tag: [$mode] the write gate must refuse; got: $(tr '\n' '|' <<<"$g")"
        fi
    done
    for mode in $WELL_FORMED_MODES; do
        g="$(gate_run "$mode")"
        if grep -qF 'PASSED_GATE' <<<"$g" && ! grep -qF 'RECEIPT=' <<<"$g"; then
            pass "$tag: [$mode] a readable inventory passes the write gate"
        else
            fail "$tag: [$mode] a readable inventory must pass the write gate; got: $(tr '\n' '|' <<<"$g")"
        fi
    done

    echo ""
    echo "Test 5 ($tag): negative control — without the stdout-shape guard the under-reported plan would be written"
    UNGUARDED_BODY="$(strip_shape_guard "$RPB_BODY")"
    if [[ "$UNGUARDED_BODY" != "$RPB_BODY" ]] && ! grep -qF '[ -n "$inventory_ok" ] && !' <<<"$UNGUARDED_BODY"; then
        pass "$tag: render_plan_body() carries the exit-0 stdout-shape guard"
    else
        fail "$tag: render_plan_body() must carry the exit-0 stdout-shape guard ('[ -n \"\$inventory_ok\" ] && !')"
    fi
    for mode in fail empty garbage; do
        out="$(render "$mode" "$UNGUARDED_BODY")"
        if [[ "$mode" == fail ]]; then
            if grep -q 'loom:inventory-unreadable' <<<"$out"; then
                pass "$tag: [negative control] a non-zero exit still renders the note without the shape guard"
            else
                fail "$tag: [negative control] a non-zero exit must still render the note without the shape guard"
            fi
        elif grep -q 'loom:inventory-unreadable' <<<"$out"; then
            fail "$tag: [negative control] [$mode] must render NO note once the shape guard is removed (control not isolating the guard)"
        else
            pass "$tag: [negative control] without the shape guard, [$mode] renders NO note (bug reproduced)"
        fi
    done
    g="$(gate_run garbage "$UNGUARDED_BODY")"
    if grep -qF 'PASSED_GATE' <<<"$g" && ! grep -qF 'RECEIPT=' <<<"$g"; then
        pass "$tag: [negative control] without the shape guard the write gate PASSES an exit-0 non-JSON read (the under-reported write)"
    else
        fail "$tag: [negative control] without the shape guard the write gate must pass an exit-0 non-JSON read; got: $(tr '\n' '|' <<<"$g")"
    fi
done

echo ""
echo "================================"
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}${TESTS_PASSED}${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}${TESTS_FAILED}${NC}"
    exit 1
fi
echo "All tests passed"
exit 0
