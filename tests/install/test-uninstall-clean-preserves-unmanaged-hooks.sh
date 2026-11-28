#!/usr/bin/env bash
# Test suite for uninstall-loom.sh's --clean directory sweep and .loom/hooks/
# ownership boundary (#5971).
#
# Usage: ./tests/install/test-uninstall-clean-preserves-unmanaged-hooks.sh
#
# Background: `.loom/hooks/` is a documented extension point Loom itself
# invokes (worktree.sh's post-worktree.sh hook) -- a repo is expected to add
# its own files there, and `.loom/roles/` likewise accepts consumer-authored
# custom roles. Epic #3835 Phase 5 (#4262) also deliberately excludes hooks/*
# from the installed-files manifest, so the manifest can never confirm a hooks
# file is Loom-owned. Before this fix, uninstall-loom.sh's --clean sweep walked
# .loom/roles|scripts|docs|hooks and queued EVERY file found there for hard
# deletion, with no ownership check at all -- silently deleting a repo-owned
# hook (the reported incident: kicad-tools' repo-owned
# .loom/hooks/post-worktree.sh was deleted on a --confirm-reinstall upgrade).
#
# The rule this suite pins, for uninstall-loom.sh's operator-requested
# --clean sweep:
#   * a path declared in `.loom/resync-ignore` is never removed, anywhere;
#   * inside `.loom/hooks/` specifically, a file the current defaults/ does not
#     ship is preserved even without a declaration (it is an invoked extension
#     point whose contents can never appear in the manifest);
#   * elsewhere, --clean keeps its documented "including unknown files"
#     contract -- the declaration is the opt-out.
# The Rust reinstall sweep (loom-daemon init) is strictly more conservative;
# its half of the boundary is unit-tested in loom-daemon/src/init/.
#
# This suite runs the REAL uninstall-loom.sh end-to-end (--yes --local
# --clean, no network/gh dependency) against scaffolded target repos and
# asserts:
#   1. A stale but genuinely Loom-shipped hook copy is still removed (no
#      regression -- --clean must still be able to clean these up).
#   2. An unmanaged (repo-owned) hook file survives, byte-for-byte, and is
#      named in the script's output rather than silently removed.
#   3. A file that collides by name with a real Loom-shipped hook, but is
#      declared repo-owned via `.loom/resync-ignore`, survives even though
#      the basename matches.
#   4. The plain (non---clean) --local path -- the one the reported repro
#      actually takes -- leaves the unmanaged hook alone.
#   5. Outside hooks/, --clean still removes an unpinned custom role (its
#      documented contract) but honors a .loom/resync-ignore pin.
#   6. A declared repo-owned path survives even when the previous install's
#      `installed_files` manifest wrongly claims Loom wrote it.
#   7. The other half of that boundary, in `.claude/settings.json` (#15764):
#      every command shape a Loom writer emits for a project-level hook entry
#      is stripped -- the two `.loom/hooks/`-prefixed bare forms (unquoted and
#      pre-#3277 bare-relative), the QUOTED bare form (#6544), and the two
#      launcher one-liners (a `bash -c '...'` body whose `.loom/hooks/` path
#      sits mid-command, behind no prefix). `.hooks` is deleted once emptied,
#      and a file that held nothing else is removed.
#   8. A hand-written entry that merely NAMES a Loom hook -- or a hand-written
#      `bash -c` launcher that resolves the project root the way Loom's does
#      but runs the repo's own hook, or a wrapper around Loom's launcher --
#      survives byte-for-byte, even when it shares a matcher entry with a
#      Loom entry that is stripped.
#
# Tests 7-8 need jq (the uninstaller's settings.json cleanup is a no-op
# without it) and are skipped when it is unavailable.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
UNINSTALL_SH="$REPO_ROOT/scripts/uninstall-loom.sh"

source "$SCRIPT_DIR/lib/assert.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# make_target <dir> — scaffold a minimal, valid uninstall target: a git repo
# with a .loom/ tree and an install-metadata.json carrying an (empty)
# installed_files manifest, so uninstall-loom.sh takes the manifest-based
# removal path (the one the --clean sweep lives in).
make_target() {
  local target="$1"
  mkdir -p "$target/.loom/hooks" "$target/.loom/roles" "$target/.loom/scripts" "$target/.loom/docs"
  git -C "$target" init -q
  git -C "$target" config user.email "test@example.com"
  git -C "$target" config user.name "test"
  printf '{"installed_files": []}\n' > "$target/.loom/install-metadata.json"
}

# ============================================================================
# Test 1: a genuinely Loom-shipped hook basename is still cleaned up by
# --clean (no regression -- this is the sweep's actual intended purpose,
# per scripts/install/manifest.sh's hooks/* CAVEAT comment: install.sh
# --quick copies defaults/hooks/*.sh into .loom/hooks/ but deliberately
# leaves them out of the manifest, so --clean is the only path that retires
# a stale copy).
# ============================================================================
echo ""
echo "=== --clean still removes a stale, genuinely Loom-shipped hook copy ==="

TARGET1="$WORK/target1"
make_target "$TARGET1"
cp "$REPO_ROOT/defaults/hooks/guard-destructive.sh" "$TARGET1/.loom/hooks/guard-destructive.sh"
git -C "$TARGET1" add -A
git -C "$TARGET1" commit -q -m init

OUT1="$("$UNINSTALL_SH" --yes --local --clean "$TARGET1" 2>&1)"

assert_eq "stale Loom-shipped hooks/guard-destructive.sh is removed" \
  "no" \
  "$([[ -f "$TARGET1/.loom/hooks/guard-destructive.sh" ]] && echo yes || echo no)"
assert_eq "removal output names the removed hook file" \
  "yes" \
  "$(printf '%s' "$OUT1" | grep -qF '.loom/hooks/guard-destructive.sh' && echo yes || echo no)"

# ============================================================================
# Test 2 (the reported incident, #5971): an unmanaged repo-owned hook file
# (basename never shipped by defaults/hooks/) survives --clean byte-for-byte,
# and is explicitly named as preserved rather than silently dropped.
# ============================================================================
echo ""
echo "=== --clean preserves an unmanaged repo-owned .loom/hooks/ file ==="

TARGET2="$WORK/target2"
make_target "$TARGET2"
MARKER='# REPO-OWNED kicad-tools-style project hook (issue #4558 precedent)'
cat > "$TARGET2/.loom/hooks/post-worktree.sh" <<EOF
#!/bin/bash
$MARKER
cd "\$1" && uv sync --frozen --extra dev
EOF
git -C "$TARGET2" add -A
git -C "$TARGET2" commit -q -m init

OUT2="$("$UNINSTALL_SH" --yes --local --clean "$TARGET2" 2>&1)"

assert_eq "unmanaged hooks/post-worktree.sh survives --clean" \
  "yes" \
  "$([[ -f "$TARGET2/.loom/hooks/post-worktree.sh" ]] && echo yes || echo no)"
assert_eq "survived file content is byte-for-byte unchanged" \
  "yes" \
  "$(grep -qF "$MARKER" "$TARGET2/.loom/hooks/post-worktree.sh" 2>/dev/null && echo yes || echo no)"
assert_eq "preservation is named in the script's output, not silent" \
  "yes" \
  "$(printf '%s' "$OUT2" | grep -qF 'preserving .loom/hooks/post-worktree.sh' && echo yes || echo no)"

# ============================================================================
# Test 3: a file whose basename collides with a real Loom-shipped hook, but
# is declared repo-owned via .loom/resync-ignore, survives despite the
# collision (the edge case the Curator flagged: a customized fork of a
# Loom-named hook must not be force-deleted by --clean).
# ============================================================================
echo ""
echo "=== --clean honors .loom/resync-ignore even when the basename collides ==="

TARGET3="$WORK/target3"
make_target "$TARGET3"
FORK_MARKER='# CUSTOMIZED FORK -- do not overwrite/delete'
printf '#!/bin/bash\n%s\n' "$FORK_MARKER" > "$TARGET3/.loom/hooks/guard-loom-workflow.sh"
printf 'hooks/guard-loom-workflow.sh\n' > "$TARGET3/.loom/resync-ignore"
git -C "$TARGET3" add -A
git -C "$TARGET3" commit -q -m init

OUT3="$("$UNINSTALL_SH" --yes --local --clean "$TARGET3" 2>&1)"

assert_eq "resync-ignore-pinned hooks/guard-loom-workflow.sh survives --clean" \
  "yes" \
  "$([[ -f "$TARGET3/.loom/hooks/guard-loom-workflow.sh" ]] && echo yes || echo no)"
assert_eq "pinned file content is unchanged" \
  "yes" \
  "$(grep -qF "$FORK_MARKER" "$TARGET3/.loom/hooks/guard-loom-workflow.sh" 2>/dev/null && echo yes || echo no)"
assert_eq "resync-ignore preservation is named in the script's output" \
  "yes" \
  "$(printf '%s' "$OUT3" | grep -qF 'preserving .loom/hooks/guard-loom-workflow.sh (declared repo-owned in .loom/resync-ignore)' && echo yes || echo no)"

# ============================================================================
# Test 4: plain (non --clean) reinstall path also preserves the unmanaged
# hook -- covers the literal reported repro
# (`install.sh --quick --yes --confirm-reinstall`, which chains a non---clean
# `uninstall-loom.sh --yes --local`).
# ============================================================================
echo ""
echo "=== plain (non --clean) --local uninstall never touches .loom/hooks/ unmanaged files ==="

TARGET4="$WORK/target4"
make_target "$TARGET4"
printf '#!/bin/bash\n%s\n' "$MARKER" > "$TARGET4/.loom/hooks/post-worktree.sh"
git -C "$TARGET4" add -A
git -C "$TARGET4" commit -q -m init

"$UNINSTALL_SH" --yes --local "$TARGET4" >/dev/null 2>&1

assert_eq "unmanaged hooks/post-worktree.sh survives a plain --local uninstall" \
  "yes" \
  "$([[ -f "$TARGET4/.loom/hooks/post-worktree.sh" ]] && echo yes || echo no)"

# ============================================================================
# Test 5: outside .loom/hooks/, --clean keeps its documented contract ("remove
# all files in managed directories, including unknown files") -- an operator
# who passes --clean asked for that. The declaration is the opt-out: a custom
# role pinned in .loom/resync-ignore survives, an unpinned one does not. This
# pins BOTH halves so neither can drift.
# ============================================================================
echo ""
echo "=== outside hooks/, --clean removes an unpinned custom role but honors a pin ==="

TARGET5="$WORK/target5"
make_target "$TARGET5"
ROLE_MARKER='# Custom project role, authored by this repo'
printf '%s\n' "$ROLE_MARKER" > "$TARGET5/.loom/roles/designer.md"
printf '%s\n' "$ROLE_MARKER" > "$TARGET5/.loom/roles/pinned-role.md"
printf 'roles/pinned-role.md\n' > "$TARGET5/.loom/resync-ignore"
git -C "$TARGET5" add -A
git -C "$TARGET5" commit -q -m init

OUT5="$("$UNINSTALL_SH" --yes --local --clean "$TARGET5" 2>&1)"

assert_eq "unpinned roles/designer.md is still removed by --clean (contract preserved)" \
  "no" \
  "$([[ -f "$TARGET5/.loom/roles/designer.md" ]] && echo yes || echo no)"
assert_eq "pinned roles/pinned-role.md survives --clean" \
  "yes" \
  "$([[ -f "$TARGET5/.loom/roles/pinned-role.md" ]] && echo yes || echo no)"
assert_eq "pinned custom role content is unchanged" \
  "yes" \
  "$(grep -qF "$ROLE_MARKER" "$TARGET5/.loom/roles/pinned-role.md" 2>/dev/null && echo yes || echo no)"
assert_eq "pinned-role preservation is named in the output" \
  "yes" \
  "$(printf '%s' "$OUT5" | grep -qF 'preserving .loom/roles/pinned-role.md (declared repo-owned in .loom/resync-ignore)' && echo yes || echo no)"

# ============================================================================
# Test 6: a declared repo-owned path outranks the manifest. A legacy
# over-broad `installed_files` entry must not be able to delete a file the
# repo explicitly claimed in .loom/resync-ignore.
# ============================================================================
echo ""
echo "=== .loom/resync-ignore outranks an installed_files manifest entry ==="

TARGET6="$WORK/target6"
make_target "$TARGET6"
CLAIMED_MARKER='# repo-owned despite the stale manifest entry'
printf '%s\n' "$CLAIMED_MARKER" > "$TARGET6/.loom/scripts/project-local.sh"
printf 'scripts/project-local.sh\n' > "$TARGET6/.loom/resync-ignore"
printf '{"installed_files": [".loom/scripts/project-local.sh"]}\n' \
  > "$TARGET6/.loom/install-metadata.json"
git -C "$TARGET6" add -A
git -C "$TARGET6" commit -q -m init

OUT6="$("$UNINSTALL_SH" --yes --local "$TARGET6" 2>&1)"

assert_eq "manifest-listed but declared repo-owned file survives" \
  "yes" \
  "$([[ -f "$TARGET6/.loom/scripts/project-local.sh" ]] && echo yes || echo no)"
assert_eq "declared-ownership preservation is named in the output" \
  "yes" \
  "$(printf '%s' "$OUT6" | grep -qF 'preserving .loom/scripts/project-local.sh (declared repo-owned in .loom/resync-ignore)' && echo yes || echo no)"

# ============================================================================
# Test 7 (#15764): .claude/settings.json -- every command shape a Loom writer
# emits for a project-level hook entry is stripped. The uninstaller's jq
# filter knew only two PREFIXES (the unquoted ${CLAUDE_PROJECT_DIR}/.loom/hooks/
# form and the pre-#3277 bare-relative .loom/hooks/ one), so the QUOTED form
# written since #6544 and the two launcher commands (a `bash -c '...'`
# one-liner, whose .loom/hooks/ path sits mid-body behind no prefix) outlived
# every uninstall -- contradicting the documented "no project-level entry can
# survive an uninstall" contract.
#
# No launcher fixture is hand-copied. The machine-level one comes from its
# writer in scripts/install/provision-hooks.sh; the project-level one from
# that writer too once `_phook_project_cmd` lands (#15741), and until then
# from this repo's own .claude/settings.json (#7761) -- the only place the
# shape exists yet. A writer edit that outdates the uninstaller's markers
# then fails here instead of in the field.
# ============================================================================
echo ""
echo "=== uninstall strips every Loom-written hook command shape from .claude/settings.json ==="

if ! command -v jq >/dev/null 2>&1; then
  echo "  (skipped: jq unavailable -- uninstall-loom.sh leaves .claude/settings.json alone without it)"
else
  # shellcheck source=../../scripts/install/provision-hooks.sh
  source "$REPO_ROOT/scripts/install/provision-hooks.sh"

  # hook_commands <settings.json> -- every hook command naming a .loom/hooks/
  # script, one per line.
  hook_commands() {
    jq -r '[.. | objects | .command? // empty | select(contains(".loom/hooks/"))] | .[]' "$1" 2>/dev/null
  }

  # The machine-level launcher `provision_loom_hooks` wires (user-scope by
  # design; the uninstaller must still strip a copy that lands in a
  # project-level file).
  MACHINE_LAUNCHER="$(_phook_cmd guard-worktree-paths.sh)"

  # project_launcher <event> <name> -- the PROJECT-level launcher command for
  # <name> under <event>. Its writer is `_phook_project_cmd` (#15741); until
  # that lands the shape exists only in this repo's own .claude/settings.json
  # (#7761), so it is read from there instead of hand-copied.
  project_launcher() {
    local event="$1" name="$2"
    if declare -F _phook_project_cmd >/dev/null 2>&1; then
      _phook_project_cmd "$event" "$name"
    else
      jq -r --arg e "$event" --arg n ".loom/hooks/$name" '
        [(.hooks // {})[$e][]? | .hooks[]? | .command // ""
         | select(startswith("bash -c \u0027")) | select(contains($n))
         | select(contains("defaults/hooks/") | not)][0]' \
        "$REPO_ROOT/.claude/settings.json"
    fi
  }
  PROJECT_LAUNCHER="$(project_launcher PreToolUse guard-destructive.sh)"
  # Same writer, carrying the fail-open prefix a PreToolUse non-guard gets.
  PROJECT_LAUNCHER_FAILOPEN="$(project_launcher PreToolUse roll-pause.sh)"

  assert_eq "fixture: the machine-level launcher is _phook_cmd's bash -c one-liner" \
    "bash -c '" "${MACHINE_LAUNCHER:0:9}"
  assert_eq "fixture: the machine-level launcher names its .loom/hooks/ copy" \
    "yes" "$([[ "$MACHINE_LAUNCHER" == *"/.loom/hooks/guard-worktree-paths.sh"* ]] && echo yes || echo no)"
  assert_eq "fixture: the machine-level launcher names its defaults/hooks/ target" \
    "yes" "$([[ "$MACHINE_LAUNCHER" == *"/defaults/hooks/guard-worktree-paths.sh"* ]] && echo yes || echo no)"
  assert_eq "fixture: the project launcher is a bash -c one-liner" \
    "bash -c '" "${PROJECT_LAUNCHER:0:9}"
  assert_eq "fixture: the project launcher routes through .loom/hooks/hook-wiring.sh to its hook" \
    "yes" "$([[ "$PROJECT_LAUNCHER" == *".loom/hooks/hook-wiring.sh"* && "$PROJECT_LAUNCHER" == *".loom/hooks/guard-destructive.sh"* ]] && echo yes || echo no)"
  assert_eq "fixture: the fail-open project launcher carries its export prefix" \
    "yes" "$([[ "$PROJECT_LAUNCHER_FAILOPEN" == "bash -c 'export LOOM_GUARD_WIRING_FAILOPEN=1; "* && "$PROJECT_LAUNCHER_FAILOPEN" == *".loom/hooks/roll-pause.sh"* ]] && echo yes || echo no)"

  # write_loom_shapes <settings.json> [--with-project-setting]: one entry in
  # every shape a Loom writer emits, plus (optionally) an unrelated project
  # setting the uninstall must leave alone.
  write_loom_shapes() {
    local out="$1" extra='{}'
    [[ "${2:-}" == "--with-project-setting" ]] && extra='{"someProjectSetting": true}'
    jq -n \
      --arg unquoted '${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh' \
      --arg bare_relative '.loom/hooks/guard-loom-workflow.sh' \
      --arg quoted '"${CLAUDE_PROJECT_DIR}/.loom/hooks/skill-router.sh"' \
      --arg machine "$MACHINE_LAUNCHER" \
      --arg launcher "$PROJECT_LAUNCHER" \
      --arg launcher_failopen "$PROJECT_LAUNCHER_FAILOPEN" \
      --argjson extra "$extra" \
      '$extra + {
        hooks: {
          "PreToolUse": [ { "matcher": "Bash", "hooks": [
            { "type": "command", "command": $unquoted },
            { "type": "command", "command": $launcher_failopen, "timeout": 10 }
          ] } ],
          "UserPromptSubmit": [ { "matcher": "", "hooks": [
            { "type": "command", "command": $bare_relative },
            { "type": "command", "command": $quoted },
            { "type": "command", "command": $machine },
            { "type": "command", "command": $launcher }
          ] } ],
          "Stop": [ { "matcher": "", "hooks": [
            { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-background-subagents.sh" }
          ] } ]
        }
      }' > "$out"
  }

  TARGET7="$WORK/target7"
  make_target "$TARGET7"
  mkdir -p "$TARGET7/.claude"
  write_loom_shapes "$TARGET7/.claude/settings.json" --with-project-setting
  git -C "$TARGET7" add -A
  git -C "$TARGET7" commit -q -m init

  assert_eq "precondition: the fixture holds 7 Loom hook commands across every writer shape" \
    "7" "$(hook_commands "$TARGET7/.claude/settings.json" | grep -c .)"

  OUT7="$("$UNINSTALL_SH" --yes --local "$TARGET7" 2>&1)"

  assert_eq "all shapes: settings.json kept (an unrelated project setting survives)" \
    "yes" "$([[ -f "$TARGET7/.claude/settings.json" ]] && echo yes || echo no)"
  assert_eq "all shapes: no Loom hook command remains (unquoted, bare-relative, quoted, both launchers)" \
    "0" "$(hook_commands "$TARGET7/.claude/settings.json" | grep -c .)"
  assert_eq "all shapes: .hooks is deleted once emptied" \
    "false" "$(jq 'has("hooks")' "$TARGET7/.claude/settings.json")"
  assert_eq "all shapes: the unrelated project setting survives" \
    "true" "$(jq '.someProjectSetting == true' "$TARGET7/.claude/settings.json")"
  assert_eq "all shapes: no jq failure warning emitted" \
    "no" "$(printf '%s' "$OUT7" | grep -qF 'Failed to process .claude/settings.json' && echo yes || echo no)"

  # The same entries with nothing else in the file: it was entirely Loom's.
  TARGET7B="$WORK/target7b"
  make_target "$TARGET7B"
  mkdir -p "$TARGET7B/.claude"
  write_loom_shapes "$TARGET7B/.claude/settings.json"
  git -C "$TARGET7B" add -A
  git -C "$TARGET7B" commit -q -m init

  OUT7B="$("$UNINSTALL_SH" --yes --local "$TARGET7B" 2>&1)"

  assert_eq "all-Loom: settings.json removed (nothing but Loom hook entries in it)" \
    "no" "$([[ -f "$TARGET7B/.claude/settings.json" ]] && echo yes || echo no)"
  assert_eq "all-Loom: removal is named in the script's output" \
    "yes" "$(printf '%s' "$OUT7B" | grep -qF '.claude/settings.json removed (was entirely Loom content)' && echo yes || echo no)"
fi

# ============================================================================
# Test 8 (#15764): the other side of the same coin -- stripping the launcher
# forms must not widen into "strip anything that mentions .loom/hooks/". A
# hand-written entry that merely names a Loom hook, a hand-written `bash -c`
# launcher that resolves the project root exactly the way Loom's launchers do
# but runs the repo's OWN hook (the #5971 scenario), and a hand-written
# wrapper around Loom's launcher itself all survive byte-for-byte --
# including when they share a matcher entry with a Loom entry that IS
# stripped.
# ============================================================================
echo ""
echo "=== hand-written settings.json entries that merely name a Loom hook survive ==="

if ! command -v jq >/dev/null 2>&1; then
  echo "  (skipped: jq unavailable -- uninstall-loom.sh leaves .claude/settings.json alone without it)"
else
  TARGET8="$WORK/target8"
  make_target "$TARGET8"
  mkdir -p "$TARGET8/.claude"
  # The hand-written entries on their own: this copy is the oracle.
  cat > "$WORK/handwritten8.json" <<'EOF'
{
  "someProjectSetting": true,
  "hooks": {
    "PreToolUse": [ { "matcher": "Bash", "hooks": [
      { "type": "command", "command": "bash \"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-loom-workflow.sh\"" }
    ] } ],
    "UserPromptSubmit": [ { "matcher": "", "hooks": [
      { "type": "command", "command": "echo see .loom/hooks/skill-router.sh" },
      { "type": "command", "command": "my-wrapper ${CLAUDE_PROJECT_DIR}/.loom/hooks/methodology-inject.sh" }
    ] } ],
    "Stop": [ { "matcher": "", "hooks": [
      { "type": "command", "command": "bash -c 'ROOT=$(cd \"$(git rev-parse --git-common-dir 2>/dev/null)/..\" 2>/dev/null && pwd); exec bash \"$ROOT/.loom/hooks/post-worktree.sh\"'" },
      { "type": "command", "command": "bash -c 'R=$(git rev-parse --git-common-dir 2>/dev/null); R=${R:+$(cd \"$R/..\" 2>/dev/null && pwd)}; W=${R:-${CLAUDE_PROJECT_DIR:-$PWD}}; exec bash \"$W/.loom/hooks/post-worktree.sh\"'" }
    ] } ]
  }
}
EOF
  # ...plus a hand-written wrapper AROUND Loom's own launcher (an operator
  # adding a timeout to it): the launcher is no longer the whole command, so
  # the entry is the operator's.
  jq --arg launcher "$PROJECT_LAUNCHER" \
     '.hooks.Stop[0].hooks += [{type:"command", command:("timeout 10 " + $launcher)}]' \
     "$WORK/handwritten8.json" > "$WORK/expected8.json"
  cp "$WORK/expected8.json" "$TARGET8/.claude/settings.json"
  EXPECTED8="$(jq -cS .hooks "$WORK/expected8.json")"
  # Loom's own entries, interleaved into the SAME matcher entries.
  jq --arg unquoted '${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh' \
     --arg machine "$MACHINE_LAUNCHER" \
     --arg launcher "$PROJECT_LAUNCHER" '
     .hooks.PreToolUse[0].hooks += [{type:"command", command:$launcher}]
     | .hooks.PreToolUse[0].hooks |= [{type:"command", command:$unquoted}] + .
     | .hooks.Stop[0].hooks += [{type:"command", command:$machine}]
     | .hooks.Stop[0].hooks |= [{type:"command", command:$launcher}] + .' \
     "$TARGET8/.claude/settings.json" > "$TARGET8/.claude/settings.json.tmp" \
     && mv "$TARGET8/.claude/settings.json.tmp" "$TARGET8/.claude/settings.json"
  git -C "$TARGET8" add -A
  git -C "$TARGET8" commit -q -m init

  assert_eq "precondition: the fixture mixes 6 hand-written with 4 Loom hook commands" \
    "10" "$(hook_commands "$TARGET8/.claude/settings.json" | grep -c .)"

  OUT8="$("$UNINSTALL_SH" --yes --local "$TARGET8" 2>&1)"

  assert_eq "unmanaged: settings.json kept" \
    "yes" "$([[ -f "$TARGET8/.claude/settings.json" ]] && echo yes || echo no)"
  assert_eq "unmanaged: exactly the hand-written entries remain -- byte-for-byte, in order; Loom's are gone" \
    "$EXPECTED8" "$(jq -cS .hooks "$TARGET8/.claude/settings.json")"
  assert_eq "unmanaged: the 3 entries that merely name a Loom hook survive" \
    "3" "$(hook_commands "$TARGET8/.claude/settings.json" | grep -cF -e 'guard-loom-workflow.sh' -e 'skill-router.sh' -e 'methodology-inject.sh')"
  assert_eq "unmanaged: the 2 hand-written root-resolving launchers of the repo's own hook survive" \
    "2" "$(hook_commands "$TARGET8/.claude/settings.json" | grep -cF 'post-worktree.sh')"
  assert_eq "unmanaged: the hand-written wrapper around Loom's launcher survives" \
    "1" "$(hook_commands "$TARGET8/.claude/settings.json" | grep -cF 'timeout 10 bash -c ')"
  assert_eq "unmanaged: the unrelated project setting survives" \
    "true" "$(jq '.someProjectSetting == true' "$TARGET8/.claude/settings.json")"
  assert_eq "preservation is named in the script's output, not silent" \
    "yes" "$(printf '%s' "$OUT8" | grep -qF '.claude/settings.json Loom hooks and permissions removed, project settings preserved' && echo yes || echo no)"
fi

# ============================================================================
# Summary
# ============================================================================
echo ""
echo "=========================================="
echo -e "Results: ${PASS} passed, ${FAIL} failed, ${TOTAL} total"
echo "=========================================="

if [[ $FAIL -gt 0 ]]; then
  exit 1
fi
exit 0
