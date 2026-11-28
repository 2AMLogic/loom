#!/usr/bin/env bash
# test-verify-install-scope.sh - Regression tests for verify-install.sh manifest
# scoping (issue #3600, Part B of #3597).
#
# verify-install.sh previously built its checksum manifest from a hard-coded
# directory walk and hashed CLAUDE.md whole-file. In a multi-tool consumer repo
# (Loom + a sibling installer such as Anvil + Repo Skills) that produced false
# DRIFT:
#
#   1. `find .claude/agents -name '*.md'` captured sibling agent shims Loom
#      never shipped (e.g. anvil-*.md).
#   2. A whole-file CLAUDE.md hash flagged drift whenever a sibling installer
#      edited CLAUDE.md *outside* Loom's <!-- BEGIN/END LOOM ORCHESTRATION -->
#      marker block.
#
# The fix derives the tracked-file set from .loom/install-metadata.json
# "installed_files" (Loom's ownership boundary) and hashes CLAUDE.md over its
# Loom marker region only. It also bumps the manifest schema to v2, so a stale
# v1 manifest reports "schema outdated" instead of false drift.
#
# Test strategy: seed a temp git repo with a hand-written install-metadata.json,
# a CLAUDE.md with a Loom block plus sibling sections, a Loom agent, and a
# sibling agent shim, then invoke the real script from the source checkout and
# assert on the generated manifest and verify exit codes.
#
# Unlike most install-tree tests (test-install-lock.sh, test-install-*-safety
# etc.), verify-install.sh is a *shipped* script (scripts/* -> .loom/scripts/*
# per scripts/install/manifest.sh) — the property this suite asserts matters
# most in a consumer repo, where verify-install.sh actually runs. This test
# therefore resolves its subject the way each layout actually lays it out:
# `.loom/scripts/verify-install.sh` first (installed consumer repos, and
# Loom's own dogfooded checkout), falling back to
# `defaults/scripts/verify-install.sh` (a bare source checkout with no
# .loom/scripts/ symlink/copy yet). See issue #6194.
#
# Usage:
#   bash .loom/scripts/tests/test-verify-install-scope.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
if [[ -f "$REPO_ROOT/.loom/scripts/verify-install.sh" ]]; then
    VERIFY_SCRIPT="$REPO_ROOT/.loom/scripts/verify-install.sh"
else
    VERIFY_SCRIPT="$REPO_ROOT/defaults/scripts/verify-install.sh"
fi

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

TESTS_RUN=0
TESTS_PASSED=0
TESTS_FAILED=0

assert_eq() {
    local expected="$1" actual="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$expected" == "$actual" ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg (expected '$expected', got '$actual')"
    fi
}

assert_not_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" != *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg (found unexpected '$needle')"
    fi
}

assert_contains() {
    local haystack="$1" needle="$2" msg="$3"
    TESTS_RUN=$((TESTS_RUN + 1))
    if [[ "$haystack" == *"$needle"* ]]; then
        TESTS_PASSED=$((TESTS_PASSED + 1))
        echo -e "  ${GREEN}PASS${NC}: $msg"
    else
        TESTS_FAILED=$((TESTS_FAILED + 1))
        echo -e "  ${RED}FAIL${NC}: $msg (missing '$needle')"
        echo "$haystack" | sed 's/^/      /'
    fi
}

if [[ ! -f "$VERIFY_SCRIPT" ]]; then
    echo "ERROR: $VERIFY_SCRIPT not found" >&2
    exit 1
fi
if ! command -v jq &>/dev/null; then
    echo "SKIP: jq not available (required by verify-install.sh verify mode)" >&2
    exit 0
fi

# Seed a temp git repo with a Loom install. Args set which optional bits exist.
# Writes CLAUDE.md with a Loom marker block flanked by sibling sections, one
# Loom agent, one sibling agent shim, settings.json, a user-edited config.json,
# and an install-metadata.json listing only Loom-owned paths.
seed_repo() {
    local tmp
    tmp=$(mktemp -d "${TMPDIR:-/tmp}/loom-vscope.XXXXXX")
    (
        cd "$tmp" || exit 1
        git init -q .
        git config user.email "test@example.com"
        git config user.name "Test"
        mkdir -p .loom .claude/agents

        cat > CLAUDE.md <<'EOF'
# Consumer Project

Sibling intro section owned by another tool.

<!-- BEGIN LOOM ORCHESTRATION -->
## Loom
Loom-managed content lives here.
<!-- END LOOM ORCHESTRATION -->

## Sibling Tail Section
More sibling-owned content.
EOF

        echo "loom builder agent" > .claude/agents/loom-builder.md
        echo "anvil sibling shim" > .claude/agents/anvil-foo.md
        printf '{"schema":"loom"}\n' > .claude/settings.json
        printf '{"nextAgentNumber":9,"user":"edited"}\n' > .loom/config.json

        # install-metadata.json lists only Loom-owned paths; anvil-foo.md is
        # deliberately absent (a sibling installer, not Loom, shipped it).
        cat > .loom/install-metadata.json <<'EOF'
{
  "loom_version": "0.10.9",
  "loom_commit": "deadbee",
  "installed_files": [".claude/agents/loom-builder.md",".claude/settings.json","CLAUDE.md",".loom/config.json"]
}
EOF
    )
    echo "$tmp"
}

# ---------------------------------------------------------------------------
# Case 1: sibling agent shim never enters the manifest.
# ---------------------------------------------------------------------------
echo "Case 1: sibling agent shim excluded from manifest"
REPO=$(seed_repo)
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
MANIFEST=$(cat "$REPO/.loom/manifest.json")
assert_not_contains "$MANIFEST" "anvil-foo.md" "Case 1: anvil-foo.md absent from .loom/manifest.json"
assert_contains "$MANIFEST" "loom-builder.md" "Case 1: Loom agent present in manifest"
assert_contains "$MANIFEST" '"version": 2' "Case 1: manifest schema is v2"
# User-mutable config.json must not be tracked (false-drift guard).
assert_not_contains "$MANIFEST" '".loom/config.json"' "Case 1: user config.json not tracked"
rm -rf "$REPO"
echo ""

# ---------------------------------------------------------------------------
# Case 2: sibling CLAUDE.md edit outside markers is clean; inside is drift.
# ---------------------------------------------------------------------------
echo "Case 2: CLAUDE.md region-scoped hashing"
REPO=$(seed_repo)
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )

# Edit OUTSIDE the Loom markers (both the intro and the tail sibling sections).
cat > "$REPO/CLAUDE.md" <<'EOF'
# Consumer Project

Sibling intro section HEAVILY rewritten by another tool.

<!-- BEGIN LOOM ORCHESTRATION -->
## Loom
Loom-managed content lives here.
<!-- END LOOM ORCHESTRATION -->

## Sibling Tail Section
Even more sibling-owned content appended later, plus a new paragraph.
EOF
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "0" "$?" "Case 2a: edit outside Loom markers -> verify exit 0"

# Edit INSIDE the Loom markers -> real drift.
cat > "$REPO/CLAUDE.md" <<'EOF'
# Consumer Project

Sibling intro.

<!-- BEGIN LOOM ORCHESTRATION -->
## Loom
Loom-managed content lives here — but TAMPERED inside the block.
<!-- END LOOM ORCHESTRATION -->

## Sibling Tail Section
tail.
EOF
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "1" "$?" "Case 2b: edit inside Loom markers -> verify exit 1 (drift)"

# Remove the Loom block entirely -> missing-markers drift.
printf '# Consumer Project\n\nNo Loom block remains.\n' > "$REPO/CLAUDE.md"
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "1" "$?" "Case 2c: removed Loom markers -> verify exit 1 (drift)"
rm -rf "$REPO"
echo ""

# ---------------------------------------------------------------------------
# Case 3: a v1-schema manifest reports "schema outdated", not false drift.
# ---------------------------------------------------------------------------
echo "Case 3: outdated v1 manifest schema"
REPO=$(seed_repo)
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
# Downgrade the recorded schema version to 1 in place.
sed 's/"version": 2/"version": 1/' "$REPO/.loom/manifest.json" > "$REPO/.loom/manifest.json.tmp"
mv "$REPO/.loom/manifest.json.tmp" "$REPO/.loom/manifest.json"

HUMAN_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify 2>&1 )
HUMAN_EXIT=$?
assert_eq "5" "$HUMAN_EXIT" "Case 3a: v1 manifest -> verify exit 5 (schema outdated, not 1)"
assert_contains "$HUMAN_OUT" "schema outdated" "Case 3b: human message says schema outdated"
assert_not_contains "$HUMAN_OUT" "DRIFT DETECTED" "Case 3c: no per-file drift reported for v1"

JSON_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --json 2>&1 )
assert_contains "$JSON_OUT" "manifest_schema_outdated" "Case 3d: json error is manifest_schema_outdated"

# Auto (no-arg) mode self-heals a v1 manifest by regenerating it.
( cd "$REPO" && bash "$VERIFY_SCRIPT" >/dev/null 2>&1 )
assert_eq "0" "$?" "Case 3e: auto mode regenerates v1 manifest (exit 0)"
REGEN=$(cat "$REPO/.loom/manifest.json")
assert_contains "$REGEN" '"version": 2' "Case 3f: auto mode rewrote manifest to v2"
rm -rf "$REPO"
echo ""

# ---------------------------------------------------------------------------
# Case 4: no install-metadata.json -> warned legacy directory-walk fallback.
# ---------------------------------------------------------------------------
echo "Case 4: legacy fallback when metadata absent"
FBTMP=$(mktemp -d "${TMPDIR:-/tmp}/loom-vscope-fb.XXXXXX")
(
    cd "$FBTMP" || exit 1
    git init -q .
    git config user.email "test@example.com"
    git config user.name "Test"
    mkdir -p .loom/roles .loom/scripts
    echo "builder role" > .loom/roles/builder.md
    echo "helper script" > .loom/scripts/helper.sh
    printf '# top\n\n<!-- BEGIN LOOM ORCHESTRATION -->\nloom\n<!-- END LOOM ORCHESTRATION -->\n' > CLAUDE.md
)
FB_OUT=$( cd "$FBTMP" && bash "$VERIFY_SCRIPT" generate 2>&1 )
assert_contains "$FB_OUT" "falling back to legacy directory walk" "Case 4a: warns about fallback"
FB_COUNT=$(jq '.file_count' "$FBTMP/.loom/manifest.json" 2>/dev/null || echo 0)
TESTS_RUN=$((TESTS_RUN + 1))
if [[ "$FB_COUNT" -ge 2 ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: Case 4b: fallback produced a non-empty manifest ($FB_COUNT files)"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: Case 4b: fallback manifest too small ($FB_COUNT files)"
fi
( cd "$FBTMP" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "0" "$?" "Case 4c: fallback verify is clean (exit 0)"
rm -rf "$FBTMP"
echo ""

# ---------------------------------------------------------------------------
# Case 5: marker-integrity check catches malformed marker structure that a
# hash-only comparison cannot -- specifically the "baked-in corruption" case,
# where `generate` runs against an ALREADY malformed CLAUDE.md, so the broken
# extraction is exactly what the manifest's hash records, and every later
# `verify` against the same still-malformed file would otherwise report a
# clean match (issue #6195).
# ---------------------------------------------------------------------------
echo "Case 5: marker-integrity check (issue #6195)"

# 5a: duplicated BEGIN/END pair, baked in at generate time.
REPO=$(seed_repo)
cat > "$REPO/CLAUDE.md" <<'EOF'
# Consumer Project

Sibling intro section.

<!-- BEGIN LOOM ORCHESTRATION -->
## Loom
First (stray) copy of the block.
<!-- END LOOM ORCHESTRATION -->

Some content between the two copies.

<!-- BEGIN LOOM ORCHESTRATION -->
## Loom
Second, canonical copy of the block.
<!-- END LOOM ORCHESTRATION -->

## Sibling Tail Section
tail.
EOF
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "1" "$?" "Case 5a: duplicated marker pair baked in at generate -> verify exit 1 (was silently clean pre-#6195)"
HUMAN_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify 2>&1 )
assert_contains "$HUMAN_OUT" "duplicated marker" "Case 5a: human output names duplicated marker(s)"
JSON_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --json 2>&1 )
assert_contains "$JSON_OUT" '"marker_integrity"' "Case 5a: json output carries a marker_integrity array"
assert_contains "$JSON_OUT" "duplicated marker" "Case 5a: json marker_integrity entry names the problem"
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$JSON_OUT" | jq -e . >/dev/null 2>&1; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: Case 5a: json output is well-formed"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: Case 5a: json output is not valid JSON"
fi
rm -rf "$REPO"
echo ""

# 5b: BEGIN present with no matching END, baked in at generate time.
REPO=$(seed_repo)
cat > "$REPO/CLAUDE.md" <<'EOF'
# Consumer Project

Sibling intro.

<!-- BEGIN LOOM ORCHESTRATION -->
## Loom
Block content with no closing marker -- a bad merge ate the END line.

## Sibling Tail Section
tail.
EOF
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "1" "$?" "Case 5b: missing END marker baked in at generate -> verify exit 1"
HUMAN_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify 2>&1 )
assert_contains "$HUMAN_OUT" "missing END marker" "Case 5b: human output names missing END marker"
rm -rf "$REPO"
echo ""

# 5c: well-formed markers still verify clean -- no false positive from the new check.
REPO=$(seed_repo)
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "0" "$?" "Case 5c: well-formed marker pair -> verify exit 0 (no false positive)"
JSON_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --json 2>&1 )
assert_contains "$JSON_OUT" '"marker_integrity": []' "Case 5c: json marker_integrity array is empty when well-formed"
rm -rf "$REPO"
echo ""

# 5d: content edited OUTSIDE the markers is not reported as a marker-integrity
# problem -- the check must not police repo-authored content (AC #2).
REPO=$(seed_repo)
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
cat > "$REPO/CLAUDE.md" <<'EOF'
# Consumer Project

Sibling intro section HEAVILY rewritten by another tool.

<!-- BEGIN LOOM ORCHESTRATION -->
## Loom
Loom-managed content lives here.
<!-- END LOOM ORCHESTRATION -->

## Sibling Tail Section
Even more sibling-owned content, unrelated to marker structure.
EOF
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "0" "$?" "Case 5d: sibling edit outside markers -> verify exit 0 (markers untouched)"
JSON_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --json 2>&1 )
assert_contains "$JSON_OUT" '"marker_integrity": []' "Case 5d: json marker_integrity array empty for sibling-only edit"
rm -rf "$REPO"
echo ""

# ---------------------------------------------------------------------------
# Case 6: an install-metadata entry for a Loom-shipped path with no file on
# disk fails `verify` (issue #15714).
#
# verify-install.sh derives its checksum file set from installed_files
# intersected with the files present, so an entry whose file is gone was
# skipped in silence: neither tracked nor reported. A consumer that resynced
# past a retirement (commands/loom/loom-iteration.md and loom-parent.md)
# kept both in installed_files and `verify` still said ALL FILES MATCH.
#
# Only Loom-shipped paths count (what resync re-copies): an install-once file
# the consumer deleted on purpose, and the runtime denylist, stay unflagged.
# ---------------------------------------------------------------------------
echo "Case 6: an installed_files entry with no file fails verify (#15714)"

seed_repo_shipped() {
    local tmp
    tmp=$(seed_repo)
    (
        cd "$tmp" || exit 1
        mkdir -p .loom/docs .loom/scripts .claude/commands/loom
        echo "shipped doc" > .loom/docs/kept.md
        echo "#!/usr/bin/env bash" > .loom/scripts/kept.sh
        echo "shipped command" > .claude/commands/loom/kept.md
    )
    echo "$tmp"
}

# 6a: two Loom-shipped entries with no file -> exit 1, each path named.
REPO=$(seed_repo_shipped)
cat > "$REPO/.loom/install-metadata.json" <<'EOF'
{
  "loom_version": "0.10.9",
  "loom_commit": "deadbee",
  "installed_files": [".claude/agents/loom-builder.md",".claude/settings.json","CLAUDE.md",".loom/config.json",".loom/docs/kept.md",".loom/scripts/kept.sh",".claude/commands/loom/kept.md",".claude/commands/loom/loom-iteration.md",".claude/commands/loom/loom-parent.md",".loom/docs/gone.md"]
}
EOF
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "1" "$?" "Case 6a: Loom-shipped entries with no file -> verify exit 1"
HUMAN_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify 2>&1 )
assert_contains "$HUMAN_OUT" ".claude/commands/loom/loom-iteration.md" "Case 6a: human output names the retired command entry"
assert_contains "$HUMAN_OUT" ".claude/commands/loom/loom-parent.md" "Case 6a: human output names the second retired command entry"
assert_contains "$HUMAN_OUT" ".loom/docs/gone.md" "Case 6a: human output names every such entry"
assert_contains "$HUMAN_OUT" "DRIFT DETECTED" "Case 6a: reported as drift"
assert_contains "$HUMAN_OUT" "resync-installed.sh" "Case 6a: names the remedy"
JSON_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --json 2>&1 )
assert_contains "$JSON_OUT" '"status": "drift"' "Case 6a: json status is drift"
assert_contains "$JSON_OUT" '"uninstalled": [' "Case 6a: json output carries an uninstalled array"
TESTS_RUN=$((TESTS_RUN + 1))
UNINSTALLED_COUNT=$(echo "$JSON_OUT" | jq '(.uninstalled // []) | length' 2>/dev/null || echo "?")
if [[ "$UNINSTALLED_COUNT" == "3" ]]; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: Case 6a: json uninstalled lists exactly the three absent shipped entries"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: Case 6a: json uninstalled has $UNINSTALLED_COUNT entries, expected 3"
fi
TESTS_RUN=$((TESTS_RUN + 1))
if echo "$JSON_OUT" | jq -e . >/dev/null 2>&1; then
    TESTS_PASSED=$((TESTS_PASSED + 1))
    echo -e "  ${GREEN}PASS${NC}: Case 6a: json output stays well-formed"
else
    TESTS_FAILED=$((TESTS_FAILED + 1))
    echo -e "  ${RED}FAIL${NC}: Case 6a: json output is not valid JSON"
fi
rm -rf "$REPO"
echo ""

# 6b: install-once and runtime-denylist entries with no file are never flagged.
REPO=$(seed_repo_shipped)
cat > "$REPO/.loom/install-metadata.json" <<'EOF'
{
  "loom_version": "0.10.9",
  "loom_commit": "deadbee",
  "installed_files": [".claude/agents/loom-builder.md",".claude/settings.json","CLAUDE.md",".loom/config.json",".loom/docs/kept.md",".loom/scripts/kept.sh",".claude/commands/loom/kept.md",".github/workflows/ci.yml",".codex/config.toml",".claude/README.md",".loom/daemon-state.json",".loom/worktrees/issue-1/.loom-managed"]
}
EOF
( cd "$REPO" && bash "$VERIFY_SCRIPT" generate --quiet )
( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --quiet )
assert_eq "0" "$?" "Case 6b: absent install-once (.github/workflows, .codex, .claude/README.md) and runtime-denylist entries -> verify exit 0"
JSON_OUT=$( cd "$REPO" && bash "$VERIFY_SCRIPT" verify --json 2>&1 )
assert_contains "$JSON_OUT" '"uninstalled": []' "Case 6b: json uninstalled array is empty"
assert_not_contains "$JSON_OUT" "ci.yml" "Case 6b: an absent install-once workflow is not named"
rm -rf "$REPO"
echo ""

# 6c: verify-install.sh's Loom-shipped classifier and resync-installed.sh's
# prune agree on every surface: the prune restores a still-shipped file or
# drops an entry retired upstream for exactly these paths, which is what makes
# "entry, no file" a defect worth failing on.
RESYNC_SCRIPT="$REPO_ROOT/.loom/scripts/resync-installed.sh"
[[ -f "$RESYNC_SCRIPT" ]] || RESYNC_SCRIPT="$REPO_ROOT/defaults/scripts/resync-installed.sh"
if [[ -f "$RESYNC_SCRIPT" ]]; then
    eval "$(awk '/^is_loom_shipped_path\(\) \{/ { grab=1 } grab { print } grab && /^}/ { exit }' "$VERIFY_SCRIPT")"
    # The arm line of resync's classifier: one `|`-separated pattern per copy
    # surface, each turned into a sample path by replacing its `*` with `x`
    # (noglob: the arms are patterns, not filenames to expand against the cwd).
    SURFACE_ARMS="$(awk '/^_is_loom_pure_copy_surface_path\(\) \{/ { on=1; next } on && /case/ { getline; print; exit }' "$RESYNC_SCRIPT" | tr -d ' \t')"
    SURFACE_ARMS="${SURFACE_ARMS%)}"
    set -f
    for arm in ${SURFACE_ARMS//|/ }; do
        sample="${arm//\*/x}"
        assert_eq "0" "$(is_loom_shipped_path "$sample" && echo 0 || echo 1)" \
            "Case 6c: '$sample' (a resync copy surface) is Loom-shipped for verify"
    done
    set +f
    assert_eq "0" "$(is_loom_shipped_path .claude/commands/loom/x.md && echo 0 || echo 1)" \
        "Case 6c: .claude/commands/loom/* is Loom-shipped (the #15714 surface)"
    assert_eq "1" "$(is_loom_shipped_path .github/workflows/ci.yml && echo 0 || echo 1)" \
        "Case 6c: an install-once workflow is not Loom-shipped"
    assert_eq "1" "$(is_loom_shipped_path CLAUDE.md && echo 0 || echo 1)" \
        "Case 6c: the merge-target CLAUDE.md is not Loom-shipped"
    assert_eq "1" "$(is_loom_shipped_path .claude/settings.json && echo 0 || echo 1)" \
        "Case 6c: .claude/settings.json is not Loom-shipped"
else
    echo "  SKIP: Case 6c: resync-installed.sh not found beside verify-install.sh"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
echo "==============================="
echo "Tests run:    $TESTS_RUN"
echo -e "Tests passed: ${GREEN}$TESTS_PASSED${NC}"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "Tests failed: ${RED}$TESTS_FAILED${NC}"
    exit 1
fi
echo "All tests passed."
exit 0
