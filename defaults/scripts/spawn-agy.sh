#!/usr/bin/env bash
# spawn-agy.sh - Tier-3 "generic passthrough" adapter for Google's Antigravity
# CLI (`agy`), the second route of the Gemini pilot (issue #14945).
#
# Thinner than spawn-aider.sh but thicker than spawn-gemini.sh, deliberately:
# like gemini, the launch SHAPE (cliBin/promptFlag/modelFlag) lives in
# `defaults/runtimes/agy.json` and is resolved by spawn-generic-launch.sh.
# Unlike gemini, the pilot carries two per-launch requirements the shared
# template cannot express as static manifest data:
#
#   1. An isolated CLI settings location OUTSIDE the checkout. The canary
#      (loom-crew-8, 2026-11-22, agy 1.5.14) ran with its own HOME holding a
#      settings file naming "modelProvider": "gemini" and needed no Google
#      login. A fresh per-launch HOME both supplies that settings file and
#      keeps agy away from any cached login under the operator's real HOME —
#      the credential MUST come from the gemini API-key pool the worker
#      process selected (manifest credentialPool), never from a stored
#      session, or the pilot's shared maxConcurrent cap is bypassable.
#   2. The guarded permission choice: `--sandbox` (plus the `--settings`
#      argument naming the isolated file). agy's headless default can
#      auto-approve some edits; the pilot pins the explicit sandbox policy
#      instead of the auto-approve flag. A tool denial can still exit 0, so
#      the read-only outcome is verified live, not trusted (see
#      defaults/docs/runtime-adapters.md's agy section).
#
# Both are composed here as LOOM_GENERIC_EXTRA_ARGS DEFAULTS: an operator who
# exported LOOM_GENERIC_EXTRA_ARGS already wins (the env > manifest tier
# spawn-generic-launch.sh documents), and the manifest's launch object stays
# free of per-launch paths.
#
# The GEMINI_API_KEY itself is NOT handled here: `loom-daemon spawn-worker`
# selects the gemini pool account and injects the key into this process's
# environment before exec'ing the runner, so the lease is counted against
# the worker PID that becomes the run.
#
# Usage:
#   .loom/scripts/spawn-agy.sh -p "your prompt"
#   LOOM_RUNTIME=agy .loom/scripts/spawn-worker.sh -p "your prompt"
set -euo pipefail

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Minutes a per-launch HOME must have been abandoned before the launch-time
# sweep below will reclaim it.
AGY_HOME_MAX_AGE_MIN=1440

# _agy_pid_confirmed_gone <pid> -- exit 0 only when `kill -0` fails with ESRCH.
# A signalable pid is live, and EPERM (the pid was reused by another user's
# process) is NOT "gone", so the two have to be told apart; the C-locale text
# of the builtin's own error is the only way bash exposes errno.
_agy_pid_confirmed_gone() {
    local msg
    msg="$(LC_ALL=C kill -0 "$1" 2>&1 >/dev/null)" && return 1
    [[ "$msg" == *"No such process"* ]]
}

# _agy_sweep_stale_homes -- best-effort reclamation of per-launch HOMEs whose
# launch is over. It can never fail the launch, prints nothing, and costs one
# pass over the top level of TMPDIR. Every step is guarded by hand, because the
# caller invokes it inside `|| true`, which suspends `set -e` for the whole
# function.
_agy_sweep_stale_homes() {
    local root="${TMPDIR:-/tmp}" dir marker pid
    [[ -d "$root" ]] || return 0
    while IFS= read -r -d '' dir; do
        marker="$dir/.loom-pid"
        [[ -f "$marker" && -r "$marker" ]] || continue
        pid="$(head -c 32 "$marker" 2>/dev/null)" || continue
        [[ "$pid" =~ ^[1-9][0-9]*$ ]] || continue
        _agy_pid_confirmed_gone "$pid" || continue
        rm -rf -- "$dir" 2>/dev/null || true
    done < <(find "$root" -mindepth 1 -maxdepth 1 -name 'loom-agy-*' -type d \
        -uid "$EUID" -mmin +"$AGY_HOME_MAX_AGE_MIN" -print0 2>/dev/null)
    return 0
}

# Per-launch isolated HOME: outside every checkout, owner-only, never reused,
# and holding the settings file that selects the gemini provider -- so no
# cached login under the operator's real HOME can be picked up. It does NOT
# die with the launch: this script `exec`s down the adapter chain, so nothing
# set here can run afterwards to remove it. What keeps it from accumulating is
# the sweep, run at the NEXT launch: it reclaims a same-owner `loom-agy-*`
# directory only once that directory is older than AGY_HOME_MAX_AGE_MIN and
# its `.loom-pid` marker names a process confirmed gone. Anything whose
# liveness is unknown is kept: a live launch, a missing/unreadable/malformed
# marker, and every HOME written before the marker existed (those stay with
# the host's own temp-dir policy, as before). Pid reuse only delays removal.
_agy_sweep_stale_homes >/dev/null 2>&1 || true
AGY_HOME="$(mktemp -d "${TMPDIR:-/tmp}/loom-agy-XXXXXX")"
chmod 700 "$AGY_HOME"
printf '{"modelProvider": "gemini"}\n' >"$AGY_HOME/settings.json"
chmod 600 "$AGY_HOME/settings.json"
# The liveness marker the sweep reads. `$$` is the pid that exec's `agy`
# (interactive) or waits on it (headless): spawn-generic-launch.sh and
# spawn-generic.sh both `exec`, the `nice` re-exec included, so this pid lives
# exactly as long as the launch.
printf '%s\n' "$$" >"$AGY_HOME/.loom-pid"
chmod 600 "$AGY_HOME/.loom-pid"

# Guarded launch argv: the settings argument selecting the gemini provider,
# and the sandbox policy (never agy's auto-approve flag). Set as DEFAULTS
# only — an operator-pinned LOOM_GENERIC_EXTRA_ARGS stays authoritative and
# this script adds nothing on top of it.
if [[ -z "${LOOM_GENERIC_EXTRA_ARGS:-}" ]]; then
    LOOM_GENERIC_EXTRA_ARGS="--settings $AGY_HOME/settings.json --sandbox"
    export LOOM_GENERIC_EXTRA_ARGS
fi
export HOME="$AGY_HOME"

exec "${_SCRIPT_DIR}/spawn-generic-launch.sh" agy "$@"
