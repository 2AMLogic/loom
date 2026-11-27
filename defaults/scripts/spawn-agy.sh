#!/usr/bin/env bash
# spawn-agy.sh - Tier-3 "generic passthrough" adapter for Google's Antigravity
# CLI (`agy`), the second route of the Gemini pilot (issue #14945).
#
# Thinner than spawn-aider.sh but thicker than spawn-gemini.sh, deliberately:
# like gemini, the launch SHAPE (cliBin/promptFlag/modelFlag) lives in
# `defaults/runtimes/agy.json` and is resolved by spawn-generic-launch.sh.
# Unlike gemini, the pilot carries per-launch requirements the shared
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
#   2. An explicit read-only PERMISSION POLICY (issue #15474). `--sandbox` is
#      terminal containment, not a permission policy: since v1.5.7 a headless
#      run starts in `auto` mode wherever that mode is available, where
#      workspace file edits and sandboxed commands run without approval, and
#      an approval headless mode cannot give is a soft denial that still
#      exits 0. So the policy is pinned in the per-launch settings file — the
#      default execution mode, `toolPermission: request-review`, and explicit
#      rules the vendor documents as taking precedence over every mode: the
#      workspace readable through the file-read tool, file writes, shell
#      commands and MCP tools denied. The file is written both to the path
#      the `--settings` argument names and to the CLI's documented default
#      settings location under the isolated HOME, so it holds whether or not
#      the binary honours `--settings`. `--sandbox` stays on the argv for
#      containment only. Whether the pinned binary honours these keys and
#      rules is verified live, not trusted (see the agy section of
#      defaults/docs/runtime-adapters.md).
#   3. The self-update kill switch (issue #15474). An enabled updater
#      replaces the binary in place, which silently moves a version pin, so
#      the child gets AGY_CLI_DISABLE_AUTO_UPDATE=true unless the environment
#      already carries a non-empty value (env > adapter).
#
# The guarded argv — the settings argument, then containment — is no longer
# a replaceable default. The operator's LOOM_GENERIC_EXTRA_ARGS and the
# worker's own arguments are screened for a token that would replace or
# contradict the policy, and such a launch REFUSES at 78 before anything is
# created or run. Every other token is left alone, and the manifest's launch
# object stays free of per-launch paths.
#
# The GEMINI_API_KEY itself is NOT handled here: `loom-daemon spawn-worker`
# (>= 0.20.41, `0918138e9`) selects the gemini pool account and injects the key
# into this process's environment before exec'ing the runner, so the lease is
# counted against the worker PID that becomes the run. An older worker binary
# skips the manifest's `credentialPool` and lets an ambient GEMINI_API_KEY
# reach the child (#15532), so this adapter checks the selected worker
# executable (`LOOM_SPAWN_WORKER_DAEMON_BIN`, supplied by `spawn-worker.sh` or
# direct native `spawn-worker`) against >= 0.20.41 before creating HOME or
# running `agy`. Bare adapter calls lack that identity and refuse at 78.
#
# Usage:
#   LOOM_RUNTIME=agy .loom/scripts/spawn-worker.sh -p "your prompt"
set -euo pipefail

_SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# Refuse the launch: $1 names where the offending token came from, $2 is the
# token, $3 what it would do, $4 what to do instead.
_agy_refuse() {
    printf 'spawn-agy: refusing to launch (exit 78): [%s] %s %s.\n  REMEDIATION: %s\n' \
        "$1" "$2" "$3" "$4" >&2
    exit 78  # EX_CONFIG
}

# Background self-update kill switch, ahead of everything else: nothing this
# script runs may inherit an updater-enabled environment.
export AGY_CLI_DISABLE_AUTO_UPDATE="${AGY_CLI_DISABLE_AUTO_UPDATE:-true}"

# Mandatory-pool daemon floor (#15532): v0.20.41 (`0918138e9`) is the first
# release where `loom-daemon spawn-worker` enforces `agy`'s manifest
# `credentialPool`. Check ONLY the worker executable handed off for this
# launch, fail closed on any missing/unreadable/below-floor binary before
# creating the isolated HOME, and consume the private handoff variable so the
# CLI child never inherits it. Advisory preflight skip switches do not apply.
_AGY_DAEMON_FLOOR="0.20.41"
_AGY_WORKER_BIN="${LOOM_SPAWN_WORKER_DAEMON_BIN:-}"
unset LOOM_SPAWN_WORKER_DAEMON_BIN

_agy_version_ge() {
    local have="$1" floor="$2"
    local hm hn hp fm fn fp
    [[ "$have" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || return 1
    hm="${BASH_REMATCH[1]}"; hn="${BASH_REMATCH[2]}"; hp="${BASH_REMATCH[3]}"
    [[ "$floor" =~ ^([0-9]+)\.([0-9]+)\.([0-9]+)$ ]] || return 1
    fm="${BASH_REMATCH[1]}"; fn="${BASH_REMATCH[2]}"; fp="${BASH_REMATCH[3]}"
    if (( 10#$hm != 10#$fm )); then (( 10#$hm > 10#$fm )); return $?; fi
    if (( 10#$hn != 10#$fn )); then (( 10#$hn > 10#$fn )); return $?; fi
    (( 10#$hp >= 10#$fp ))
}

_agy_check_worker_floor() {
    local ver_out="" ver_rc=0 ver_line="" have_ver=""
    if [[ -z "$_AGY_WORKER_BIN" ]]; then
        _agy_refuse "daemon floor" "LOOM_SPAWN_WORKER_DAEMON_BIN is unset" \
            "because agy requires loom-daemon >= $_AGY_DAEMON_FLOOR to enforce its mandatory gemini credentialPool and a bare spawn-agy.sh invocation cannot establish which worker ran" \
            "launch via 'LOOM_RUNTIME=agy .loom/scripts/spawn-worker.sh -p ...', or update loom-daemon (>= $_AGY_DAEMON_FLOOR; .loom/scripts/cli/loom-daemon-update.sh --fetch) when invoking 'loom-daemon spawn-worker' directly"
    fi
    if [[ "$_AGY_WORKER_BIN" != /* || ! -f "$_AGY_WORKER_BIN" || ! -x "$_AGY_WORKER_BIN" ]]; then
        _agy_refuse "daemon floor" "'$_AGY_WORKER_BIN'" \
            "is not an executable absolute loom-daemon path (agy requires loom-daemon >= $_AGY_DAEMON_FLOOR)" \
            "launch via 'LOOM_RUNTIME=agy .loom/scripts/spawn-worker.sh -p ...' or roll this host (.loom/scripts/cli/loom-daemon-update.sh --fetch) to loom-daemon >= $_AGY_DAEMON_FLOOR"
    fi
    set +e
    ver_out="$("$_AGY_WORKER_BIN" --version 2>/dev/null </dev/null)"
    ver_rc=$?
    set -e
    if [[ "$ver_rc" -ne 0 ]]; then
        _agy_refuse "daemon floor" "'$_AGY_WORKER_BIN --version'" \
            "exited $ver_rc instead of reporting a usable loom-daemon >= $_AGY_DAEMON_FLOOR" \
            "roll this host (.loom/scripts/cli/loom-daemon-update.sh --fetch) or set LOOM_DAEMON_BIN to a working loom-daemon >= $_AGY_DAEMON_FLOOR, and launch via .loom/scripts/spawn-worker.sh"
    fi
    ver_line="${ver_out%%$'\n'*}"
    ver_line="${ver_line%$'\r'}"
    if [[ ! "$ver_line" =~ ^loom-daemon[[:space:]]+([0-9]+\.[0-9]+\.[0-9]+)([[:space:]]|$) ]]; then
        _agy_refuse "daemon floor" "'$_AGY_WORKER_BIN --version'" \
            "returned an unrecognized version string (expected 'loom-daemon <major.minor.patch>' >= $_AGY_DAEMON_FLOOR)" \
            "roll this host (.loom/scripts/cli/loom-daemon-update.sh --fetch) or set LOOM_DAEMON_BIN to loom-daemon >= $_AGY_DAEMON_FLOOR, and launch via .loom/scripts/spawn-worker.sh"
    fi
    have_ver="${BASH_REMATCH[1]}"
    if ! _agy_version_ge "$have_ver" "$_AGY_DAEMON_FLOOR"; then
        _agy_refuse "daemon floor" "'$_AGY_WORKER_BIN' (loom-daemon $have_ver)" \
            "predates loom-daemon >= $_AGY_DAEMON_FLOOR, the first release that enforces agy's mandatory gemini credentialPool" \
            "roll this host (.loom/scripts/cli/loom-daemon-update.sh --fetch) or build/pin LOOM_DAEMON_BIN to loom-daemon >= $_AGY_DAEMON_FLOOR before launching via .loom/scripts/spawn-worker.sh"
    fi
}

_agy_check_worker_floor

# Screen one source of CLI-bound tokens against the pinned policy and collect
# what survives in _agy_kept (space-prefixed). `tag` is `extra-args` for an
# override this script re-emits, or `argv` for the worker's own arguments,
# which are forwarded verbatim and only checked. `label` names the source in
# a refusal.
_agy_kept=""
_agy_screen() {
    local tag="$1" label="$2"
    shift 2
    local arg name
    for arg in "$@"; do
        name="${arg%%=*}"
        case "$name" in
            --settings)
                _agy_refuse "$label" "'$arg'" \
                    'names its own settings file, so the per-launch permission policy would not apply' \
                    'take it out; spawn-agy.sh binds --settings to the policy file it writes per launch'
                ;;
            --dangerously-skip-permissions)
                # spawn-generic.sh consumes the bare form from the worker's
                # own argv (a Loom runner-neutral convention) and never
                # forwards it. Every other form here reaches the CLI.
                if [[ "$tag" == "argv" && "$arg" == "--dangerously-skip-permissions" ]]; then
                    continue
                fi
                _agy_refuse "$label" "'$arg'" \
                    'auto-approves every tool call, contradicting the read-only permission policy' \
                    'take it out; the pilot launches read-only (issue #15474)'
                ;;
            --mode)
                _agy_refuse "$label" "'$arg'" \
                    'overrides the execution mode the per-launch settings pin' \
                    'take it out; the default mode is part of the pinned policy (issue #15474)'
                ;;
            --sandbox)
                # The guarded argv already carries the bare flag.
                case "$arg" in
                    --sandbox|--sandbox=true) ;;
                    *)
                        _agy_refuse "$label" "'$arg'" \
                            'sets the terminal sandbox explicitly, while the guarded launch keeps containment as the bare flag' \
                            'take it out, or pass the bare --sandbox'
                        ;;
                esac
                ;;
            *)
                if [[ "$tag" == "extra-args" ]]; then
                    _agy_kept+=" $arg"
                fi
                ;;
        esac
    done
}

_agy_screen argv "the worker's arguments" "$@"

_OPERATOR_ARGS=()
if [[ -n "${LOOM_GENERIC_EXTRA_ARGS:-}" ]]; then
    IFS=' ' read -r -a _OPERATOR_ARGS <<<"$LOOM_GENERIC_EXTRA_ARGS" || true
fi
_agy_screen extra-args LOOM_GENERIC_EXTRA_ARGS ${_OPERATOR_ARGS[@]+"${_OPERATOR_ARGS[@]}"}

# The read rule names the workspace: the directory the CLI will take as its
# own, since nothing in the chain changes directory.
AGY_WORKSPACE="${PWD:-$(pwd)}"
if [[ "$AGY_WORKSPACE" =~ [[:cntrl:]] ]]; then
    _agy_refuse "the workspace path" "'$AGY_WORKSPACE'" \
        'contains a control character and cannot be written into the permission policy' \
        'launch from a workspace whose path has no control characters'
fi

# Per-launch isolated HOME: outside every checkout, owner-only, never reused.
# `agy` state (settings, any cache) lives and dies with the launch; nothing
# accumulates on the host and no cached login can be picked up.
AGY_HOME="$(mktemp -d "${TMPDIR:-/tmp}/loom-agy-XXXXXX")"
chmod 700 "$AGY_HOME"
AGY_SETTINGS="$AGY_HOME/settings.json"

_agy_json_string() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    printf '%s' "$s"
}

# The permission policy. Explicit rules beat every execution mode (deny >
# ask > allow), so the denials hold even if the binary resolves `auto` mode;
# `write_file(*)` and `command(*)` cover workspace and non-workspace targets,
# inside and outside the terminal sandbox.
printf '{"modelProvider": "gemini", "agentMode": "", "toolPermission": "request-review", "permissions": {"allow": ["read_file(%s)"], "deny": ["write_file(*)", "command(*)", "mcp(*)"]}}\n' \
    "$(_agy_json_string "$AGY_WORKSPACE")" >"$AGY_SETTINGS"
chmod 600 "$AGY_SETTINGS"

# The same policy at the CLI's documented default settings location, so it
# holds whether or not the binary honours `--settings`.
mkdir -p "$AGY_HOME/.gemini/antigravity-cli"
chmod 700 "$AGY_HOME/.gemini" "$AGY_HOME/.gemini/antigravity-cli"
cp "$AGY_SETTINGS" "$AGY_HOME/.gemini/antigravity-cli/settings.json"
chmod 600 "$AGY_HOME/.gemini/antigravity-cli/settings.json"

# Guarded argv: the policy file, then containment (never agy's auto-approve
# flag), then whatever survived screening. spawn-generic-launch.sh forwards
# extra args ahead of the model and the prompt.
LOOM_GENERIC_EXTRA_ARGS="--settings $AGY_SETTINGS --sandbox$_agy_kept"
export LOOM_GENERIC_EXTRA_ARGS
export HOME="$AGY_HOME"

exec "${_SCRIPT_DIR}/spawn-generic-launch.sh" agy "$@"
