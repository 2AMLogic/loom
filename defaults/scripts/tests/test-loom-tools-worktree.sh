#!/usr/bin/env bash
# test-loom-tools-worktree.sh - the installed worktree wrapper on a tools-only
# host (#14476, part of #14397; design docs/design/loom-tools-cli-boundary.md
# §13.11).
#
# Installs the REAL `loom-tools` and the shipped scripts into a throwaway
# checkout whose path contains a space, with NO `loom-daemon` in any locator
# and no service, and drives the normal entrypoint end to end:
#
#   (0) the isolation holds: every daemon lookup path resolves nothing, the
#       two nested locators below worktree.sh's own resolution included
#   (1) create, enter the returned path, reuse it, remove it (a local origin)
#   (2) crash cleanup, an interrupted create and its safe retry, and a live
#       competing lock whose ownership survives the refused caller
#   (3) the create-time guards against a controlled forge: the claim probe
#       (blocked / clear / unanswered / a mode error), the wrapper's own
#       fresh-branch probe, branch reuse, the claim lock and the attended lease
#   (4) removal refusal for unmanaged and dirty worktrees
#   (5) legacy invocations stay compatible, and the daemon-only verbs refuse
#
# Needs a built loom-tools: LOOM_TOOLS_TEST_BIN=<path>. Without one the suite
# prints SKIP and exits 0 (a bare shell CI leg has none); the cargo test
# loom-daemon/tests/loom_tools_worktree.rs hands it the binary cargo just built.
#
# Every child runs under `env -i` (iso()): an isolated HOME, install directory,
# PATH and cargo target dir, with nothing inherited. The host this runs on may
# well have a loom-daemon installed; (0) proves the fixture cannot reach it.
# Linux: the renewer census reads `ps` and the interrupt uses `setsid`.

set -uo pipefail

TEST_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SCRIPTS="$(cd "$TEST_DIR/.." && pwd)"
GREEN='\033[0;32m'; RED='\033[0;31m'; NC='\033[0m'
TESTS_RUN=0; TESTS_FAILED=0
pass() { TESTS_RUN=$((TESTS_RUN + 1)); echo -e "  ${GREEN}PASS${NC}: $1"; }
fail() { TESTS_RUN=$((TESTS_RUN + 1)); TESTS_FAILED=$((TESTS_FAILED + 1)); echo -e "  ${RED}FAIL${NC}: $1"; }
assert_eq() { # <expected> <actual> <label>
    if [[ "$1" == "$2" ]]; then pass "$3"; else fail "$3 (expected '$1', got '$2')"; fi
}
assert_has() { # <haystack-file> <needle> <label>
    if grep -qF -- "$2" "$1" 2>/dev/null; then pass "$3"; else fail "$3 ('$2' not in ${1##*/})"; fi
}
assert_pure_json() { # <stdout-file> <label>
    local f="$1" l="$2" count
    assert_eq "{" "$(head -c1 "$f")" "$l: stdout begins with '{' (no leading noise)"
    if count="$(jq -s 'length' "$f" 2>/dev/null)" && [[ "$count" == "1" ]]; then
        pass "$l: stdout is exactly one JSON value"
    else
        fail "$l: stdout is not a single JSON value — content: $(cat "$f")"
    fi
    assert_eq true "$(jq -r '.success' "$f" 2>/dev/null)" "$l: .success == true"
}
# assert_doc_keys <stdout-file> <label> — the key set every --json success
# document shares (worktree.sh's new-branch, reuse and reconfigure answers).
assert_doc_keys() {
    assert_eq '["branchName","cone","issueNumber","sparse","success","worktreePath"]' \
        "$(jq -S -c 'keys' "$1" 2>/dev/null)" "$2: JSON key set is the documented shape"
}

TOOLS_SRC="${LOOM_TOOLS_TEST_BIN:-}"
if [[ -z "$TOOLS_SRC" || ! -x "$TOOLS_SRC" ]]; then
    echo "SKIP: LOOM_TOOLS_TEST_BIN does not name a built loom-tools"
    exit 0
fi
command -v jq >/dev/null 2>&1 || { echo "SKIP: jq not available"; exit 0; }
TOOLS_SRC="$(readlink -f "$TOOLS_SRC")"

# --- The sandbox: every path below it contains a space ----------------------
T="$(mktemp -d)"
T="$(cd "$T" && pwd -P)"
CO="$T/checkout with spaces"
BIN="$T/bin"; INSTALL="$T/install"; FORGE="$T/forge"
HOME_DIR="$T/home"; CARGO="$T/cargo target with spaces"
SESSION_PID=""
# LOOM_TOOLS_WORKTREE_KEEP=1 leaves the sandbox in place for inspection.
cleanup() {
    [[ -z "$SESSION_PID" ]] || kill "$SESSION_PID" 2>/dev/null || true
    pkill -f "$T/" 2>/dev/null || true
    if [[ "${LOOM_TOOLS_WORKTREE_KEEP:-}" == "1" ]]; then echo "kept sandbox: $T" >&2; else rm -rf "$T" 2>/dev/null || true; fi
}
trap cleanup EXIT
mkdir -p "$BIN" "$INSTALL" "$FORGE" "$HOME_DIR" "$CO/.loom"
printf '[http "https://github.com/"]\n\tproxy = 127.0.0.1:1\n' > "$HOME_DIR/.gitconfig"

# The install: the tools binary in the machine-install directory (NOT on
# PATH, so resolution has to find it where provision-tools.sh puts it), and
# the shipped scripts, as resync-payload materialises them. The binary is
# tapped: the wrapper records every argv it is exec'd with, which is how each
# section shows which installed helper a guard actually ran through.
cat > "$INSTALL/loom-tools" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >> "$T/tools.log"
if [[ "\$1 \$2" == "worktree-lock acquire" && -e "$T/hold-acquire" ]]; then
    out="\$("$TOOLS_SRC" "\$@")"; rc=\$?
    : > "$T/held"
    sleep 120
    printf '%s' "\$out"; exit \$rc
fi
exec "$TOOLS_SRC" "\$@"
EOF
chmod +x "$INSTALL/loom-tools"
cp -r "$SCRIPTS" "$CO/.loom/scripts"
rm -rf "$CO/.loom/scripts/tests"

# A stateful stub forge: the shape test-loom-tools-lease.sh drives the lease
# chain with (identity, pulls, comments, state, permissions; POST appends,
# PATCH renews), plus the two `gh` forms the claim and branch probes make.
cat > "$BIN/gh" <<'STUB'
#!/usr/bin/env bash
D="${LOOM_TEST_FORGE:?stub gh: LOOM_TEST_FORGE not set}"
echo "$*" >> "$D/calls.log"
[[ ! -f "$D/reads-fail" ]] || { echo "stub gh: read failed" >&2; exit 1; }
state=""; n=""
if [[ "$1" == "pr" && "$2" == "list" ]]; then
    [[ ! -f "$D/pr-fail" ]] || { echo "stub gh: pr list failed" >&2; exit 1; }
    while [[ $# -gt 0 ]]; do [[ "$1" == "--state" ]] && state="$2"; shift; done
    [[ ! -f "$D/pr-$state.json" ]] || { cat "$D/pr-$state.json"; exit 0; }
    printf '[]\n'; exit 0
fi
if [[ "$1" == "issue" && "$2" == "view" ]]; then
    n="$3"; labels="[]"
    [[ ! -f "$D/issue-labels" ]] || labels="$(jq -R . "$D/issue-labels" | jq -s 'map({name: .})')"
    jq -nc --argjson l "$labels" --arg s "$(cat "$D/issue-state" 2>/dev/null || echo open)" --arg n "$n" \
        '{labels: $l, state: (if $s == "closed" then "CLOSED" else "OPEN" end),
          url: ("https://github.com/acme/widget/issues/" + $n)}'
    exit 0
fi
[[ "$1" == "api" ]] || { echo "stub gh: unhandled: $*" >&2; exit 3; }
shift
method="GET"; path=""; jq_filter=""; body=""; have_body=""; include=0; operation=""
while [[ $# -gt 0 ]]; do
  case "$1" in
    --method|-X) method="$2"; shift 2 ;;
    --paginate|--silent) shift ;;
    --include|-i) include=1; shift ;;
    --jq) jq_filter="$2"; shift 2 ;;
    -f|-F) [[ "${2:-}" == body=* ]] && { body="${2#body=}"; have_body=1; }; shift 2 ;;
    -H|--header|--hostname) shift 2 ;;
    *) [[ -n "$path" ]] || path="$1"; shift ;;
  esac
done
[[ "$body" == "@-" ]] && body="$(cat)"
if [[ "$method" == "GET" && "$path" == repos/*/pulls\?* ]]; then
  [[ ! -f "$D/linked-fail" ]] || { echo "stub gh: linked-PR read failed" >&2; exit 1; }
  [[ "$include" -eq 0 ]] || printf 'HTTP/2 200 OK\r\n\r\n'
  repo="${path#repos/}"; repo="${repo%%/pulls\?*}"
  f="$D/${repo//\//-}-pulls.json"
  if [[ -f "$f" ]]; then cat "$f"; else printf '[]\n'; fi
  exit 0
fi
if [[ "$method" == "GET" && "$path" =~ ^repos/[^/]+/[^/]+$ && "$include" -eq 1 ]]; then
  repo="${path#repos/}"
  printf 'HTTP/2 200 OK\r\n\r\n'
  jq -nc --arg repo "$repo" '{id:101,full_name:$repo,owner:{login:($repo|split("/")[0])},name:($repo|split("/")[1])}'
  exit 0
fi
if [[ "$path" == "graphql" ]]; then
  [[ ! -f "$D/linked-fail" ]] || { echo "stub gh: linked-PR read failed" >&2; exit 1; }
  printf '%s\n' '{"data":{"repository":{"issue":{"closedByPullRequestsReferences":{"nodes":[]}}}}}'
  exit 0
fi
if [[ "$method" == "GET" && "$path" == repos/*/issues/*/timeline ]]; then
  [[ ! -f "$D/linked-fail" ]] || { echo "stub gh: linked-PR read failed" >&2; exit 1; }
  printf '[]\n'; exit 0
fi
case "$method $path" in
  "GET repos/acme/widget") echo '{"push":true}'; exit 0 ;;
  GET\ repos/*/issues/*/comments|GET\ repos/*/issues/*/comments\?*) operation=comments ;;
  GET\ repos/*/issues/*) operation=state ;;
  POST\ repos/*/issues/*/comments)
    case "$body" in "<!-- loom:lease "*) operation=publish ;; esac ;;
  PATCH\ repos/acme/widget/issues/comments/*) operation=renew ;;
esac
if [[ "$operation" == "comments" ]]; then
  [[ ! -f "$D/comments-fail" ]] || { echo "stub gh: comments read failed" >&2; exit 1; }
  [[ -f "$D/comments.json" ]] || echo '[]' > "$D/comments.json"
  if [[ -n "$jq_filter" ]]; then jq -c "$jq_filter" "$D/comments.json"; else cat "$D/comments.json"; fi
  exit 0
fi
if [[ "$operation" == "state" ]]; then
  s="$(cat "$D/issue-state" 2>/dev/null || echo open)"
  if [[ "$jq_filter" == ".state" ]]; then echo "$s"; else printf '{"state":"%s"}\n' "$s"; fi
  exit 0
fi
if [[ "$method" == "POST" && "$path" == repos/*/issues/*/comments ]]; then
  [[ -n "$have_body" ]] || { echo "stub gh: POST without a body" >&2; exit 1; }
  [[ -f "$D/comments.json" ]] || echo '[]' > "$D/comments.json"
  n=$(( $(jq 'length' "$D/comments.json") + 1000 ))
  jq --arg b "$body" --argjson id "$n" --arg t "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    '. + [{id: $id, body: $b, created_at: $t, updated_at: $t,
           user: {login: "loom-fleet-dispatch[bot]", type: "Bot"}, author_association: "CONTRIBUTOR"}]' \
    "$D/comments.json" > "$D/c.tmp" && mv "$D/c.tmp" "$D/comments.json"
  echo "POST $path" >> "$D/posts.log"
  echo "{\"id\": $n}"; exit 0
fi
if [[ "$method" == "PATCH" && "$path" == repos/*/issues/comments/* ]]; then
  id="${path##*/}"
  jq --argjson id "$id" --arg t "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    'map(if .id == $id then .updated_at = $t else . end)' "$D/comments.json" > "$D/c.tmp" \
    && mv "$D/c.tmp" "$D/comments.json"
  echo "$id" >> "$D/patches.log"; echo '{}'; exit 0
fi
echo "stub gh: unhandled: $method $path" >&2; exit 3
STUB
chmod +x "$BIN/gh"

# isov — set ISOV to the `env -i` invocation every child runs through; iso
# runs a child with it. Nothing is inherited: only what is listed here
# exists, so no binary override, fleet routing variable or service setting can
# leak in from the runner. The only git config is the fixture's own: it routes
# every github.com transfer to a closed loopback port, so the forge-shaped
# origin of section (3) is unreachable in about a millisecond instead of
# depending on the host's network (a stalled connect has no git timeout).
isov() {
    ISOV=(env -i HOME="$HOME_DIR" PATH="$BIN:/usr/bin:/bin" \
        LOOM_DAEMON_BIN_DIR="$INSTALL" CARGO_TARGET_DIR="$CARGO" \
        LOOM_LISTING_CACHE_DIR="$T/listing-cache" LOOM_LEASE_RENEW_STATE_DIR="$T/renew-state" \
        LOOM_HOST_ID=tools-host SWEEP_LEASE_RENEW_INTERVAL_SECS=1 LOOM_TEST_FORGE="$FORGE" \
        GIT_TERMINAL_PROMPT=0 GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL="$HOME_DIR/.gitconfig" \
        ${LOOM_GH_BIN:+LOOM_GH_BIN="$LOOM_GH_BIN"} \
        ${LOOM_WRITE_SCOPE_CACHE_DIR:+LOOM_WRITE_SCOPE_CACHE_DIR="$LOOM_WRITE_SCOPE_CACHE_DIR"})
}
iso() { isov; "${ISOV[@]}" "$@"; }
WT="$CO/.loom/scripts/worktree.sh"
# wt <VAR=..>... <worktree.sh args...> -> RC, stdout in $T/out, stderr in $T/err,
# and every argv the installed tools binary was exec'd with in $T/tools.log.
wt() {
    local envs=()
    while [[ $# -gt 0 && "$1" == *=* ]]; do envs+=("$1"); shift; done
    : > "$T/tools.log"
    ( cd "$CO" && iso ${envs[@]+"${envs[@]}"} bash "$WT" "$@" >"$T/out" 2>"$T/err" ); RC=$?
}
count_lines() { if [[ -f "$1" ]]; then wc -l < "$1" | tr -d ' '; else echo 0; fi; }
posts() { count_lines "$FORGE/posts.log"; }
# registrations <issue> — how many git worktree entries end in issue-<N>.
registrations() { git -C "$CO" worktree list --porcelain | grep -c "^worktree .*/issue-$1\$"; }
json_path() { jq -r '.worktreePath // empty' "$T/out" 2>/dev/null; }
session() { sleep 600 >/dev/null 2>&1 & SESSION_PID=$!; }
end_session() { [[ -z "$SESSION_PID" ]] || kill "$SESSION_PID" 2>/dev/null || true; SESSION_PID=""; }
wait_for() { # <seconds> <command...>
    local n=$(($1 * 4)); shift
    while ((n-- > 0)); do "$@" && return 0; sleep 0.25; done
    return 1
}
# The detached renewal loop's root processes (the census test-loom-tools-lease.sh uses).
renewer_pids() {
    ps -axww -o pid= -o ppid= -o stat= -o command= | awk -v start="$CO/.loom/scripts/sweep-lease-renew.sh start " '
        { parent[$1] = $2
          if ($3 ~ /^Z/ || $4 !~ /(^|\/)bash$/) next
          command = $0
          sub(/^[[:space:]]*[0-9]+[[:space:]]+[0-9]+[[:space:]]+[^[:space:]]+[[:space:]]+[^[:space:]]+[[:space:]]+/, "", command)
          if (index(command, start) == 1) matching[$1] = 1 }
        END { for (pid in matching) { a = parent[pid]
               while (a > 1 && a in parent && !(a in matching)) a = parent[a]
               if (!(a in matching)) print pid } }' | sort -n
}
renewers() { renewer_pids | wc -l | tr -d ' '; }
no_renewers() { [[ "$(renewers)" -eq 0 ]]; }
has_renewer() { [[ "$(renewers)" -ge 1 ]]; }
lock_held() { [[ -e "$T/held" ]]; }

git -C "$CO" init -q -b main
git -C "$CO" config user.email t@example.invalid
git -C "$CO" config user.name t
git -C "$CO" commit -q --allow-empty -m init
git init -q -b main "$T/origin.git" --bare
git -C "$CO" push -q "$T/origin.git" main
git -C "$CO" remote add origin "$T/origin.git"
git -C "$CO" fetch -q origin
# What a clone leaves behind: the default branch then resolves locally.
git -C "$CO" symbolic-ref refs/remotes/origin/HEAD refs/remotes/origin/main
# The scripts and the worktree root are untracked in the checkout; ignore them
# so a dirty tree never comes from the fixture itself.
printf '.loom/\n' > "$CO/.gitignore"

# ===========================================================================
echo "(0) the fixture is a tools-only host: no daemon in any locator"
iso bash -c 'source "$1/.loom/scripts/lib/locate-daemon-bin.sh"; LOOM_LOCATE_DAEMON_BIN_QUIET=1
    echo "self=$(loom_resolve_self_daemon_bin)"
    echo "locate=$(loom_locate_daemon_bin "$1")"
    echo "pinned=$(LOOM_DAEMON_BIN="$1/no-such-daemon" loom_locate_daemon_bin "$1")"
    echo "onpath=$(command -v loom-daemon)"
    echo "build=$(LOOM_PREFER_REPO_BUILD=1 loom_locate_daemon_bin "$1")"
    echo "tools=$(loom_locate_tools_bin "$1")"' _ "$CO" > "$T/locators"
loc() { sed -n "s/^$1=//p" "$T/locators"; }
assert_eq "" "$(loc self)" "(0) loom_resolve_self_daemon_bin resolves nothing (the create path's resolution)"
assert_eq "" "$(loc locate)" "(0) loom_locate_daemon_bin resolves nothing (the cargo-target helper's)"
assert_eq "" "$(loc pinned)" "(0) a non-executable \$LOOM_DAEMON_BIN pin is not a daemon"
assert_eq "" "$(loc onpath)" "(0) no loom-daemon on the isolated PATH (the branch-landed / forge-pr-check fallbacks)"
assert_eq "" "$(loc build)" "(0) LOOM_PREFER_REPO_BUILD=1 finds no checkout build"
assert_eq "$INSTALL/loom-tools" "$(loc tools)" "(0) loom-tools resolves from the machine-install directory"
assert_eq 0 "$(iso bash -c 'source "$1/.loom/scripts/lib/locate-daemon-bin.sh"; loom_tools_backstops worktree-lock && loom_tools_backstops worktree-remove && loom_tools_backstops worktree-check' _ "$CO" >/dev/null 2>&1; echo $?)" \
    "(0) the worktree family is backstopped"
assert_eq 1 "$(iso bash -c 'source "$1/.loom/scripts/lib/locate-daemon-bin.sh"; loom_tools_backstops worktree-wip' _ "$CO" >/dev/null 2>&1; echo $?)" \
    "(0) worktree-wip is not: the WIP verbs stay on the daemon"

# ===========================================================================
echo "(1) create, enter, reuse and remove through the normal entrypoint"
wt --json 101
assert_eq 0 "$RC" "(1) worktree.sh --json 101 exits 0"
assert_pure_json "$T/out" "(1) create"
assert_doc_keys "$T/out" "(1) create"
P="$(json_path)"
assert_eq "$CO/.loom/worktrees/issue-101" "$P" "(1) worktreePath is the issue dir, inside the path with a space"
assert_eq "feature/issue-101" "$(cd "$P" && git branch --show-current)" "(1) entering the returned path lands on feature/issue-101"
assert_has "$P/.loom-managed" "Issue: 101" "(1) the managed sentinel names the issue"
assert_eq 1 "$(registrations 101)" "(1) git registers the worktree exactly once"
for sub in "worktree-check --help" "worktree-cleanup 101" "worktree-lock acquire --issue 101" \
           "worktree-lock release" "worktree-lock check-issue --issue 101" "worktree-base --help" \
           "worktree-link --repo-root" "worktree-submodules --repo-root"; do
    assert_has "$T/tools.log" "$sub" "(1) ran on the installed helper: $sub"
done
assert_has "$T/tools.log" "forge check-claim 101 --force-claim" "(1) the fresh-branch claim probe ran"
wt --json 101
assert_eq 0 "$RC" "(1) re-running against the existing worktree exits 0"
assert_pure_json "$T/out" "(1) reuse"
assert_eq "$P" "$(json_path)" "(1) reuse returns the same path"
assert_eq 1 "$(registrations 101)" "(1) reuse registers nothing more"
assert_has "$T/tools.log" "worktree-existing --worktree" "(1) reuse ran through worktree-existing"
wt remove 101
assert_eq 0 "$RC" "(1) remove exits 0"
[[ ! -d "$CO/.loom/worktrees/issue-101" ]] && pass "(1) the directory is gone" || fail "(1) the directory survives remove"
assert_eq 0 "$(registrations 101)" "(1) git no longer registers it"
git -C "$CO" show-ref --verify --quiet refs/heads/feature/issue-101 && fail "(1) the local branch survived" || pass "(1) the local branch is deleted"
wt remove 101
assert_eq 0 "$RC" "(1) removing an absent worktree is the idempotent no-op"
assert_has "$T/out" "nothing to remove" "(1) the no-op says so"

# ===========================================================================
echo "(2) crash cleanup, an interrupted create, and a competing lock"
LOCK_DIR="$CO/.loom/locks/worktree-add"
mkdir -p "$CO/.loom/worktrees/issue-102" "$CO/.git/worktrees/issue-102"
touch "$CO/.git/worktrees/issue-102/index.lock" "$CO/.loom/worktrees/issue-102/half-written"
wt --json 102
assert_eq 0 "$RC" "(2) creation succeeds over a stale index.lock and an unregistered issue-102 dir"
[[ ! -e "$CO/.git/worktrees/issue-102/index.lock" && ! -e "$CO/.loom/worktrees/issue-102/half-written" ]] \
    && pass "(2) worktree-cleanup removed both pieces of crash debris" || fail "(2) crash debris survived"
assert_eq 1 "$(registrations 102)" "(2) exactly one checkout exists afterwards"
assert_has "$(json_path)/.loom-managed" "Issue: 102" "(2) the recovered checkout carries the sentinel"
wt remove 102; assert_eq 0 "$RC" "(2) cleanup: removed 102"

# Interrupt the creator AFTER the lock is taken (the tap holds acquire's answer
# and sleeps) and before `git worktree add`. KILL lets no EXIT trap run, so the
# add-lock is left owned by a dead pid. setsid gives the creator its own
# process group, so the kill cannot reach this suite.
: > "$T/hold-acquire"; isov
( cd "$CO" && exec setsid "${ISOV[@]}" bash "$WT" 103 >"$T/out" 2>"$T/err" ) &
CREATOR=$!
wait_for 20 lock_held
assert_eq 0 "$?" "(2) the creator reached the point of holding the lock"
pkill -9 -g "$CREATOR" 2>/dev/null
wait "$CREATOR" 2>/dev/null
rm -f "$T/hold-acquire" "$T/held"
[[ -d "$LOCK_DIR" ]] && pass "(2) the killed creator left the add-lock behind" || fail "(2) no lock to recover from (the kill did not land after acquisition)"
[[ -e "$CO/.loom/worktrees/issue-103/.git" ]] && fail "(2) the interrupted create left a checkout" || pass "(2) the interrupted create registered nothing"
wt --json 103
assert_eq 0 "$RC" "(2) the retry succeeds"
assert_pure_json "$T/out" "(2) retry"
assert_eq 1 "$(registrations 103)" "(2) the retry leaves exactly one managed checkout"
assert_has "$(json_path)/.loom-managed" "Issue: 103" "(2) that checkout carries the sentinel"
[[ ! -d "$LOCK_DIR" ]] && pass "(2) the retry released the recovered lock" || fail "(2) the add-lock is still held after a successful create"
wt remove 103; assert_eq 0 "$RC" "(2) cleanup: removed 103"

# A live competing lock: the refused caller must not take it with it.
session
mkdir -p "$LOCK_DIR"
printf '{"issue": 999, "owner_pid": %s, "token": "theirs", "script": "test"}\n' "$SESSION_PID" > "$LOCK_DIR/owner.json"
wt LOOM_WORKTREE_LOCK_TIMEOUT=1 LOOM_WORKTREE_LOCK_POLL_INTERVAL=1 --json 104
assert_eq 1 "$RC" "(2) a live competing lock refuses the create"
assert_has "$T/out" '"error": "worktree-lock-timeout"' "(2) the refusal is the documented --json document"
assert_eq "$SESSION_PID" "$(jq -r '.holderPid' "$T/out" 2>/dev/null)" "(2) the document names the live holder"
assert_eq "theirs" "$(jq -r '.token' "$LOCK_DIR/owner.json" 2>/dev/null)" "(2) the live holder's lock and token survive the refused caller"
[[ ! -e "$CO/.loom/worktrees/issue-104/.git" ]] && pass "(2) nothing was created against a live lock" || fail "(2) a checkout appeared despite the live lock"
rm -rf "$LOCK_DIR"; end_session

# ===========================================================================
echo "(3) the create-time guards against a controlled forge"
# shellcheck source=lib/write-scope-fixture.sh
source "$TEST_DIR/lib/write-scope-fixture.sh"
write_scope_register "$CO" acme/widget
assert_eq "https://github.com/acme/widget.git" "$(git -C "$CO" remote get-url origin)" "(3) the checkout now has a forge-shaped origin"

# 3a. a blocked claim: the issue is CLOSED, a leg --force-claim never overrides
printf 'closed\n' > "$FORGE/issue-state"
wt LOOM_MODE=session --json 201
assert_eq 1 "$RC" "(3a) a blocked claim refuses the create"
assert_has "$T/out" "CLOSED" "(3a) the guard's reason token reaches the caller"
[[ ! -e "$CO/.loom/worktrees/issue-201/.git" ]] && pass "(3a) no worktree" || fail "(3a) a worktree was created"
git -C "$CO" show-ref --verify --quiet refs/heads/feature/issue-201 && fail "(3a) a branch was created" || pass "(3a) no branch"
assert_eq 0 "$(posts)" "(3a) a refused create publishes no lease"
assert_has "$T/tools.log" "forge check-claim 201 --force-claim" "(3a) the probe ran through the installed loom-tools"

# 3b. a clear claim creates, and the attended lease is published once
printf 'open\n' > "$FORGE/issue-state"
session
wt LOOM_MODE=session LOOM_AGENT_SESSION_PID="$SESSION_PID" CLAUDE_PID="$SESSION_PID" --json 201
assert_eq 0 "$RC" "(3b) a clear claim creates the worktree"
assert_pure_json "$T/out" "(3b) clear claim"
assert_eq 1 "$(registrations 201)" "(3b) registered once"
assert_has "$T/tools.log" "lease ensure 201 --watch-pid $SESSION_PID" "(3b) the helper asked for the attended lease"
wait_for 10 has_renewer
assert_eq 1 "$(renewers)" "(3b) the attended lease started one renewer"
assert_eq 1 "$(posts)" "(3b) exactly one lease comment was posted"
wt LOOM_MODE=session LOOM_AGENT_SESSION_PID="$SESSION_PID" CLAUDE_PID="$SESSION_PID" --json 201
assert_eq 0 "$RC" "(3b) the reuse arm also leases"
assert_eq 1 "$(posts)" "(3b) the reuse arm posted no second lease"
assert_eq 1 "$(renewers)" "(3b) nor started a second renewer"
end_session
wait_for 15 no_renewers
assert_eq 0 "$(renewers)" "(3b) the renewer stopped when the watched session ended"
assert_eq 0 "$( cd "$CO" && iso LOOM_MODE=session "$INSTALL/loom-tools" lease release 201 >/dev/null 2>&1; echo $?)" "(3b) the lease releases"

# 3c. the outcomes of the claim probe, and each caller's policy on them
probe() { # <mode|empty> <issue> -> the guard's own exit status
    ( cd "$CO" && iso ${1:+LOOM_MODE="$1"} "$INSTALL/loom-tools" forge check-claim "$2" --force-claim >/dev/null 2>&1 ); echo $?
}
printf 'closed\n' > "$FORGE/issue-state"
assert_eq 0 "$(probe session 202)" "(3c) blocked: exit 0 (the Builder hard-aborts; the wrapper refuses)"
printf 'open\n' > "$FORGE/issue-state"
assert_eq 1 "$(probe session 202)" "(3c) clear: exit 1 (the Builder claims; the wrapper creates)"
# An unanswered read: the one leg --force-claim cannot override (the linked-PR
# leg) fails under it, while `gh pr list` — the wrapper's own probe — answers.
: > "$FORGE/linked-fail"
UNANSWERED="$(probe session 202)"
assert_eq 5 "$UNANSWERED" "(3c) unanswered: exit 5 — the Builder stops on it, the wrapper continues"
# The wrapper's own policy on an unanswered read is fail-open, as on a forge
# outage: the probe cannot refuse, and the create goes ahead.
wt LOOM_MODE=session --json 202
rm -f "$FORGE/linked-fail"
[[ "$RC" -eq 0 ]] && pass "(3c) the wrapper continues past an unanswered claim read" || fail "(3c) the wrapper refused on an unanswered read (rc=$RC): $(head -c 200 "$T/out")"
wt LOOM_MODE=session remove 202 --force; assert_eq 0 "$RC" "(3c) cleanup: removed 202"
# A mode the gate cannot resolve is an unanswered read too — never a clear one.
assert_eq 5 "$(probe "" 203)" "(3c) a mode error exits 5, not the 1 that means clear"
wt --json 203
[[ "$RC" -ne 0 && ! -e "$CO/.loom/worktrees/issue-203/.git" ]] && pass "(3c) with mode unset the wrapper refuses rather than create blind" \
    || fail "(3c) mode unset: rc=$RC, a worktree was handed over"
assert_has "$T/out" '"error":"forge-check-unavailable"' "(3c) by the fresh-branch probe, which cannot be skipped"

# 3d. the wrapper's fresh-branch probe (a different guard, its own policy)
cat > "$FORGE/pr-open.json" <<'EOF'
[{"number": 9, "isCrossRepository": true, "headRepository": {"nameWithOwner": "fork/loom", "owner": {"login": "fork"}, "name": "loom"}, "headRefName": "feature/issue-204", "url": "https://github.com/acme/widget/pull/9"}]
EOF
wt LOOM_MODE=session --json 204
assert_eq 1 "$RC" "(3d) a fork PR owning the branch name refuses a fresh branch"
assert_has "$T/out" '"error":"shadowed-cross-repo-pr"' "(3d) the refusal is the #7765 document"
assert_has "$T/tools.log" "worktree-open-pr --branch feature/issue-204" "(3d) the probe ran through worktree-open-pr"
rm -f "$FORGE/pr-open.json"; : > "$FORGE/pr-fail"
wt LOOM_MODE=session --json 204
assert_eq 1 "$RC" "(3d) a forge that cannot be asked refuses too (never a guess)"
assert_has "$T/out" '"error":"forge-check-unavailable"' "(3d) the #7863 distinction: unavailable is not not_found"
rm -f "$FORGE/pr-fail"
wt LOOM_MODE=session --json 204
assert_eq 0 "$RC" "(3d) a confirmed absence creates the fresh branch"
wt LOOM_MODE=session remove 204; assert_eq 0 "$RC" "(3d) cleanup: removed 204"

# 3e. branch reuse: the LOCAL-branch arm runs through worktree-branch-reuse
git -C "$CO" branch feature/issue-205
wt LOOM_MODE=session 205
assert_eq 0 "$RC" "(3e) an existing local branch is reused"
assert_eq 1 "$(registrations 205)" "(3e) registered once"
assert_has "$T/out" "already exists - reusing it" "(3e) the reuse warning is printed"
assert_has "$T/tools.log" "worktree-branch-reuse --repo" "(3e) through worktree-branch-reuse"
wt LOOM_MODE=session remove 205 --keep-branch; assert_eq 0 "$RC" "(3e) cleanup: removed 205"
# ...and refused when that branch already landed (#8280): a commit of its own
# (a different tree, so no local tree-equality answer) whose tip is a merged
# PR's head. The forge read goes to the stub, through the installed binary.
git -C "$CO" branch -D feature/issue-205 >/dev/null
blob="$(printf 'landed work\n' | git -C "$CO" hash-object -w --stdin)"
tree="$(printf '100644 blob %s\tlanded.txt\n' "$blob" | git -C "$CO" mktree)"
landed="$(git -C "$CO" commit-tree "$tree" -p main -m 'a commit of its own')"
git -C "$CO" branch feature/issue-207 "$landed"
printf '[{"headRefOid": "%s", "number": 77}]\n' "$landed" > "$FORGE/pr-merged.json"
: > "$FORGE/calls.log"
wt LOOM_MODE=session --json 207
assert_eq 1 "$RC" "(3e) a local branch whose tip is a merged PR's head is not reused"
[[ ! -e "$CO/.loom/worktrees/issue-207/.git" ]] && pass "(3e) nothing was created" || fail "(3e) a worktree was created for a landed branch"
assert_has "$T/out" '"error":"branch-already-landed"' "(3e) the refusal is the #8280 --json document"
assert_has "$T/out" '"prNumber":77' "(3e) naming the merged PR"
assert_has "$FORGE/calls.log" "pr list --head feature/issue-207 --state merged" "(3e) the landed check read the forge through loom-tools, not a host daemon"
rm -f "$FORGE/pr-merged.json"
git -C "$CO" branch -D feature/issue-207 >/dev/null

# 3f. the claim lock, cross-checked through worktree-lock check-issue
session
mkdir -p "$CO/.loom/locks/issue-206"
printf '{"issue": 206, "owner_pid": %s, "acquired_at": "2026-01-01T00:00:00Z", "sweep_id": "sweep-206-live"}\n' "$SESSION_PID" > "$CO/.loom/locks/issue-206/owner.json"
wt LOOM_MODE=session 206
assert_eq 1 "$RC" "(3f) a live claim lock refuses the create"
assert_has "$T/err" "sweep-206-live" "(3f) the refusal names the sweep"
wt LOOM_MODE=session 206 --force
assert_eq 0 "$RC" "(3f) --force proceeds past it"
assert_has "$T/err" "force" "(3f) and says so"
end_session
rm -rf "$CO/.loom/locks/issue-206"
wt LOOM_MODE=session remove 206 --force; assert_eq 0 "$RC" "(3f) cleanup: removed 206"

# ===========================================================================
echo "(4) removal refusal for unmanaged and dirty worktrees"
wt LOOM_MODE=session --json 301
assert_eq 0 "$RC" "(4) setup: a managed worktree exists"
D="$CO/.loom/worktrees/issue-301"
rm -f "$D/.loom-managed"
wt LOOM_MODE=session remove 301
assert_eq 1 "$RC" "(4) an unmanaged worktree is refused"
assert_has "$T/err" "refusing to remove" "(4) the refusal says so"
[[ -d "$D" && -e "$D/.git" ]] && pass "(4) the unmanaged directory and its registration survive" || fail "(4) the unmanaged worktree was removed"
wt LOOM_MODE=session --json 301   # the reuse arm back-fills the sentinel (#3548)
[[ -f "$D/.loom-managed" ]] && pass "(4) the sentinel is back-filled on reuse" || fail "(4) sentinel not back-filled"
echo "uncommitted work" > "$D/wip.txt"
wt LOOM_MODE=session remove 301
assert_eq 1 "$RC" "(4) a dirty worktree is refused"
assert_has "$T/err" "Refusing to remove" "(4) the dirty refusal is printed"
assert_has "$T/err" "wip.txt" "(4) it lists what it found"
[[ -f "$D/wip.txt" && -d "$D" ]] && pass "(4) the work and the worktree survive" || fail "(4) the dirty worktree was removed"
wt LOOM_MODE=session remove 301 --json
assert_eq 1 "$RC" "(4) --json refuses too"
assert_eq false "$(jq -r '.removed' "$T/out" 2>/dev/null)" "(4) with a parseable removed=false document on stdout"
wt LOOM_MODE=session remove 301 --force
assert_eq 0 "$RC" "(4) --force removes it"
[[ ! -d "$D" && "$(registrations 301)" -eq 0 ]] && pass "(4) gone from the filesystem and git" || fail "(4) the forced removal left something behind"
# The "has this branch landed?" read needs the forge for a branch with commits
# of its own. With mode unset it stops AFTER the worktree is removed; the local
# branch is kept, because its delete is the leg that could not be decided.
wt LOOM_MODE=session --json 303
assert_eq 0 "$RC" "(4) setup: a worktree to commit in"
echo "own work" > "$CO/.loom/worktrees/issue-303/own.txt"
git -C "$CO/.loom/worktrees/issue-303" add own.txt
git -C "$CO/.loom/worktrees/issue-303" commit -q -m "a commit of its own"
wt remove 303
assert_eq 1 "$RC" "(4) a mode error stops the landed read of a branch with commits of its own"
assert_has "$T/err" '`mode` is unset' "(4) and the error names the fix"
[[ ! -d "$CO/.loom/worktrees/issue-303" ]] && pass "(4) the worktree itself was removed" || fail "(4) the worktree survived"
git -C "$CO" show-ref --verify --quiet refs/heads/feature/issue-303 && pass "(4) the local branch is kept" || fail "(4) the local branch was deleted unverified"
wt LOOM_MODE=session remove 303
assert_eq 0 "$RC" "(4) the retry is the no-op, and still leaves the branch"
git -C "$CO" branch -D feature/issue-303 >/dev/null

# ===========================================================================
echo "(5) legacy invocations stay compatible; the daemon-only verbs still refuse"
# A pinned daemon still wins: the backstop is only for a host with none.
STUB_DAEMON="$T/stub-daemon"
cat > "$STUB_DAEMON" <<EOF
#!/usr/bin/env bash
echo "\$*" >> "$T/stub.log"
exit 2
EOF
chmod +x "$STUB_DAEMON"; : > "$T/stub.log"
( cd "$CO" && iso LOOM_DAEMON_SELF_BIN="$STUB_DAEMON" LOOM_DAEMON_BIN="$STUB_DAEMON" bash "$WT" remove 301 >"$T/out" 2>"$T/err" ); RC=$?
assert_eq 2 "$RC" "(5) a pinned daemon is honoured for remove, ahead of the backstop"
assert_has "$T/stub.log" "worktree-remove 301" "(5) by the pinned binary"
: > "$T/tools.log"
( cd "$CO" && iso LOOM_DAEMON_SELF_BIN="$STUB_DAEMON" LOOM_DAEMON_BIN="$STUB_DAEMON" bash "$WT" --json 303 >"$T/out" 2>"$T/err" ); RC=$?
assert_eq 0 "$RC" "(5) a pinned daemon is honoured for create too"
assert_has "$T/stub.log" "worktree-cleanup 303" "(5) the create path resolved the pin"
assert_eq "" "$(cat "$T/tools.log")" "(5) loom-tools was not consulted"
wt LOOM_MODE=session remove 303; assert_eq 0 "$RC" "(5) cleanup: removed 303"
# The tools binary keeps the daemon's own command surface.
assert_eq 0 "$( cd "$CO" && iso LOOM_MODE=session "$INSTALL/loom-tools" worktree-remove 301 >/dev/null 2>&1; echo $?)" \
    "(5) the direct worktree-remove invocation is a compatibility delegate"
# worktree-wip is daemon-only by design (§13.10): without one it still refuses.
wt snapshot 301
assert_eq 2 "$RC" "(5) snapshot needs a daemon: exit 2, unchanged"
assert_has "$T/err" "loom-daemon not found (needed for 'worktree-wip')" "(5) and names what is missing"
# --check is of the family and runs on loom-tools.
wt LOOM_MODE=session --json 302
assert_eq 0 "$RC" "(5) setup: a worktree to stand in"
( cd "$CO/.loom/worktrees/issue-302" && iso LOOM_MODE=session bash "$WT" --check >"$T/out" 2>"$T/err" ); RC=$?
assert_eq 0 "$RC" "(5) --check answers from inside a worktree"
assert_has "$T/out" "feature/issue-302" "(5) naming its branch"
wt LOOM_MODE=session remove 302; assert_eq 0 "$RC" "(5) cleanup: removed 302"

# --- Summary ----------------------------------------------------------------
echo ""
echo "Tests run: $TESTS_RUN, Failed: $TESTS_FAILED"
if [[ "$TESTS_FAILED" -eq 0 ]]; then echo "ALL PASSED"; else exit 1; fi
