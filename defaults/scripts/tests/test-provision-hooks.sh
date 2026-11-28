#!/usr/bin/env bash
# test-provision-hooks.sh — tests for user-scope Loom guard-hook wiring
# (Epic #3835 Phase 5, #4262).
#
# Covers scripts/install/provision-hooks.sh:
#   - fresh provision merges the Loom hook entries into a missing / empty /
#     populated ~/.claude/settings.json
#   - the verifiable-globals contract (PROVISIONED_HOOKS_SETTINGS /
#     PROVISIONED_HOOKS_BACKUP, mirroring provision-dispatcher.sh #4053)
#   - idempotent re-provision (no duplicates, including a requoted pre-existing
#     entry — the #4200 lesson)
#   - pre-existing non-Loom hooks + permissions are preserved
#   - invalid existing JSON -> soft-fail (return 1) with NO write
#   - a backup file is written before the first mutation
#   - backups are deduped (byte-identical to the last one -> skipped) and
#     bounded to a retention count, on BOTH provision and deprovision (#5387)
#   - the wired command WRAPPER behaves: no-ops outside a Loom workspace (AC3),
#     execs the machine-checkout hook inside one (AC1), and defers to a present
#     per-repo .loom/hooks/ copy (transition dedup, design decision 3)
#   - deprovision removes ONLY Loom-owned entries, preserving operator hooks
#   - #8372: the uncommitted-work consumer canary stub is wired on BOTH Stop and
#     SubagentStop in both install modes, deduped, deferred, and deprovisioned
#   - #6544: a project path containing a space survives `sh -c` execution —
#     since #15741 because the entry is a launcher command that never embeds
#     the path as text (it was a bare quoted path before), and the pre-#6544
#     UNQUOTED path-as-text entry is rewritten on the next re-provision
#   - #15741: project-level entries are the LAUNCHER form (`_phook_project_cmd`,
#     the shape `.claude/settings.json` carries): they route their event and
#     script through hook-wiring.sh and signal a broken install; the bare
#     `${CLAUDE_PROJECT_DIR}/.loom/hooks/<name>` commands earlier writers left
#     (quoted or not) are upgraded in place, custom commands are never touched,
#     and a written entry runs its hook / reports a broken install as configured
#
# Sandboxed $HOME per case via mktemp -d, matching test-provision-skills.sh.
#
# Source-tree-only by design (#6194/#6241): scripts/install/provision-hooks.sh
# lives at the repo root, not under defaults/, so it is never shipped into an
# installed consumer repo. This suite SKIPs (exit 0) rather than errors when
# run outside Loom's own checkout.

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# defaults/scripts/tests -> defaults/scripts/tests/../../.. -> repo root
REPO_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"

PROVISION_LIB="$REPO_ROOT/scripts/install/provision-hooks.sh"

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

# Local assert_eq kept: takes (actual, expected, msg); shared assert_eq is (expected, actual, msg).
assert_eq() {
    if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (expected '$2', got '$1')"; fi
}

if [[ ! -f "$PROVISION_LIB" ]]; then
    echo "SKIP: source-tree-only test, $PROVISION_LIB not found (not shipped into an installed repo)" >&2
    exit 0
fi
command -v jq >/dev/null 2>&1 || { echo "jq required for these tests"; exit 1; }

# A daemon-spawned session exports this (2am#4501); every wrapper
# case below that does not set it must see the checkout path.
unset LOOM_HOOKS_HOME

# shellcheck source=/dev/null
source "$PROVISION_LIB"

# Count hook commands (across all types/matchers) whose command carries the
# machine-level marker for a given hook script name.
count_marker() {
    local file="$1" name="$2"
    jq --arg m "defaults/hooks/$name" '
        [ (.hooks // {}) | to_entries[] | .value[]? | .hooks[]? | .command // "" | select(contains($m)) ] | length
    ' "$file" 2>/dev/null
}

# ── Test 1: fresh provision into a MISSING settings.json ─────────────────────
echo "Test 1: fresh provision creates settings.json and wires all Loom hooks"
HOME1=$(mktemp -d)
OUT1=$(mktemp)
provision_loom_hooks "$HOME1/.claude" >"$OUT1" 2>&1
rc=$?
assert_eq "$rc" "0" "fresh provision returns 0"
SET1="$HOME1/.claude/settings.json"
[[ -f "$SET1" ]] && pass "settings.json was created" || fail "settings.json was not created"
jq empty "$SET1" 2>/dev/null && pass "settings.json is valid JSON" || fail "settings.json is not valid JSON"
assert_eq "$(count_marker "$SET1" guard-destructive.sh)" "1" "guard-destructive.sh wired once"
assert_eq "$(count_marker "$SET1" guard-worktree-paths.sh)" "1" "guard-worktree-paths.sh (Edit|Write) wired once"
assert_eq "$(count_marker "$SET1" guard-background-subagents.sh)" "1" "Stop hook wired once"
# The Edit|Write matcher exists (design decision 4 — consumers never had it).
assert_eq "$(jq -r '[.hooks.PreToolUse[]?.matcher] | index("Edit|Write") != null' "$SET1")" "true" "Edit|Write matcher is present"

# ── Test 2: verifiable-globals contract (#4053) ──────────────────────────────
echo "Test 2: provisioning exposes verifiable globals (#4053)"
assert_eq "$PROVISIONED_HOOKS_SETTINGS" "$SET1" "PROVISIONED_HOOKS_SETTINGS points at the settings file"
# No backup on a fresh (missing) file — nothing to preserve.
assert_eq "$PROVISIONED_HOOKS_BACKUP" "" "no backup written when there was no pre-existing file"

# ── Test 3: idempotent re-provision (no duplicates) ──────────────────────────
echo "Test 3: re-provision is idempotent (no duplicate entries)"
provision_loom_hooks "$HOME1/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$SET1" guard-destructive.sh)" "1" "guard-destructive.sh still wired exactly once after re-run"
assert_eq "$(count_marker "$SET1" skill-router.sh)" "1" "skill-router.sh still wired exactly once after re-run"

# ── Test 4: dedup against a REQUOTED pre-existing entry (#4200 lesson) ────────
echo "Test 4: dedup keys on the marker substring, so a requoted entry is not duplicated (#4200)"
HOME4=$(mktemp -d); mkdir -p "$HOME4/.claude"
# A pre-existing Loom entry with DIFFERENT quoting/wrapper but the SAME marker.
cat > "$HOME4/.claude/settings.json" <<'EOF'
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [
        { "type": "command", "command": "sh -c \"exec ${LOOM_HOME}/defaults/hooks/guard-destructive.sh\"" }
      ] }
    ]
  }
}
EOF
provision_loom_hooks "$HOME4/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$HOME4/.claude/settings.json" guard-destructive.sh)" "1" "requoted guard-destructive.sh entry is not duplicated"

# ── Test 5: pre-existing non-Loom hooks + permissions preserved ──────────────
echo "Test 5: operator's non-Loom hooks and permissions are preserved"
HOME5=$(mktemp -d); mkdir -p "$HOME5/.claude"
cat > "$HOME5/.claude/settings.json" <<'EOF'
{
  "hooks": {
    "PreToolUse": [
      { "matcher": "Bash", "hooks": [
        { "type": "command", "command": ".claude/hooks/my-own-guard.sh" }
      ] }
    ]
  },
  "permissions": { "allow": ["Bash(mytool:*)"] }
}
EOF
provision_loom_hooks "$HOME5/.claude" >/dev/null 2>&1
S5="$HOME5/.claude/settings.json"
assert_eq "$(jq -r '[.hooks.PreToolUse[] | .hooks[]? | .command | select(. == ".claude/hooks/my-own-guard.sh")] | length' "$S5")" "1" "operator's own hook preserved"
assert_eq "$(jq -r '.permissions.allow[0]' "$S5")" "Bash(mytool:*)" "operator's permissions preserved"
assert_eq "$(count_marker "$S5" guard-destructive.sh)" "1" "Loom hook added alongside the operator's Bash matcher"

# ── Test 6: invalid existing JSON -> soft-fail, NO write ─────────────────────
echo "Test 6: invalid existing JSON is left untouched (soft-fail)"
HOME6=$(mktemp -d); mkdir -p "$HOME6/.claude"
printf 'this is { not json' > "$HOME6/.claude/settings.json"
before6=$(cat "$HOME6/.claude/settings.json")
OUT6=$(mktemp)
provision_loom_hooks "$HOME6/.claude" >"$OUT6" 2>&1
rc=$?
assert_eq "$rc" "1" "invalid JSON returns 1"
assert_eq "$(cat "$HOME6/.claude/settings.json")" "$before6" "invalid JSON file left byte-identical (no write)"
assert_contains "$(cat "$OUT6")" "not valid JSON" "explains the refusal"

# ── Test 7: a backup is written before the first mutation ─────────────────────
echo "Test 7: a backup file is written before mutating an existing settings.json"
HOME7=$(mktemp -d); mkdir -p "$HOME7/.claude"
echo '{"permissions":{"allow":["Bash(x:*)"]}}' > "$HOME7/.claude/settings.json"
provision_loom_hooks "$HOME7/.claude" >/dev/null 2>&1
backups=$(find "$HOME7/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | wc -l | tr -d ' ')
[[ "$backups" -ge 1 ]] && pass "a timestamped backup was written" || fail "no backup file found"
# The backup holds the ORIGINAL content (pre-mutation).
bfile=$(find "$HOME7/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | head -1)
assert_eq "$(jq -r '.permissions.allow[0]' "$bfile")" "Bash(x:*)" "backup preserves the pre-mutation content"

# ── Test 7b: unchanged settings between runs -> no new backup (#5387) ────────
echo "Test 7b: re-running with unchanged settings does not add a new backup (#5387)"
# HOME7's settings.json was mutated by Test 7's provision call (raw ->
# hooks-wired) — that transition is itself a real content change, so a SECOND
# call legitimately backs up the now-wired state once (its own new backup).
# The interesting assertion is the THIRD call onward: install.sh run
# repeatedly with the settings already stable must stop adding backups.
sleep 1
provision_loom_hooks "$HOME7/.claude" >/dev/null 2>&1
count_before_7b=$(find "$HOME7/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | wc -l | tr -d ' ')
sleep 1  # ensure a distinguishable timestamp would exist if a new backup WERE written
provision_loom_hooks "$HOME7/.claude" >/dev/null 2>&1
count_after_7b=$(find "$HOME7/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | wc -l | tr -d ' ')
assert_eq "$count_after_7b" "$count_before_7b" "no new backup written when settings.json is unchanged since the last backup"

# ── Test 7c: bounded retention across repeated CHANGING runs (#5387) ─────────
echo "Test 7c: backup count stays bounded across repeated settings-changing runs (#5387)"
HOME7C=$(mktemp -d); mkdir -p "$HOME7C/.claude"
i=0
while [[ "$i" -lt 8 ]]; do
    printf '{"permissions":{"allow":["Bash(x%d:*)"]}}\n' "$i" > "$HOME7C/.claude/settings.json"
    provision_loom_hooks "$HOME7C/.claude" >/dev/null 2>&1
    # Re-seed with new, non-Loom content so the NEXT provision sees changed
    # settings again (provision_loom_hooks itself mutates the file with the
    # wired hooks, which would otherwise make every subsequent call a no-op
    # dedup case rather than a genuinely changing run).
    i=$((i + 1))
    sleep 1  # force a distinguishable %Y%m%dT%H%M%SZ timestamp per iteration
done
count_7c=$(find "$HOME7C/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | wc -l | tr -d ' ')
[[ "$count_7c" -le 5 ]] && pass "backup count bounded to retention limit (5) after 8 changing runs (got $count_7c)" \
    || fail "backup count exceeded retention limit after 8 changing runs (got $count_7c)"
[[ "$count_7c" -ge 1 ]] && pass "at least one backup survives pruning" || fail "pruning removed every backup"

# ── Test 8: the wired WRAPPER command behaves correctly ──────────────────────
echo "Test 8: the wired command wrapper — workspace gate, machine exec, transition dedup"
# Build a fake machine checkout whose guard-destructive.sh prints a sentinel.
CHK=$(mktemp -d)
mkdir -p "$CHK/defaults/hooks"
printf '#!/usr/bin/env bash\necho "MACHINE-RAN:$LOOM_PROJECT_ROOT"\nexit 0\n' > "$CHK/defaults/hooks/guard-destructive.sh"
chmod +x "$CHK/defaults/hooks/guard-destructive.sh"
# Extract the actual wired command for guard-destructive.sh from a fresh provision.
HOME8=$(mktemp -d)
provision_loom_hooks "$HOME8/.claude" >/dev/null 2>&1
CMD=$(jq -r '.hooks.PreToolUse[] | select(.matcher=="Bash") | .hooks[] | .command | select(contains("defaults/hooks/guard-destructive.sh"))' "$HOME8/.claude/settings.json" | head -1)
[[ -n "$CMD" ]] && pass "extracted the wired guard-destructive.sh command" || fail "could not extract the wired command"

WOUT=""; WRC=0
run_wrapper() { # $1=cwd -> assigns globals WOUT + WRC in the CURRENT shell
    WOUT=$(cd "$1" && LOOM_HOME="$CHK" bash -c "$CMD" </dev/null 2>/dev/null)
    WRC=$?
}

# 8a: non-Loom repo -> no-op (AC3)
NONLOOM=$(mktemp -d); git -C "$NONLOOM" init -q
run_wrapper "$NONLOOM"
[[ -z "$WOUT" && "$WRC" == "0" ]] && pass "AC3: non-Loom repo -> silent no-op (exit 0)" || fail "AC3: expected silent exit 0, got out='$WOUT' rc=$WRC"

# 8b: Loom workspace (legacy .loom/config.json), no per-repo copy -> machine exec (AC1)
LOOMREPO=$(mktemp -d); git -C "$LOOMREPO" init -q; mkdir -p "$LOOMREPO/.loom"; echo '{}' > "$LOOMREPO/.loom/config.json"
run_wrapper "$LOOMREPO"
assert_contains "$WOUT" "MACHINE-RAN:$LOOMREPO" "AC1: Loom workspace with no copy -> execs the machine hook, LOOM_PROJECT_ROOT set"

# 8c: Loom workspace WITH a per-repo .loom/hooks/ copy but NO project-level
# entry referencing it -> the deferral is now CONDITIONAL (#4806): with
# nothing to defer TO, the wrapper falls through and execs the machine hook
# rather than silently no-op'ing (the zero-guard-hooks bug this issue closes).
mkdir -p "$LOOMREPO/.loom/hooks"
printf '#!/usr/bin/env bash\necho SHOULD-NOT-RUN\n' > "$LOOMREPO/.loom/hooks/guard-destructive.sh"
chmod +x "$LOOMREPO/.loom/hooks/guard-destructive.sh"
run_wrapper "$LOOMREPO"
assert_contains "$WOUT" "MACHINE-RAN:$LOOMREPO" "#4806 AC(a): copies present + no project entry -> machine hook runs (was a zero-guard silent no-op)"

# 8c2: same repo, but NOW the project .claude/settings.json actually
# references the per-repo copy -> the wrapper defers (exactly one fire, no
# double-fire; #4806 AC(b)).
mkdir -p "$LOOMREPO/.claude"
printf '{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh"}]}]}}\n' > "$LOOMREPO/.claude/settings.json"
run_wrapper "$LOOMREPO"
[[ -z "$WOUT" && "$WRC" == "0" ]] && pass "#4806 AC(b): copies present + project entry -> defers to it (machine hook does not double-fire)" || fail "#4806 AC(b): expected silent defer, got out='$WOUT' rc=$WRC"

# 8d: Loom workspace, copies ABSENT -> machine exec runs (#4806 AC(c); also
# covered by 8b above, restated explicitly per the issue's AC wording).
rm -rf "$LOOMREPO/.loom/hooks" "$LOOMREPO/.claude/settings.json"
run_wrapper "$LOOMREPO"
assert_contains "$WOUT" "MACHINE-RAN:$LOOMREPO" "#4806 AC(c): copies absent -> machine hook runs"

# 8e: Loom workspace but machine checkout absent -> no-op (fail-open)
out=$(cd "$LOOMREPO" && LOOM_HOME="/nonexistent/checkout" bash -c "$CMD" </dev/null 2>/dev/null); wrc=$?
[[ -z "$out" && "$wrc" == "0" ]] && pass "machine checkout absent -> silent no-op (exit 0)" || fail "expected silent exit 0 when checkout absent, got out='$out' rc=$wrc"

# 8f: the daemon's own release hooks (2am#4501). A daemon-spawned
# session carries LOOM_HOOKS_HOME (the hooks unpacked from the running
# binary); the wrapper prefers it, and falls back to the checkout when the
# variable is blank or its hook file is missing, so nothing fails open that
# did not before.
REL8=$(mktemp -d)
mkdir -p "$REL8/defaults/hooks"
printf '#!/usr/bin/env bash\necho "RELEASE-RAN:$LOOM_PROJECT_ROOT"\nexit 0\n' > "$REL8/defaults/hooks/guard-destructive.sh"
chmod +x "$REL8/defaults/hooks/guard-destructive.sh"
out=$(cd "$LOOMREPO" && LOOM_HOOKS_HOME="$REL8" LOOM_HOME="$CHK" bash -c "$CMD" </dev/null 2>/dev/null)
assert_contains "$out" "RELEASE-RAN:$LOOMREPO" "2am#4501: LOOM_HOOKS_HOME set -> the daemon's release hook runs, not the checkout's"
out=$(cd "$LOOMREPO" && LOOM_HOOKS_HOME="" LOOM_HOME="$CHK" bash -c "$CMD" </dev/null 2>/dev/null)
assert_contains "$out" "MACHINE-RAN:$LOOMREPO" "2am#4501: LOOM_HOOKS_HOME blank -> the checkout's hook runs"
out=$(cd "$LOOMREPO" && LOOM_HOOKS_HOME="/nonexistent/release" LOOM_HOME="$CHK" bash -c "$CMD" </dev/null 2>/dev/null)
assert_contains "$out" "MACHINE-RAN:$LOOMREPO" "2am#4501: LOOM_HOOKS_HOME without the hook -> falls back to the checkout, never fails open"

# ── Test 9: deprovision removes only Loom-owned entries ──────────────────────
echo "Test 9: deprovision strips Loom-owned entries, preserves operator hooks"
HOME9=$(mktemp -d); mkdir -p "$HOME9/.claude"
cat > "$HOME9/.claude/settings.json" <<'EOF'
{ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [
  { "type": "command", "command": ".claude/hooks/my-own-guard.sh" }
] } ] }, "permissions": { "allow": ["Bash(x:*)"] } }
EOF
provision_loom_hooks "$HOME9/.claude" >/dev/null 2>&1
deprovision_loom_hooks "$HOME9/.claude" >/dev/null 2>&1
S9="$HOME9/.claude/settings.json"
assert_eq "$(count_marker "$S9" guard-destructive.sh)" "0" "deprovision removed the Loom hook entries"
assert_eq "$(jq -r '[.hooks.PreToolUse[]? | .hooks[]? | .command | select(. == ".claude/hooks/my-own-guard.sh")] | length' "$S9")" "1" "operator's own hook preserved after deprovision"
assert_eq "$(jq -r '.permissions.allow[0]' "$S9")" "Bash(x:*)" "operator's permissions preserved after deprovision"

# ── Test 9b: deprovision's backups are also deduped (#5387 edge case) ────────
echo "Test 9b: deprovision does not write a duplicate backup for repeated byte-identical settings (#5387)"
HOME9B=$(mktemp -d); mkdir -p "$HOME9B/.claude"
SETTINGS_9B='{ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [
  { "type": "command", "command": "/machine/checkout/defaults/hooks/guard-destructive.sh" }
] } ] }, "permissions": { "allow": ["Bash(x:*)"] } }'
echo "$SETTINGS_9B" > "$HOME9B/.claude/settings.json"
deprovision_loom_hooks "$HOME9B/.claude" >/dev/null 2>&1
count_before_9b=$(find "$HOME9B/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | wc -l | tr -d ' ')
[[ "$count_before_9b" -ge 1 ]] && pass "deprovision wrote a backup before its first mutation" || fail "deprovision did not write a backup"
sleep 1  # ensure a distinguishable timestamp would exist if a new backup WERE (wrongly) written
# Restore the SAME pre-mutation content and run deprovision again — it still
# has Loom entries to strip (so it does not early-return before reaching the
# backup step), but the content is byte-identical to what the last backup
# already captured.
echo "$SETTINGS_9B" > "$HOME9B/.claude/settings.json"
deprovision_loom_hooks "$HOME9B/.claude" >/dev/null 2>&1
count_after_9b=$(find "$HOME9B/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | wc -l | tr -d ' ')
assert_eq "$count_after_9b" "$count_before_9b" "deprovision does not write a duplicate backup when the pre-mutation content repeats"

# ── Test 9c: deprovision's backups are also bounded (#5387 edge case) ────────
echo "Test 9c: deprovision backup count stays bounded across repeated changing runs (#5387)"
HOME9C=$(mktemp -d); mkdir -p "$HOME9C/.claude"
i=0
while [[ "$i" -lt 8 ]]; do
    cat > "$HOME9C/.claude/settings.json" <<EOF
{ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [
  { "type": "command", "command": "/machine/checkout/defaults/hooks/guard-destructive.sh" }
] } ] }, "permissions": { "allow": ["Bash(x${i}:*)"] } }
EOF
    deprovision_loom_hooks "$HOME9C/.claude" >/dev/null 2>&1
    i=$((i + 1))
    sleep 1  # force a distinguishable %Y%m%dT%H%M%SZ timestamp per iteration
done
count_9c=$(find "$HOME9C/.claude" -maxdepth 1 -name 'settings.json.loom-backup-*' | wc -l | tr -d ' ')
[[ "$count_9c" -le 5 ]] && pass "deprovision backup count bounded to retention limit (5) after 8 changing runs (got $count_9c)" \
    || fail "deprovision backup count exceeded retention limit after 8 changing runs (got $count_9c)"

# ─────────────────────────────────────────────────────────────────────────────
# ensure_project_hook_wiring — the project-level fallback (#4401)
#
# Guards the exact zero-coverage state reported in #4401: a repo that still
# carries per-repo `.loom/hooks/` copies (so the user-scope wrapper defers) but
# whose project-level `.claude/settings.json` entries were stripped (by the
# 0.16.0 Phase-5 defaults / a --confirm-reinstall's chained uninstall).
# ─────────────────────────────────────────────────────────────────────────────

# Count project-level hook commands referencing a given hook script name.
count_project_entry() {
    local file="$1" name="$2"
    jq --arg m ".loom/hooks/$name" '
        [ (.hooks // {}) | to_entries[] | .value[]? | .hooks[]? | .command // ""
          | select(contains($m)) | select(contains("defaults/hooks/") | not) ] | length
    ' "$file" 2>/dev/null
}

# Project-level commands that are NOT the launcher form (#15741): every
# `.loom/hooks/<name>` entry must name hook-wiring.sh, the broken-install
# signal, and `exec bash "$L" <its own event> <name>`. Prints them, one per
# line, so an empty output means every entry conforms.
unlaunched_commands() {
    local file="$1"
    jq -r '
        (.hooks // {}) | to_entries[] | .key as $event
        | .value[]? | .hooks[]? | .command // ""
        | select(contains(".loom/hooks/")) | select(contains("defaults/hooks/") | not)
        | [ ., ([ match("\\.loom/hooks/(?<n>[A-Za-z0-9._-]+\\.sh)"; "g") | .captures[0].string
                 | select(. != "hook-wiring.sh") ][0] // "") ]
        | select((.[0] | startswith("bash -c \u0027") and endswith("\u0027")
                  and contains("hook-wiring.sh")
                  and contains("BROKEN GUARD INSTALL")
                  and contains("exec bash \"$L\" " + $event + " " + .[1])) | not)
        | .[0]' "$file" 2>/dev/null
}
count_unlaunched() { unlaunched_commands "$1" | grep -c . ; }

# Build a "pre-Phase-6 transition repo" fixture: .loom/hooks/ copies present.
make_transition_repo() {
    local root="$1"
    mkdir -p "$root/.loom/hooks" "$root/.claude"
    local n
    for n in guard-destructive.sh guard-loom-workflow.sh guard-worktree-paths.sh \
             skill-router.sh methodology-inject.sh guard-background-subagents.sh; do
        printf '#!/usr/bin/env bash\nexit 0\n' > "$root/.loom/hooks/$n"
        chmod +x "$root/.loom/hooks/$n"
    done
}

# ── Test 10: the #4401 repro — copies present, hooks block stripped ──────────
echo "Test 10: #4401 repro — per-repo copies + stripped project hooks -> entries restored"
R10=$(mktemp -d)
make_transition_repo "$R10"
# The post-uninstall / post-init state: 0.16.0 defaults settings.json, i.e.
# permissions only and NO `hooks` key at all.
printf '{"permissions":{"allow":["Bash(gh:*)"]}}\n' > "$R10/.claude/settings.json"
assert_eq "$(jq -r 'has("hooks")' "$R10/.claude/settings.json")" "false" "precondition: zero guard-hook coverage (no hooks key)"
# Invoked in the CURRENT shell (not a command substitution) so the verifiable
# globals it publishes survive for the assertions below.
ensure_project_hook_wiring "$R10" >/dev/null 2>&1; rc=$?
assert_eq "$rc" "0" "ensure_project_hook_wiring returns 0 on a transition repo"
S10="$R10/.claude/settings.json"
assert_eq "$(count_project_entry "$S10" guard-destructive.sh)" "1" "guard-destructive.sh reachable via a project-level entry"
assert_eq "$(count_project_entry "$S10" guard-loom-workflow.sh)" "1" "guard-loom-workflow.sh reachable"
assert_eq "$(count_project_entry "$S10" guard-worktree-paths.sh)" "1" "guard-worktree-paths.sh (Edit|Write) reachable"
assert_eq "$(count_project_entry "$S10" skill-router.sh)" "1" "skill-router.sh reachable"
assert_eq "$(count_project_entry "$S10" methodology-inject.sh)" "1" "methodology-inject.sh reachable"
assert_eq "$(count_project_entry "$S10" guard-background-subagents.sh)" "1" "guard-background-subagents.sh (Stop) reachable"
assert_eq "$PROJECT_HOOKS_WIRED" "6" "PROJECT_HOOKS_WIRED reports all six copies (#4053 verifiable-globals contract)"
assert_eq "$PROJECT_HOOKS_SETTINGS" "$S10" "PROJECT_HOOKS_SETTINGS points at the project settings file"
assert_eq "$(jq -r '.permissions.allow[0]' "$S10")" "Bash(gh:*)" "existing project permissions preserved"
# Every written command is the LAUNCHER form (#15741): it routes its own event
# and script through hook-wiring.sh and signals a broken install. The bare
# quoted path it replaces carried neither, and #6544's quoting is now moot —
# the command never embeds the project path as text (Test 22 runs it from a
# path with a space).
assert_eq "$(count_unlaunched "$S10")" "0" "every written command routes its event and script through hook-wiring.sh and signals a broken install (#15741)"
assert_eq "$(jq -r '[(.hooks // {}) | to_entries[] | .value[]? | .hooks[]? | .command | select(startswith("\"${CLAUDE_PROJECT_DIR}/.loom/hooks/"))] | length' "$S10")" "0" "no written command is the bare quoted path (#15741)"
assert_eq "$(jq -r '[.hooks.PreToolUse[] | select(.matcher=="Bash") | .hooks[] | .command | select(contains(".loom/hooks/guard-destructive.sh"))][0]' "$S10")" \
    "$(_phook_project_cmd PreToolUse guard-destructive.sh)" "the written command is exactly _phook_project_cmd's output"
assert_eq "$(jq -r '[.hooks.Stop[] | .hooks[] | .command | select(contains(".loom/hooks/guard-background-subagents.sh"))][0]' "$S10")" \
    "$(_phook_project_cmd Stop guard-background-subagents.sh)" "a non-PreToolUse entry gets its own event's launcher command"
# The referenced script must exist — a dangling entry is not coverage.
for n in guard-destructive.sh guard-background-subagents.sh; do
    [[ -x "$R10/.loom/hooks/$n" ]] && pass "wired entry for $n points at an executable copy" \
        || fail "wired entry for $n points at a missing/non-executable copy"
done

# ── Test 11: idempotent — a second install must not duplicate entries ────────
echo "Test 11: re-running is idempotent (no duplicate project-level entries)"
ensure_project_hook_wiring "$R10" >/dev/null 2>&1
assert_eq "$(count_project_entry "$S10" guard-destructive.sh)" "1" "guard-destructive.sh still exactly one entry"
assert_eq "$(count_project_entry "$S10" skill-router.sh)" "1" "skill-router.sh still exactly one entry"

# ── Test 12: legacy bare-relative entries are recognized, not duplicated ─────
echo "Test 12: a legacy pre-#3277 bare-relative entry is not duplicated"
R12=$(mktemp -d)
make_transition_repo "$R12"
cat > "$R12/.claude/settings.json" <<'EOF'
{ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [
  { "type": "command", "command": ".loom/hooks/guard-destructive.sh" }
] } ] } }
EOF
ensure_project_hook_wiring "$R12" >/dev/null 2>&1
assert_eq "$(count_project_entry "$R12/.claude/settings.json" guard-destructive.sh)" "1" "legacy bare-relative entry recognized (no duplicate added)"
assert_eq "$(count_project_entry "$R12/.claude/settings.json" guard-loom-workflow.sh)" "1" "the missing sibling entry was still added"
assert_eq "$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$R12/.claude/settings.json")" '.loom/hooks/guard-destructive.sh' "the bare-relative entry is recognized but NOT rewritten — it is not a form Loom wrote (#15741)"

# ── Test 13: post-Phase-6 (migrated, copy-free) repo -> no-op ────────────────
echo "Test 13: a migrated repo with no .loom/hooks/ copies is left alone"
R13=$(mktemp -d)
mkdir -p "$R13/.loom" "$R13/.claude"
printf '{"permissions":{"allow":["Bash(gh:*)"]}}\n' > "$R13/.claude/settings.json"
BEFORE13=$(cat "$R13/.claude/settings.json")
LOG13=$(mktemp)
ensure_project_hook_wiring "$R13" >"$LOG13" 2>&1; rc=$?
OUT13=$(cat "$LOG13")
assert_eq "$rc" "0" "copy-free repo returns 0"
assert_eq "$(cat "$R13/.claude/settings.json")" "$BEFORE13" "copy-free repo settings.json left byte-identical"
assert_eq "$PROJECT_HOOKS_WIRED" "0" "PROJECT_HOOKS_WIRED is 0 on a copy-free repo"
assert_contains "$OUT13" "machine checkout" "explains that guards run from the machine checkout"

# ── Test 14: only hooks whose copy exists get an entry (no dangling entries) ──
echo "Test 14: a hook with no per-repo copy gets no project-level entry"
R14=$(mktemp -d)
make_transition_repo "$R14"
rm -f "$R14/.loom/hooks/skill-router.sh"
printf '{}\n' > "$R14/.claude/settings.json"
ensure_project_hook_wiring "$R14" >/dev/null 2>&1
assert_eq "$(count_project_entry "$R14/.claude/settings.json" skill-router.sh)" "0" "no entry written for the absent skill-router.sh copy"
assert_eq "$(count_project_entry "$R14/.claude/settings.json" guard-destructive.sh)" "1" "present copies still wired"
assert_eq "$PROJECT_HOOKS_WIRED" "5" "PROJECT_HOOKS_WIRED counts only present copies"

# ── Test 15: operator's own project hooks are preserved ──────────────────────
echo "Test 15: an operator's own project-level hooks survive"
R15=$(mktemp -d)
make_transition_repo "$R15"
cat > "$R15/.claude/settings.json" <<'EOF'
{ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [
  { "type": "command", "command": ".claude/hooks/my-own-guard.sh" }
] } ] }, "permissions": { "allow": ["Bash(x:*)"] } }
EOF
ensure_project_hook_wiring "$R15" >/dev/null 2>&1
assert_eq "$(jq -r '[.hooks.PreToolUse[]? | .hooks[]? | .command | select(. == ".claude/hooks/my-own-guard.sh")] | length' "$R15/.claude/settings.json")" "1" "operator's own project hook preserved"
assert_eq "$(count_project_entry "$R15/.claude/settings.json" guard-destructive.sh)" "1" "Loom entry added alongside it"

# ── Test 16: invalid existing JSON -> soft-fail, no write ───────────────────
echo "Test 16: invalid project settings.json is left untouched (soft-fail)"
R16=$(mktemp -d)
make_transition_repo "$R16"
printf '{ this is not json' > "$R16/.claude/settings.json"
BEFORE16=$(cat "$R16/.claude/settings.json")
LOG16=$(mktemp)
ensure_project_hook_wiring "$R16" >"$LOG16" 2>&1; rc=$?
OUT16=$(cat "$LOG16")
assert_eq "$rc" "1" "invalid JSON returns 1"
assert_eq "$(cat "$R16/.claude/settings.json")" "$BEFORE16" "invalid JSON left byte-identical (no write)"
assert_contains "$OUT16" "not valid JSON" "explains the refusal"

# ── Test 17: a missing .claude/settings.json is created ─────────────────────
echo "Test 17: a missing project settings.json is created with the entries"
R17=$(mktemp -d)
make_transition_repo "$R17"
rm -rf "$R17/.claude"
ensure_project_hook_wiring "$R17" >/dev/null 2>&1
[[ -f "$R17/.claude/settings.json" ]] && pass "settings.json created" || fail "settings.json not created"
assert_eq "$(count_project_entry "$R17/.claude/settings.json" guard-destructive.sh)" "1" "entry written into the new file"

# ── Test 18: exactly ONE path fires — no double-fire, no zero-coverage ──────
echo "Test 18: user-scope + project-level compose to exactly one execution path"
# A transition repo wired BOTH ways (what a --quick install now produces).
R18=$(mktemp -d); git -C "$R18" init -q
make_transition_repo "$R18"
mkdir -p "$R18/.loom"; echo '{}' > "$R18/.loom/config.json"
printf '{}\n' > "$R18/.claude/settings.json"
HOME18=$(mktemp -d)
CHK18=$(mktemp -d); mkdir -p "$CHK18/defaults/hooks"
printf '#!/usr/bin/env bash\necho MACHINE-RAN\nexit 0\n' > "$CHK18/defaults/hooks/guard-destructive.sh"
chmod +x "$CHK18/defaults/hooks/guard-destructive.sh"
provision_loom_hooks "$HOME18/.claude" >/dev/null 2>&1
ensure_project_hook_wiring "$R18" >/dev/null 2>&1
# Project-level entry present AND the machine wrapper defers to the copy.
assert_eq "$(count_project_entry "$R18/.claude/settings.json" guard-destructive.sh)" "1" "project-level entry present (the live path)"
CMD18=$(jq -r '.hooks.PreToolUse[] | select(.matcher=="Bash") | .hooks[] | .command | select(contains("defaults/hooks/guard-destructive.sh"))' "$HOME18/.claude/settings.json" | head -1)
OUT18=$(cd "$R18" && LOOM_HOME="$CHK18" bash -c "$CMD18" </dev/null 2>/dev/null); rc18=$?
[[ -z "$OUT18" && "$rc18" == "0" ]] && pass "user-scope wrapper defers (no double-fire) while the copy exists" \
    || fail "expected the user-scope wrapper to defer, got out='$OUT18' rc=$rc18"
# After a Phase-6 migration removes the copies, the machine path takes over —
# so coverage is never zero in EITHER configuration.
rm -rf "$R18/.loom/hooks"
OUT18b=$(cd "$R18" && LOOM_HOME="$CHK18" bash -c "$CMD18" </dev/null 2>/dev/null)
assert_contains "$OUT18b" "MACHINE-RAN" "once the copies are gone, the machine-checkout hook runs (coverage never zero)"

# ─────────────────────────────────────────────────────────────────────────────
# Stale-Loom-wrapper UPGRADE path (#4806) — re-provisioning an install that
# carries an OLDER Loom-authored wrapper (predating a `_phook_cmd()` edit)
# must REWRITE it in place, since the dedup marker (`defaults/hooks/<name>`)
# matches regardless of wrapper version and would otherwise cause a naive
# re-provision to skip it forever.
# ─────────────────────────────────────────────────────────────────────────────

# An "older" wrapper: same overall shape (the recognizable
# `ROOT=$(cd "$(git rev-parse --git-common-dir` prefix + the
# `defaults/hooks/<name>` marker) but with the UNCONDITIONAL transition-dedup
# step this issue replaces (no `.claude/settings.json` check before deferring).
OLD_WRAPPER_CMD='bash -c '"'"'ROOT=$(cd "$(git rev-parse --git-common-dir 2>/dev/null)/.." 2>/dev/null && pwd); [ -n "$ROOT" ] || exit 0; { [ -f "$ROOT/.loom-project/project.json" ] || [ -f "$ROOT/.loom/config.json" ]; } || exit 0; [ -x "$ROOT/.loom/hooks/guard-destructive.sh" ] && exit 0; H="${LOOM_HOME:-$HOME/.local/share/loom}/defaults/hooks/guard-destructive.sh"; [ -x "$H" ] && LOOM_PROJECT_ROOT="$ROOT" exec "$H" || exit 0'"'"''

# ── Test 19: re-provisioning REPLACES a stale Loom-authored wrapper ──────────
echo "Test 19: re-provisioning an install with an OLDER Loom wrapper replaces it in place (#4806)"
HOME19=$(mktemp -d); mkdir -p "$HOME19/.claude"
jq -n --arg cmd "$OLD_WRAPPER_CMD" \
    '{hooks:{PreToolUse:[{matcher:"Bash",hooks:[{type:"command",command:$cmd}]}]}}' \
    > "$HOME19/.claude/settings.json"
BEFORE_CMD19=$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$HOME19/.claude/settings.json")
assert_eq "$BEFORE_CMD19" "$OLD_WRAPPER_CMD" "precondition: the stale wrapper is seeded verbatim"
provision_loom_hooks "$HOME19/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$HOME19/.claude/settings.json" guard-destructive.sh)" "1" "still exactly one guard-destructive.sh entry after upgrade (no duplicate)"
AFTER_CMD19=$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$HOME19/.claude/settings.json")
CURRENT_CMD19="$(_phook_cmd guard-destructive.sh)"
assert_eq "$AFTER_CMD19" "$CURRENT_CMD19" "the stale entry was rewritten to the CURRENT _phook_cmd() output"
[[ "$AFTER_CMD19" != "$OLD_WRAPPER_CMD" ]] && pass "the old unconditional-defer text is gone" || fail "the old wrapper text is still present — upgrade did not fire"
# Idempotent: a second re-provision must not touch it again (it now matches).
provision_loom_hooks "$HOME19/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$HOME19/.claude/settings.json" guard-destructive.sh)" "1" "still exactly one entry after a second re-provision"
assert_eq "$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$HOME19/.claude/settings.json")" "$CURRENT_CMD19" "already-current entry left unchanged by a second re-provision"

# ── Test 20: a hand-written entry sharing the marker is NEVER rewritten ──────
echo "Test 20: a non-Loom / hand-written entry is never rewritten or removed by the upgrade path (#4806)"
HOME20=$(mktemp -d); mkdir -p "$HOME20/.claude"
HANDWRITTEN_CMD='.claude/hooks/my-custom-wrapper.sh --marker defaults/hooks/guard-destructive.sh'
jq -n --arg cmd "$HANDWRITTEN_CMD" \
    '{hooks:{PreToolUse:[{matcher:"Bash",hooks:[{type:"command",command:$cmd}]}]}}' \
    > "$HOME20/.claude/settings.json"
provision_loom_hooks "$HOME20/.claude" >/dev/null 2>&1
assert_eq "$(jq -r '[.hooks.PreToolUse[0].hooks[] | select(.command == $h)] | length' --arg h "$HANDWRITTEN_CMD" "$HOME20/.claude/settings.json")" "1" "hand-written entry is byte-identical after provisioning (never rewritten)"
# Its shape does not match the known Loom wrapper prefix, so the dedup test
# treats the marker match as satisfied and does NOT append a second (Loom)
# entry either — same no-duplicate contract as before this issue.
assert_eq "$(count_marker "$HOME20/.claude/settings.json" guard-destructive.sh)" "1" "no second (Loom) entry appended alongside the hand-written one"

# ── Test 21: the #7761 project wiring is recognized, not duplicated ──────────
#
# ensure_project_hook_wiring dedups on `.loom/hooks/<name>` while EXCLUDING any
# command containing `defaults/hooks/` (that substring marks the MACHINE-level
# wrapper, which embeds `$ROOT/.loom/hooks/<name>` in its own dedup probe). The
# #7761 project wiring therefore must not carry a `defaults/hooks/` substring —
# if it ever does, the exclude test misreads it as a machine-level entry and
# APPENDS a second, bare project entry, double-firing every guard. That
# constraint is invisible from the settings file itself, so it is pinned here.
echo "Test 21: the #7761 project-level wiring is deduped, never duplicated (#7761)"
if [[ -f "$REPO_ROOT/.claude/settings.json" ]]; then
    TARGET21=$(mktemp -d); mkdir -p "$TARGET21/.loom/hooks" "$TARGET21/.claude"
    for n in guard-destructive.sh guard-loom-workflow.sh guard-worktree-paths.sh \
             skill-router.sh methodology-inject.sh guard-background-subagents.sh hook-wiring.sh; do
        printf '#!/bin/sh\nexit 0\n' > "$TARGET21/.loom/hooks/$n"
        chmod +x "$TARGET21/.loom/hooks/$n"
    done
    cp "$REPO_ROOT/.claude/settings.json" "$TARGET21/.claude/settings.json"
    ensure_project_hook_wiring "$TARGET21" >/dev/null 2>&1 || true
    for n in guard-destructive.sh guard-loom-workflow.sh guard-worktree-paths.sh \
             skill-router.sh guard-background-subagents.sh; do
        assert_eq "$(jq -r --arg n "$n" \
            '[.. | objects | .command? // empty] | map(select(contains(".loom/hooks/" + $n))) | length' \
            "$TARGET21/.claude/settings.json")" "1" \
            "exactly one project entry for $n after re-asserting project wiring"
    done
    assert_eq "$(jq -r '[.. | objects | .command? // empty] | map(select(contains("defaults/hooks/"))) | length' \
        "$REPO_ROOT/.claude/settings.json")" "0" \
        "the project wiring carries no 'defaults/hooks/' substring (would defeat the dedup exclude)"
    rm -rf "$TARGET21"
else
    echo "  (skipped: no $REPO_ROOT/.claude/settings.json)"
fi

# ─────────────────────────────────────────────────────────────────────────────
# Project paths containing a space (#6544). A project entry used to BE the
# project path as text — `${CLAUDE_PROJECT_DIR}/.loom/hooks/<name>`, which `sh`
# word-split on any space until #6544 quoted it. The launcher command (#15741)
# never embeds the path: it resolves the root inside its own `bash -c` and
# quotes every use of it.
# ─────────────────────────────────────────────────────────────────────────────

# ── Test 22: fresh provision into a spaced path survives execution (#6544) ──
echo "Test 22: fresh provision into a project path containing a space survives sh -c execution (#6544)"
R22_PARENT=$(mktemp -d)
R22="$R22_PARENT/has a space"
mkdir -p "$R22"
make_transition_repo "$R22"
printf '{}\n' > "$R22/.claude/settings.json"
ensure_project_hook_wiring "$R22" >/dev/null 2>&1
S22="$R22/.claude/settings.json"
CMD22=$(jq -r '[(.hooks // {}) | to_entries[] | .value[]? | .hooks[]? | .command | select(contains("guard-destructive.sh"))][0]' "$S22")
assert_eq "$CMD22" "$(_phook_project_cmd PreToolUse guard-destructive.sh)" "emitted command is the launcher form"
# Run it the way Claude Code does: from inside the project, CLAUDE_PROJECT_DIR
# exported. No ${CLAUDE_PROJECT_DIR} text substitution is simulated — the
# command no longer carries the project path as text.
OUT22=$(cd "$R22" && CLAUDE_PROJECT_DIR="$R22" sh -c "$CMD22" </dev/null 2>&1); rc22=$?
assert_eq "$rc22" "0" "the command survives sh -c execution against a spaced path"
assert_eq "$OUT22" "" "no word-split error output from the command"
# ...and with no CLAUDE_PROJECT_DIR at all, run from the project's own cwd:
# $PWD is the last resort of the command's root resolution.
OUT22=$(cd "$R22" && env -u CLAUDE_PROJECT_DIR sh -c "$CMD22" </dev/null 2>&1); rc22=$?
assert_eq "$rc22|$OUT22" "0|" "the command also runs from a spaced cwd with no CLAUDE_PROJECT_DIR"
# Negative control: confirm the pre-#6544 UNQUOTED path-as-text form really did
# break on the same path (pins that bug rather than assuming it).
LEGACY22='${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh'
LEGACY_LITERAL22=${LEGACY22//\$\{CLAUDE_PROJECT_DIR\}/$R22}
sh -c "$LEGACY_LITERAL22" >/dev/null 2>&1
rc22_legacy=$?
[[ "$rc22_legacy" != "0" ]] && pass "the pre-#6544 unquoted form fails on the same spaced path (regression pinned)" \
    || fail "expected the unquoted form to fail on a spaced path (word-split), it did not"

# ── Test 23: a pre-existing UNQUOTED entry self-heals (#6544) ────────────────
echo "Test 23: re-provisioning over a pre-existing UNQUOTED entry rewrites it to the launcher form (#6544, #15741)"
R23=$(mktemp -d)
make_transition_repo "$R23"
cat > "$R23/.claude/settings.json" <<'EOF'
{ "hooks": { "PreToolUse": [ { "matcher": "Bash", "hooks": [
  { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh" }
] } ] } }
EOF
S23="$R23/.claude/settings.json"
BEFORE_CMD23=$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$S23")
assert_eq "$BEFORE_CMD23" '${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh' "precondition: the unquoted legacy entry is seeded verbatim"
ensure_project_hook_wiring "$R23" >/dev/null 2>&1
AFTER_CMD23=$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$S23")
assert_eq "$AFTER_CMD23" "$(_phook_project_cmd PreToolUse guard-destructive.sh)" "the unquoted entry was rewritten to the launcher form (migration, not a no-op)"
assert_eq "$(count_project_entry "$S23" guard-destructive.sh)" "1" "rewritten in place — still exactly one entry, not duplicated"
assert_eq "$(count_project_entry "$S23" guard-loom-workflow.sh)" "1" "the missing sibling entry was also added in the same pass"
# Idempotent: a second re-provision leaves the now-current launcher entry alone.
ensure_project_hook_wiring "$R23" >/dev/null 2>&1
assert_eq "$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$S23")" "$(_phook_project_cmd PreToolUse guard-destructive.sh)" "already-current entry left unchanged by a second re-provision"
assert_eq "$(count_project_entry "$S23" guard-destructive.sh)" "1" "still exactly one entry after a second re-provision"

# ── Test 24: the machine-level (user-scope) wrapper path is unaffected ───────
echo "Test 24: the machine-level ~/.claude/settings.json wrapper is unaffected by the project-level quoting fix (#6544)"
HOME24=$(mktemp -d)
provision_loom_hooks "$HOME24/.claude" >/dev/null 2>&1
CMD24=$(jq -r '.hooks.PreToolUse[] | select(.matcher=="Bash") | .hooks[] | .command | select(contains("guard-destructive.sh"))' "$HOME24/.claude/settings.json" | head -1)
assert_contains "$CMD24" "bash -c '" "machine-level entry is still the bash -c wrapper form"
[[ "$CMD24" != *'"${CLAUDE_PROJECT_DIR}'* ]] && pass "machine-level wrapper does not use the project-level \${CLAUDE_PROJECT_DIR} quoting form at all" \
    || fail "machine-level wrapper unexpectedly picked up the project-level quoting form"

# ── Test 25: the SessionStart gh front (#10516) ──────────────────────────────
echo "Test 25: SessionStart gh-front-env.sh — wired once, operator entries kept, removed by deprovision, stub fail-open + stdout-silent (#10516)"
HOME25=$(mktemp -d); mkdir -p "$HOME25/.claude"
cat > "$HOME25/.claude/settings.json" <<'EOF'
{ "hooks": { "SessionStart": [ { "matcher": "startup", "hooks": [
  { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.claude/skills/repo/hooks/session-start-handoff.sh" }
] } ] } }
EOF
S25="$HOME25/.claude/settings.json"
provision_loom_hooks "$HOME25/.claude" >/dev/null 2>&1
provision_loom_hooks "$HOME25/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$S25" gh-front-env.sh)" "1" "gh-front-env.sh wired exactly once after two provisions"
assert_eq "$(jq -r '[.hooks.SessionStart[] | select(any(.hooks[]?; .command | contains("defaults/hooks/gh-front-env.sh"))) | .matcher] == [""]' "$S25")" "true" "wired under matcher \"\" (startup, resume, clear, compact)"
HANDOFF25='[.hooks.SessionStart[]? | .hooks[]? | .command | select(contains("session-start-handoff.sh"))] | length'
assert_eq "$(jq -r "$HANDOFF25" "$S25")" "1" "operator's own SessionStart entry preserved"
deprovision_loom_hooks "$HOME25/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$S25" gh-front-env.sh)" "0" "deprovision removed the SessionStart entry"
assert_eq "$(jq -r "$HANDOFF25" "$S25")" "1" "operator's SessionStart entry survives deprovision"
# The stub itself: whatever the daemon prints, the hook prints
# nothing on stdout (SessionStart stdout reaches the model) and exits 0.
CHK25=$(mktemp -d); mkdir -p "$CHK25/defaults/hooks"
cp "$REPO_ROOT/defaults/hooks/gh-front-env.sh" "$CHK25/defaults/hooks/"
ln -s "$REPO_ROOT/defaults/scripts" "$CHK25/defaults/scripts"
FAKE25="$CHK25/fake-daemon"
ARGV25="$CHK25/argv"
# A fake daemon: `--version` answers the preflight (FAKEVER25), anything else
# is logged with LOOM_PROJECT_ROOT and prints stdout noise the hook must swallow.
printf '#!/bin/sh\n[ "$1" = --version ] && { echo "loom-daemon $FAKEVER25"; exit 0; }\necho "$*|$LOOM_PROJECT_ROOT" >> "%s"\necho STDOUT-NOISE\n' "$ARGV25" > "$FAKE25"
chmod +x "$FAKE25"
WS25=$(mktemp -d)
out=$(cd "$WS25" && FAKEVER25=99.0.0 LOOM_DAEMON_SELF_BIN="$FAKE25" LOOM_PROJECT_ROOT="$WS25" bash "$CHK25/defaults/hooks/gh-front-env.sh" </dev/null 2>/dev/null); rc=$?
assert_eq "$out|$rc" "|0" "stub is stdout-silent and exits 0 even when the daemon prints on stdout"
assert_eq "$(cat "$ARGV25" 2>/dev/null)" "gh-shim session-env|$WS25" "stub runs \`gh-shim session-env\` with LOOM_PROJECT_ROOT"
: > "$ARGV25"
out=$(cd "$WS25" && FAKEVER25=0.1.0 LOOM_DAEMON_SELF_BIN="$FAKE25" bash "$CHK25/defaults/hooks/gh-front-env.sh" </dev/null 2>/dev/null); rc=$?
assert_eq "$out|$rc|$(cat "$ARGV25")" "|0|" "a daemon below the declared floor is refused by the preflight: exit 0, never called"
NOLIB25=$(mktemp -d); cp "$REPO_ROOT/defaults/hooks/gh-front-env.sh" "$NOLIB25/"
out=$(LOOM_DAEMON_SELF_BIN="$FAKE25" bash "$NOLIB25/gh-front-env.sh" </dev/null 2>&1); rc=$?
assert_eq "$out|$rc" "|0" "no daemon library -> silent exit 0"
# Through the wired wrapper: a non-Loom repo never reaches the stub.
CMD25=$(_phook_cmd gh-front-env.sh)
NONLOOM25=$(mktemp -d); git -C "$NONLOOM25" init -q
: > "$ARGV25"
out=$(cd "$NONLOOM25" && LOOM_HOME="$CHK25" LOOM_DAEMON_SELF_BIN="$FAKE25" bash -c "$CMD25" </dev/null 2>&1); rc=$?
assert_eq "$out|$rc|$(cat "$ARGV25")" "|0|" "non-Loom repo: the wrapper never runs the stub (PATH untouched)"

# ── Test 26: guard-uncommitted-work.sh on Stop AND SubagentStop (#8372) ─────
echo "Test 26: uncommitted-work consumer canary stub — both events, both install modes, dedup, deprovision (#8372)"
# Count entries for <name> under ONE hook event type (machine marker).
count_marker_in() {
    local file="$1" ev="$2" name="$3"
    jq --arg ev "$ev" --arg m "defaults/hooks/$name" '
        [ (.hooks[$ev] // [])[] | .hooks[]? | .command // "" | select(contains($m)) ] | length
    ' "$file" 2>/dev/null
}
count_project_in() {
    local file="$1" ev="$2" name="$3"
    jq --arg ev "$ev" --arg m ".loom/hooks/$name" '
        [ (.hooks[$ev] // [])[] | .hooks[]? | .command // ""
          | select(contains($m)) | select(contains("defaults/hooks/") | not) ] | length
    ' "$file" 2>/dev/null
}
UW=guard-uncommitted-work.sh

# 26a: user-scope (machine) provisioning — twice — wires each event exactly once.
HOME26=$(mktemp -d)
provision_loom_hooks "$HOME26/.claude" >/dev/null 2>&1
provision_loom_hooks "$HOME26/.claude" >/dev/null 2>&1
S26="$HOME26/.claude/settings.json"
assert_eq "$(count_marker_in "$S26" Stop "$UW")" "1" "user-scope: Stop entry wired exactly once after two provisions"
assert_eq "$(count_marker_in "$S26" SubagentStop "$UW")" "1" "user-scope: SubagentStop entry wired exactly once after two provisions"
assert_eq "$(count_marker_in "$S26" Stop guard-background-subagents.sh)" "1" "user-scope: the existing Stop guard is still wired once alongside it"
assert_eq "$(count_marker "$S26" "$UW")" "2" "user-scope: two entries total (one per event), never more"

# 26b: project-copy (quick-install) fallback wires both events, idempotently.
R26=$(mktemp -d); git -C "$R26" init -q
make_transition_repo "$R26"
mkdir -p "$R26/.loom/scripts/lib"
cp "$REPO_ROOT/defaults/hooks/$UW" "$R26/.loom/hooks/$UW"; chmod +x "$R26/.loom/hooks/$UW"
printf '{"guards":{"uncommittedWorkConsumerCanary":true}}\n' > "$R26/.loom/config.json"
printf '{}\n' > "$R26/.claude/settings.json"
ensure_project_hook_wiring "$R26" >/dev/null 2>&1
ensure_project_hook_wiring "$R26" >/dev/null 2>&1
P26="$R26/.claude/settings.json"
assert_eq "$(count_project_in "$P26" Stop "$UW")" "1" "project-copy: Stop entry asserted exactly once after two runs"
assert_eq "$(count_project_in "$P26" SubagentStop "$UW")" "1" "project-copy: SubagentStop entry asserted exactly once after two runs"

# 26c: transition dedup — the user-scope wrapper defers to the project copy, so
# exactly ONE entry point runs per event in a repo wired both ways.
CHK26=$(mktemp -d); mkdir -p "$CHK26/defaults/hooks"
printf '#!/usr/bin/env bash\necho MACHINE-RAN\nexit 0\n' > "$CHK26/defaults/hooks/$UW"
chmod +x "$CHK26/defaults/hooks/$UW"
for ev in Stop SubagentStop; do
    CMD26=$(jq -r --arg ev "$ev" --arg m "defaults/hooks/$UW" '.hooks[$ev][] | .hooks[] | .command | select(contains($m))' "$S26" | head -1)
    OUT26=$(cd "$R26" && LOOM_HOME="$CHK26" bash -c "$CMD26" </dev/null 2>/dev/null); rc26=$?
    assert_eq "$OUT26|$rc26" "|0" "$ev: user-scope wrapper defers to the project copy (no double-fire)"
done
rm -f "$R26/.loom/hooks/$UW"
for ev in Stop SubagentStop; do
    CMD26=$(jq -r --arg ev "$ev" --arg m "defaults/hooks/$UW" '.hooks[$ev][] | .hooks[] | .command | select(contains($m))' "$S26" | head -1)
    OUT26=$(cd "$R26" && LOOM_HOME="$CHK26" bash -c "$CMD26" </dev/null 2>/dev/null)
    assert_contains "$OUT26" "MACHINE-RAN" "$ev: with no project copy the machine-checkout stub runs"
done

# 26d: the REAL stub through the REAL user-scope wrapper, against fake daemons.
CHK26R=$(mktemp -d); mkdir -p "$CHK26R/defaults/hooks"
cp "$REPO_ROOT/defaults/hooks/$UW" "$CHK26R/defaults/hooks/$UW"
ln -s "$REPO_ROOT/defaults/scripts" "$CHK26R/defaults/scripts"
FAKES26=$(mktemp -d)
printf '#!/bin/sh\ncat >/dev/null\necho %s\n' "'{\"decision\":\"block\",\"reason\":\"fake block\"}'" > "$FAKES26/block"
printf '#!/bin/sh\ncat >/dev/null\nexit 0\n' > "$FAKES26/allow"
printf '#!/bin/sh\necho "error: unrecognized subcommand" >&2\nexit 2\n' > "$FAKES26/stale"
chmod +x "$FAKES26/block" "$FAKES26/allow" "$FAKES26/stale"
LOG26="$FAKES26/canary.jsonl"
run26() { # $1=event $2=fake-bin-path -> W26OUT / W26RC
    local cmd
    cmd=$(jq -r --arg ev "$1" --arg m "defaults/hooks/$UW" '.hooks[$ev][] | .hooks[] | .command | select(contains($m))' "$S26" | head -1)
    W26OUT=$(cd "$R26" && LOOM_HOME="$CHK26R" LOOM_DAEMON_SELF_BIN="$2" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$LOG26" \
        bash -c "$cmd" <<<'{"hook_event_name":"'"$1"'","cwd":"'"$R26"'"}' 2>/dev/null)
    W26RC=$?
}
for ev in Stop SubagentStop; do
    run26 "$ev" "$FAKES26/block"
    assert_contains "$W26OUT|$W26RC" '"decision":"block"' "$ev: a successful block verdict passes through the wired stub"
    assert_eq "$W26RC" "0" "$ev: block is stdout JSON with exit 0"
    run26 "$ev" "$FAKES26/allow"
    assert_eq "$W26OUT|$W26RC" "|0" "$ev: an allow is silent"
    run26 "$ev" "$FAKES26/stale"
    assert_eq "$W26OUT|$W26RC" "|0" "$ev: a stale daemon (clap exit 2) fails open, never a block"
    run26 "$ev" "$FAKES26/does-not-exist"
    assert_eq "$W26RC" "0" "$ev: a missing daemon fails open"
done
assert_contains "$(cat "$LOG26" 2>/dev/null)" '"error":"unsupported_subcommand_or_flag"' "stale daemon recorded as a wrapper failure for the opted-in workspace"

# 26d2: effective precedence — a higher-tier false (local) overrides a lower-tier
# true (.loom/config.json): no wrapper record; reversing the order records one.
P26X=$(mktemp -d); mkdir -p "$P26X/.loom" "$P26X/.loom-local" "$P26X/defaults/hooks"
cp "$REPO_ROOT/defaults/hooks/$UW" "$P26X/defaults/hooks/$UW"   # no ../scripts/lib -> lib_missing
printf '{"guards":{"uncommittedWorkConsumerCanary":true}}\n' > "$P26X/.loom/config.json"
printf '{"guards":{"uncommittedWorkConsumerCanary":false}}\n' > "$P26X/.loom-local/local.json"
LOG26X="$P26X/canary.jsonl"
for ev in Stop SubagentStop; do
    LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$LOG26X" \
        bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"'"$ev"'"}' >/dev/null 2>&1
done
assert_eq "$(cat "$LOG26X" 2>/dev/null)" "" "effective false (local overrides config true) writes no wrapper record"
printf '{"guards":{"uncommittedWorkConsumerCanary":false}}\n' > "$P26X/.loom/config.json"
printf '{"guards":{"uncommittedWorkConsumerCanary":true}}\n' > "$P26X/.loom-local/local.json"
LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$LOG26X" \
    bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"Stop"}' >/dev/null 2>&1
assert_contains "$(cat "$LOG26X" 2>/dev/null)" '"error":"lib_missing"' "effective true (local overrides config false) writes a wrapper record"

# 26d3: the log override may not point inside a Git checkout (main: .git dir) or
# worktree (.git file), nor reach one via "..": no record, no directory, still exit 0.
printf '{"guards":{"uncommittedWorkConsumerCanary":true}}\n' > "$P26X/.loom-local/local.json"
mkdir -p "$P26X/chk/.git" "$P26X/wt"; : > "$P26X/wt/.git"
for L in "$P26X/chk/sub/canary.jsonl" "$P26X/wt/canary.jsonl" "$P26X/other/../chk/canary.jsonl"; do
    LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$L" \
        bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"Stop"}' >/dev/null 2>&1
    assert_eq "$?|$(ls "$P26X/chk/sub" "$P26X/other" 2>/dev/null)|$([[ -e "$L" ]] && echo wrote)" "0||" "override $L is refused: exit 0, nothing written"
done
LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$P26X/ok/canary.jsonl" \
    bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"Stop"}' >/dev/null 2>&1
assert_contains "$(cat "$P26X/ok/canary.jsonl" 2>/dev/null)" '"error":"lib_missing"' "an override outside every checkout is still honoured"

# 26d4: a FILE symlink (outside) pointing into a checkout/worktree is resolved
# before the containment check; nothing is appended through it.
mkdir -p "$P26X/lnk"; : > "$P26X/chk/target.jsonl"; : > "$P26X/wt/target.jsonl"
ln -sf "$P26X/chk/target.jsonl" "$P26X/lnk/main.jsonl"; ln -sf "$P26X/wt/target.jsonl" "$P26X/lnk/wt.jsonl"
for L in "$P26X/lnk/main.jsonl" "$P26X/lnk/wt.jsonl"; do
    LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$L" \
        bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"Stop"}' >/dev/null 2>&1
    assert_eq "$(cat "$P26X/chk/target.jsonl" "$P26X/wt/target.jsonl")" "" "file symlink $L into a checkout appends nothing"
done

# 26d5: only guards.<key> counts — an unrelated same-name key or a non-boolean
# higher-tier value must not enable wrapper records.
: > "$P26X/ok/canary.jsonl"
for CFG in '{"guards":{"uncommittedWorkConsumerCanary":false},"other":{"uncommittedWorkConsumerCanary":true}}' \
           '{"other":{"uncommittedWorkConsumerCanary":true}}' \
           '{"guards":{"uncommittedWorkConsumerCanary":"true"}}'; do
    printf '%s\n' "$CFG" > "$P26X/.loom-local/local.json"
    LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$P26X/ok/canary.jsonl" \
        bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"Stop"}' >/dev/null 2>&1
    assert_eq "$(cat "$P26X/ok/canary.jsonl" 2>/dev/null)" "" "config $CFG writes no wrapper record"
done
printf '{"guards":{"uncommittedWorkConsumerCanary":true}}\n' > "$P26X/.loom-local/local.json"

# 26d6: resolver merge semantics — a higher-tier null, or a scalar replacing the
# whole `guards` object, clears a lower-tier true (config_resolver.rs deep_merge).
printf '{"guards":{"uncommittedWorkConsumerCanary":true}}\n' > "$P26X/.loom/config.json"
for CFG in '{"guards":{"uncommittedWorkConsumerCanary":null}}' '{"guards":null}' '{"guards":"off"}'; do
    printf '%s\n' "$CFG" > "$P26X/.loom-local/local.json"
    LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$P26X/ok/canary.jsonl" \
        bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"Stop"}' >/dev/null 2>&1
    assert_eq "$(cat "$P26X/ok/canary.jsonl" 2>/dev/null)" "" "local $CFG over config true writes no wrapper record"
done

# 26d6b: a malformed or non-object tier is ignored as {} (config_resolver.rs
# soft_read_json_object), so the lower-tier true still records a lib_missing
# failure on both events.
for CFG in 'null' '[]' '{broken' '"str"' ''; do
    for ev in Stop SubagentStop; do
        : > "$P26X/ok/canary.jsonl"
        printf '%s' "$CFG" > "$P26X/.loom-local/local.json"
        LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$P26X/ok/canary.jsonl" \
            bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"'"$ev"'"}' >/dev/null 2>&1
        assert_eq "$(grep -c '"error":"lib_missing"' "$P26X/ok/canary.jsonl" 2>/dev/null)" "1" "$ev: invalid local tier [$CFG] is ignored, lower-tier true still records"
    done
done
printf '{"guards":{"uncommittedWorkConsumerCanary":true}}\n' > "$P26X/.loom-local/local.json"

# 26d7: opted in but unable to record -> exit 0 and a non-blocking coverage
# notice on both events (full log, directory as log, rejected path, relative path).
head -c 1048576 /dev/zero > "$P26X/full.jsonl"; mkdir -p "$P26X/logdir"
for ev in Stop SubagentStop; do
    for L in "$P26X/full.jsonl" "$P26X/logdir" "$P26X/chk/canary.jsonl" "rel/canary.jsonl"; do
        G26=$(LOOM_PROJECT_ROOT="$P26X" LOOM_CONFIG_DEFAULTS_FILE="" LOOM_UNCOMMITTED_WORK_CANARY_LOG="$L" \
            bash "$P26X/defaults/hooks/$UW" <<<'{"hook_event_name":"'"$ev"'"}' 2>/dev/null); rc26g=$?
        assert_eq "$rc26g|$(printf '%s' "$G26" | jq -r '[.continue, (.systemMessage|contains("NOT recorded")), has("decision")]|@csv' 2>/dev/null)" "0|true,true,false" "$ev: unrecordable log $L -> exit 0 + non-blocking coverage notice"
    done
done
assert_eq "$(( $(wc -c < "$P26X/full.jsonl") ))" "1048576" "a full log is never grown"

# 26e: deprovision removes both events' entries and leaves no empty arrays.
deprovision_loom_hooks "$HOME26/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$S26" "$UW")" "0" "deprovision removed both uncommitted-work entries"
assert_eq "$(jq -r '.hooks.SubagentStop // "absent"' "$S26")" "absent" "deprovision leaves no empty SubagentStop array behind"

# ── Test 27 (#12121): retired operator presence wiring is stripped ───────────
echo "Test 27: provision_operator_presence strips the retired presence-beat.sh wiring; user settings survive"
H27=$(mktemp -d); mkdir -p "$H27/.claude"
S27="$H27/.claude/settings.json"
PB27="bash -c 'H=\"\$HOME/.local/share/loom/defaults/hooks/presence-beat.sh\"; exec \"\$H\"'"
jq -n --arg pb "$PB27" '{permissions:{allow:["Bash(ls)"]},env:{KEEP:"1",LOOM_OPERATOR_PRESENCE:"1"},
  hooks:{Stop:[{hooks:[{type:"command",command:"echo mine"}]},{hooks:[{type:"command",command:$pb}]}],
         UserPromptSubmit:[{hooks:[{type:"command",command:$pb}]}]}}' > "$S27"
assert_eq "$(count_marker "$S27" presence-beat.sh)" "2" "fixture: two presence-beat.sh entries present"
provision_operator_presence "$H27/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$S27" presence-beat.sh)" "0" "presence-beat.sh entries stripped"
assert_eq "$(jq -r '.env.LOOM_OPERATOR_PRESENCE // "absent"' "$S27")" "absent" "presence env=1 removed"
assert_eq "$(jq -r '.env.KEEP' "$S27")" "1" "unrelated env kept"
assert_eq "$(jq -r '.permissions.allow[0]' "$S27")" "Bash(ls)" "permissions kept"
assert_eq "$(jq -r '[.hooks.Stop[].hooks[].command | select(. == "echo mine")] | length' "$S27")" "1" "user's own Stop hook kept"
assert_eq "$(jq -r '.hooks.UserPromptSubmit // "absent"' "$S27")" "absent" "emptied event removed"
cp "$S27" "$H27/before.json"
provision_operator_presence "$H27/.claude" >/dev/null 2>&1
assert_eq "$(jq -S . "$S27")" "$(jq -S . "$H27/before.json")" "re-run: settings unchanged"
# the opt-out value is the daemon's privacy switch and is never stripped
jq '.env.LOOM_OPERATOR_PRESENCE = "0"' "$S27" > "$H27/optout.json" && mv "$H27/optout.json" "$S27"
provision_operator_presence "$H27/.claude" >/dev/null 2>&1
assert_eq "$(jq -r '.env.LOOM_OPERATOR_PRESENCE' "$S27")" "0" "env=0 opt-out kept"

# ── Test 28 (#13959): the SessionStart entry is wired with a `timeout` ──────
echo "Test 28: SessionStart gh-front-env.sh carries a 10 s timeout; other entries carry none; convergence leaves hand-written entries alone (#13959)"
# The timeout of the Loom gh-front-env.sh entry (its command carries the
# machine-level marker), or "none".
GH_FRONT_TMO() {
    jq -r '[.hooks.SessionStart[]? | .hooks[]?
            | select((.command // "") | contains($m)) | .timeout // "none"] | join(",")' \
        --arg m "defaults/hooks/gh-front-env.sh" "$1" 2>/dev/null
}
# 28a: fresh provision — the SessionStart entry has the timeout, a guard does not.
H28=$(mktemp -d)
provision_loom_hooks "$H28/.claude" >/dev/null 2>&1
S28="$H28/.claude/settings.json"
assert_eq "$(GH_FRONT_TMO "$S28")" "10" "gh-front-env.sh entry wired with timeout 10"
assert_eq "$(jq -r '[.hooks.PreToolUse[]? | .hooks[]? | select((.command // "") | contains("defaults/hooks/guard-destructive.sh")) | .timeout // "none"] | join(",")' "$S28")" "none" "guard entry carries no timeout field"
assert_eq "$(jq -r '[.hooks.SessionStart[]? | .hooks[]? | select((.command // "") | contains("defaults/hooks/gh-front-env.sh")) | .timeout | type] | join(",")' "$S28")" "number" "timeout is a JSON number, not a string"
# 28b: idempotent — a second provision keeps exactly one entry, byte-stable.
cp "$S28" "$H28/before28.json"
provision_loom_hooks "$H28/.claude" >/dev/null 2>&1
assert_eq "$(GH_FRONT_TMO "$S28")" "10" "timeout still 10 after re-provision"
assert_eq "$(jq -S . "$S28")" "$(jq -S . "$H28/before28.json")" "re-provision of a converged install is byte-stable"
# 28c: a CURRENT-form wrapper seeded WITHOUT the timeout (a pre-#13959 install)
# converges in place — timeout added, command untouched, no duplicate.
H28C=$(mktemp -d); mkdir -p "$H28C/.claude"
CMD28C="$(_phook_cmd gh-front-env.sh)"
jq -n --arg cmd "$CMD28C" \
    '{hooks:{SessionStart:[{matcher:"",hooks:[{type:"command",command:$cmd}]}]}}' \
    > "$H28C/.claude/settings.json"
provision_loom_hooks "$H28C/.claude" >/dev/null 2>&1
assert_eq "$(count_marker "$H28C/.claude/settings.json" gh-front-env.sh)" "1" "pre-timeout install: still exactly one gh-front-env.sh entry (no duplicate)"
assert_eq "$(jq -r '.hooks.SessionStart[0].hooks[0].command' "$H28C/.claude/settings.json")" "$CMD28C" "pre-timeout install: command untouched"
assert_eq "$(jq -r '.hooks.SessionStart[0].hooks[0].timeout // "none"' "$H28C/.claude/settings.json")" "10" "pre-timeout install: timeout added in place"
# 28d: a hand-written SessionStart entry sharing the marker never gains a timeout.
H28D=$(mktemp -d); mkdir -p "$H28D/.claude"
HAND28='.claude/hooks/my-own-front-env.sh --source defaults/hooks/gh-front-env.sh'
jq -n --arg cmd "$HAND28" \
    '{hooks:{SessionStart:[{matcher:"",hooks:[{type:"command",command:$cmd}]}]}}' \
    > "$H28D/.claude/settings.json"
provision_loom_hooks "$H28D/.claude" >/dev/null 2>&1
assert_eq "$(jq -r '[.hooks.SessionStart[] | .hooks[] | select(.command == $h) | .timeout // "none"] | join(",")' --arg h "$HAND28" "$H28D/.claude/settings.json")" "none" "hand-written entry never gains a timeout"
# 28e: project-level wiring — the written entry carries the timeout, and a
# bare pre-#15741 Loom entry is upgraded to the launcher form AND converges in
# place.
R28=$(mktemp -d)
mkdir -p "$R28/.loom/hooks" "$R28/.claude"
touch "$R28/.loom/hooks/gh-front-env.sh"
cat > "$R28/.claude/settings.json" <<'EOF'
{ "hooks": { "SessionStart": [ { "matcher": "", "hooks": [
  { "type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh\"" }
] } ] } }
EOF
ensure_project_hook_wiring "$R28" >/dev/null 2>&1
assert_eq "$(jq -r '[.hooks.SessionStart[]? | .hooks[]? | select((.command // "") | contains(".loom/hooks/gh-front-env.sh")) | .timeout // "none"] | join(",")' "$R28/.claude/settings.json")" "10" "project-level gh-front-env.sh entry carries timeout 10"
assert_eq "$(jq -r '[.hooks.SessionStart[]? | .hooks[]? | select((.command // "") | contains(".loom/hooks/gh-front-env.sh"))] | length' "$R28/.claude/settings.json")" "1" "project-level: converged in place, no duplicate"
assert_eq "$(jq -r '[.hooks.SessionStart[]? | .hooks[]? | select((.command // "") | contains(".loom/hooks/gh-front-env.sh")) | .command][0]' "$R28/.claude/settings.json")" \
    "$(_phook_project_cmd SessionStart gh-front-env.sh)" "project-level: the bare entry was upgraded to the launcher form in the same pass"
# 28f: a LAUNCHER-form entry that lost its timeout converges back, command untouched.
jq --arg cmd "$(_phook_project_cmd SessionStart gh-front-env.sh)" \
    '.hooks.SessionStart[0].hooks = [{type:"command", command:$cmd}]' "$R28/.claude/settings.json" > "$R28/s28f.json" \
    && mv "$R28/s28f.json" "$R28/.claude/settings.json"
ensure_project_hook_wiring "$R28" >/dev/null 2>&1
assert_eq "$(jq -r '[.hooks.SessionStart[]? | .hooks[]? | .timeout // "none"] | join(",")' "$R28/.claude/settings.json")" "10" "a launcher-form entry seeded without its timeout converges to 10"
assert_eq "$(jq -r '.hooks.SessionStart[0].hooks[0].command' "$R28/.claude/settings.json")" "$(_phook_project_cmd SessionStart gh-front-env.sh)" "the converged entry's command is untouched"
# 28g: a hand-written PROJECT entry naming the same script is not Loom's: never
# rewritten, never given a timeout, never duplicated.
jq --arg cmd 'bash "${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh"' \
    '.hooks.SessionStart[0].hooks = [{type:"command", command:$cmd}]' "$R28/.claude/settings.json" > "$R28/s28g.json" \
    && mv "$R28/s28g.json" "$R28/.claude/settings.json"
ensure_project_hook_wiring "$R28" >/dev/null 2>&1
assert_eq "$(jq -r '.hooks.SessionStart[0].hooks[0].command' "$R28/.claude/settings.json")" 'bash "${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh"' "hand-written project entry: command untouched"
assert_eq "$(jq -r '[.hooks.SessionStart[0].hooks[] | .timeout // "none"] | join(",")' "$R28/.claude/settings.json")" "none" "hand-written project entry: no timeout added"
assert_eq "$(jq -r '.hooks.SessionStart[0].hooks | length' "$R28/.claude/settings.json")" "1" "hand-written project entry: no duplicate appended beside it"

# ── Test 29 (#15741): bare Loom commands upgrade in place; custom ones survive ─
echo "Test 29: the bare \${CLAUDE_PROJECT_DIR} commands an earlier Loom writer left are upgraded in place to the launcher form; custom commands, order and unrelated settings survive (#15741)"
R29=$(mktemp -d)
make_transition_repo "$R29"
cat > "$R29/.claude/settings.json" <<'EOF'
{ "permissions": { "allow": ["Bash(x:*)"] },
  "env": { "KEEP": "1" },
  "hooks": {
    "UserPromptSubmit": [ { "matcher": "", "hooks": [
      { "type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.claude/hooks/mine.sh\"" },
      { "type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/skill-router.sh\"" }
    ] } ],
    "PreToolUse": [ { "matcher": "Bash", "hooks": [
      { "type": "command", "command": "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\"" },
      { "type": "command", "command": "bash \"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-loom-workflow.sh\"" }
    ] } ],
    "Stop": [ { "matcher": "", "hooks": [
      { "type": "command", "command": "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-background-subagents.sh" }
    ] } ]
  } }
EOF
S29="$R29/.claude/settings.json"
ensure_project_hook_wiring "$R29" >/dev/null 2>&1
assert_eq "$(jq -r '.hooks.UserPromptSubmit[0].hooks[0].command' "$S29")" '"${CLAUDE_PROJECT_DIR}/.claude/hooks/mine.sh"' "the operator's own entry is kept, in place (still first)"
assert_eq "$(jq -r '.hooks.UserPromptSubmit[0].hooks[1].command' "$S29")" "$(_phook_project_cmd UserPromptSubmit skill-router.sh)" "a quoted bare Loom entry is upgraded in place to the launcher form"
assert_eq "$(jq -r '.hooks.PreToolUse[0].hooks[0].command' "$S29")" "$(_phook_project_cmd PreToolUse guard-destructive.sh)" "the upgrade carries the entry's own event (PreToolUse guard keeps its deny)"
assert_eq "$(jq -r '.hooks.Stop[0].hooks[0].command' "$S29")" "$(_phook_project_cmd Stop guard-background-subagents.sh)" "an UNQUOTED bare Loom entry is upgraded in place too"
assert_eq "$(jq -r '.hooks.PreToolUse[0].hooks[1].command' "$S29")" 'bash "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-loom-workflow.sh"' "a custom command that merely names a Loom hook is never rewritten"
for n in guard-destructive.sh guard-loom-workflow.sh guard-worktree-paths.sh skill-router.sh methodology-inject.sh guard-background-subagents.sh; do
    assert_eq "$(count_project_entry "$S29" "$n")" "1" "exactly one project entry for $n after the upgrade pass"
done
assert_eq "$(jq -r '.hooks.UserPromptSubmit[0].hooks | length' "$S29")" "3" "the still-missing sibling (methodology-inject.sh) was appended after the surviving entries, in the same pass"
assert_eq "$(jq -r '.hooks | keys_unsorted | join(",")' "$S29")" "UserPromptSubmit,PreToolUse,Stop" "event order preserved"
assert_eq "$(jq -r '.permissions.allow[0]' "$S29")" "Bash(x:*)" "project permissions preserved by the upgrade"
assert_eq "$(jq -r '.env.KEEP' "$S29")" "1" "unrelated settings preserved by the upgrade"
cp "$S29" "$R29/before29.json"
ensure_project_hook_wiring "$R29" >/dev/null 2>&1
cmp -s "$S29" "$R29/before29.json" && pass "a second pass is byte-stable (no duplicate, no rewrite)" \
    || fail "a second pass changed the settings file"

# ── Test 30 (#15741): a WRITTEN entry runs its hook, and reports a broken install ──
echo "Test 30: a written project entry runs its hook from a spaced path and reports a broken install without blocking the event (#15741)"
P30_PARENT=$(mktemp -d); R30="$P30_PARENT/has a space"
make_transition_repo "$R30"
# Hooks that prove they ran (they replace the fixture's silent stub where one
# exists): the launcher, a PreToolUse guard, and three table hooks the
# transition fixture does not carry.
cp "$REPO_ROOT/defaults/hooks/hook-wiring.sh" "$R30/.loom/hooks/hook-wiring.sh"
for n in guard-destructive.sh skill-router.sh guard-uncommitted-work.sh roll-pause.sh gh-front-env.sh; do
    cat > "$R30/.loom/hooks/$n" <<'HOOK'
#!/usr/bin/env bash
printf 'RAN:%s STDIN:%s\n' "$(basename "$0")" "$(cat)"
HOOK
    chmod +x "$R30/.loom/hooks/$n"
done
printf '{}\n' > "$R30/.claude/settings.json"
ensure_project_hook_wiring "$R30" >/dev/null 2>&1
S30="$R30/.claude/settings.json"
assert_eq "$(count_unlaunched "$S30")" "0" "fixture: every written entry is the launcher form"
assert_eq "$(jq -r '[.hooks.SessionStart[]? | .hooks[]? | .timeout // "none"] | join(",")' "$S30")" "10" "the written SessionStart gh-front-env.sh entry carries its 10 s timeout"
O30="$P30_PARENT/out"; E30="$P30_PARENT/err"
E30_HOME="$P30_PARENT/empty/home"; E30_LOOM="$P30_PARENT/empty/loom"
mkdir -p "$E30_HOME" "$E30_LOOM"
PAYLOAD30='{"hook_event_name":"x","payload":1}'
field30() { printf '%s' "$1" | cut -d'|' -f"$2"; }
# run_entry <event> <name> [VAR=VALUE...] -> "<exit>|<stdout>|<stderr>": runs the
# WRITTEN entry the way Claude Code does — from inside the project, with
# CLAUDE_PROJECT_DIR exported and the hook payload on stdin.
run_entry() {
    local event="$1" name="$2" cmd rc=0; shift 2
    cmd="$(jq -r --arg e "$event" --arg n ".loom/hooks/$name" \
        '[.hooks[$e][]? | .hooks[]? | .command | select(contains($n)) | select(contains("defaults/hooks/") | not)][0]' "$S30")"
    [[ -n "$cmd" ]] || { printf 'NOENTRY||'; return 0; }
    ( cd "$R30" && env -u LOOM_PROJECT_ROOT -u LOOM_GUARD_WIRING_FAILOPEN HOME="$E30_HOME" LOOM_HOME="$E30_LOOM" \
        CLAUDE_PROJECT_DIR="$R30" "$@" bash -c "$cmd" ) <<<"$PAYLOAD30" >"$O30" 2>"$E30" || rc=$?
    printf '%s|%s|%s' "$rc" "$(cat "$O30")" "$(cat "$E30")"
}
# 30a: launcher present -> each event's written entry runs its hook from a
# spaced path, stdin passed through untouched.
for row in "UserPromptSubmit skill-router.sh" "Stop guard-uncommitted-work.sh" "SubagentStop guard-uncommitted-work.sh" \
           "SessionStart gh-front-env.sh" "PreToolUse roll-pause.sh" "PreToolUse guard-destructive.sh"; do
    read -r ev30 name30 <<<"$row"
    R30OUT="$(run_entry "$ev30" "$name30")"
    assert_eq "$(field30 "$R30OUT" 1)" "0" "$ev30 $name30: exits 0"
    assert_contains "$(field30 "$R30OUT" 2)" "RAN:$name30 STDIN:$PAYLOAD30" "$ev30 $name30: the written entry runs the installed hook, stdin passed through"
    assert_eq "$(field30 "$R30OUT" 3)" "" "$ev30 $name30: silent on stderr when everything is installed"
done
# 30b: launcher NOT yet resynced -> the inline fallback runs the copy; a copy
# that lost its +x bit still runs (zero lost coverage).
rm -f "$R30/.loom/hooks/hook-wiring.sh"
R30OUT="$(run_entry UserPromptSubmit skill-router.sh)"
assert_contains "$(field30 "$R30OUT" 2)" "RAN:skill-router.sh" "no launcher: the inline fallback exec's the copy"
chmod -x "$R30/.loom/hooks/skill-router.sh"
R30OUT="$(run_entry UserPromptSubmit skill-router.sh)"
assert_eq "$(field30 "$R30OUT" 1)|$(field30 "$R30OUT" 2)" "0|RAN:skill-router.sh STDIN:$PAYLOAD30" "no launcher, copy not executable: the inline fallback runs it via bash"
chmod +x "$R30/.loom/hooks/skill-router.sh"
# 30c: launcher and hook both gone -> a NON-PreToolUse event reports the broken
# install on stderr and lets the event finish (no stdout, exit 0).
rm -f "$R30/.loom/hooks/skill-router.sh" "$R30/.loom/hooks/guard-uncommitted-work.sh" "$R30/.loom/hooks/gh-front-env.sh"
for row in "UserPromptSubmit skill-router.sh" "Stop guard-uncommitted-work.sh" "SessionStart gh-front-env.sh"; do
    read -r ev30 name30 <<<"$row"
    R30OUT="$(run_entry "$ev30" "$name30")"
    assert_eq "$(field30 "$R30OUT" 1)|$(field30 "$R30OUT" 2)" "0|" "$ev30 $name30: broken install still allows the event (exit 0, nothing on stdout)"
    assert_contains "$(field30 "$R30OUT" 3)" "BROKEN GUARD INSTALL" "$ev30 $name30: broken install is reported on stderr"
    assert_contains "$(field30 "$R30OUT" 3)" "$name30" "$ev30 $name30: the report names the missing hook"
done
# 30d: a PreToolUse GUARD with nothing anywhere denies (exit 0 + a decision
# document); the fail-open roll-pause entry does not; the escape hatch downgrades.
rm -f "$R30/.loom/hooks/guard-destructive.sh" "$R30/.loom/hooks/roll-pause.sh"
R30OUT="$(run_entry PreToolUse guard-destructive.sh)"
assert_eq "$(field30 "$R30OUT" 1)" "0" "PreToolUse guard, nothing installed: a deny is a decision, never a non-zero exit"
assert_contains "$(field30 "$R30OUT" 2)" '"permissionDecision":"deny"' "PreToolUse guard, nothing installed: the inline fallback DENIES"
if printf '%s' "$(field30 "$R30OUT" 2)" | jq -e '.hookSpecificOutput.hookEventName == "PreToolUse"' >/dev/null 2>&1; then
    pass "PreToolUse guard, nothing installed: the deny document is valid JSON tagged PreToolUse"
else
    fail "PreToolUse guard, nothing installed: the deny document is valid JSON tagged PreToolUse ($(field30 "$R30OUT" 2))"
fi
assert_contains "$(field30 "$R30OUT" 3)" "BROKEN GUARD INSTALL" "PreToolUse guard, nothing installed: reported on stderr as well"
R30OUT="$(run_entry PreToolUse roll-pause.sh)"
assert_eq "$(field30 "$R30OUT" 1)|$(field30 "$R30OUT" 2)" "0|" "PreToolUse roll-pause, nothing installed: no deny (it is not a guard)"
assert_contains "$(field30 "$R30OUT" 3)" "BROKEN GUARD INSTALL" "PreToolUse roll-pause, nothing installed: still reported on stderr"
R30OUT="$(run_entry PreToolUse guard-destructive.sh LOOM_GUARD_WIRING_FAILOPEN=1)"
assert_eq "$(field30 "$R30OUT" 1)|$(field30 "$R30OUT" 2)" "0|" "LOOM_GUARD_WIRING_FAILOPEN=1: the deny is downgraded to an allow"
assert_contains "$(field30 "$R30OUT" 3)" "BROKEN GUARD INSTALL" "LOOM_GUARD_WIRING_FAILOPEN=1: never silent"
# 30e: not a Loom workspace -> the same command text allows silently.
mkdir -p "$P30_PARENT/plain"
R30OUT="$( cd "$P30_PARENT/plain" && env -u LOOM_PROJECT_ROOT HOME="$E30_HOME" LOOM_HOME="$E30_LOOM" \
    CLAUDE_PROJECT_DIR="$P30_PARENT/plain" bash -c "$(_phook_project_cmd PreToolUse guard-destructive.sh)" </dev/null 2>&1 )"
assert_eq "0|$R30OUT" "0|" "a non-Loom repo: the command allows silently, even for a PreToolUse guard"

echo ""
echo "======================================"
echo "test-provision-hooks.sh: $TESTS_PASSED/$TESTS_RUN passed, $TESTS_FAILED failed"
echo "======================================"
[[ "$TESTS_FAILED" -eq 0 ]]
