#!/usr/bin/env bash
# Guard: files Loom ships to consumers must not name the pre-transfer owner slug.
#
# Usage: ./tests/install/test-no-old-owner-slug.sh
#
# Loom moved from rjwalters/loom to 2AMLogic/loom (org transfer, #14170).
# Everything an install or resync copies into a consumer repo -- defaults/, the
# installer and uninstaller, the CLAUDE.md/README.md text, the daemon's
# install-time scaffolding text -- must name the new slug where it is a live
# reference. GitHub redirects the old slug, so a stray hit is not an outage,
# but it silently re-teaches every consumer the stale owner (#14221).
#
# What is scanned: SHIPPED_PATHS below (tracked files only, via `git grep`).
# What is never scanned (history that legitimately names the old owner):
#   CHANGELOG.md, docs/adr/, docs/migration/, WORK_LOG.md
# Fixtures that deliberately use the old slug live outside the scanned set and
# are named here so a future widening of the set does not trip over them:
#   tests/install/test-forge-detect.sh, tests/hooks/test-guard-loom-workflow.sh
# Allowlisted lines inside the scanned set (matched by regex, not by line
# number, so they survive edits):
#   scaffolding.rs "**Loom Repository**"  # the LEGACY_LOOM_SIGNATURES entry
#                         # must keep matching what pre-transfer installers
#                         # wrote; it detects history, it is not a reference.
#
# The scan is a function over a repo root so the suite can also prove, against
# a throwaway repo, that it flags a regression, honours the allowlist, and
# surfaces fatal scan errors instead of swallowing them.
#
# Exit code 0 = all tests pass, 1 = failures detected.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

source "$SCRIPT_DIR/lib/assert.sh"

TMP_WORK="$(mktemp -d)"
trap 'rm -rf "$TMP_WORK"' EXIT

OLD_SLUG='rjwalters/loom'

# Paths an install/resync ships to consumers, plus the tracked in-repo mirrors
# of them (.loom/, .claude/, .github/CONFIGURATION.md) that CI requires to stay
# byte-equal to defaults/.
SHIPPED_PATHS=(
  defaults
  scripts/install
  scripts/uninstall-loom.sh
  scripts/test-installer.sh
  CLAUDE.md
  README.md
  .loom/README.md
  .loom/bin/loom
  .loom/config
  .loom/docs
  .loom/install-metadata.json
  .loom/runtimes
  .claude/README.md
  .claude/settings.json
  .github/CONFIGURATION.md
  loom-daemon/src/init/scaffolding.rs
)

# Lines matching this ERE are allowed to carry the old slug (see header).
# Permits only the exact LEGACY_LOOM_SIGNATURES entry in scaffolding.rs: that
# detector must keep matching the `**Loom Repository**:` line pre-transfer
# installers actually wrote, so it stays a live detector of history rather than
# a renamed reference (#14221 review).
ALLOWLIST_RE='^loom-daemon/src/init/scaffolding\.rs:[0-9]+:[[:space:]]*"\*\*Loom Repository\*\*: https://github\.com/rjwalters/loom",?[[:space:]]*$'

# scan_old_slug <repo-root> [allowlist-re]: print "path:line:text" for each
# non-allowlisted hit. Prints "MISSING: <path>" for a SHIPPED_PATHS entry that
# does not exist and "UNTRACKED: <path>" for one that exists but matches zero
# tracked paths -- git grep reads tracked files only, so such an entry would
# otherwise read as covered while scanning nothing (#15080). In a non-git tree
# the ls-files query fails; that case is left to the fatal git grep path below
# so stdout stays quiet for it. Treats grep exit 1 as "no matches" and
# propagates fatal errors (> 1) with a diagnostic on stderr.
scan_old_slug() {
  local root="$1" allowlist_re="${2:-$ALLOWLIST_RE}"
  local p tracked hits err_file err grep_rc filtered filter_rc
  for p in "${SHIPPED_PATHS[@]}"; do
    [[ -e "$root/$p" ]] || { echo "MISSING: $p"; continue; }
    if tracked="$(git -C "$root" ls-files -- "$p" 2>/dev/null)"; then
      [[ -n "$tracked" ]] || echo "UNTRACKED: $p"
    fi
  done

  err_file="$(mktemp "$TMP_WORK/scan-err.XXXXXX")"
  grep_rc=0
  hits="$(git -C "$root" grep -n -I -e "$OLD_SLUG" -- "${SHIPPED_PATHS[@]}" 2>"$err_file")" || grep_rc=$?
  err="$(<"$err_file")"
  rm -f "$err_file"

  if [[ $grep_rc -gt 1 ]]; then
    echo "ERROR: git grep failed (exit $grep_rc) in $root${err:+: $err}" >&2
    return "$grep_rc"
  fi
  [[ $grep_rc -eq 0 && -n "$hits" ]] || return 0

  err_file="$(mktemp "$TMP_WORK/filter-err.XXXXXX")"
  filter_rc=0
  filtered="$(grep -Ev -e "$allowlist_re" <<<"$hits" 2>"$err_file")" || filter_rc=$?
  err="$(<"$err_file")"
  rm -f "$err_file"

  if [[ $filter_rc -gt 1 ]]; then
    echo "ERROR: allowlist filter failed (exit $filter_rc)${err:+: $err}" >&2
    return "$filter_rc"
  fi
  [[ $filter_rc -eq 0 && -n "$filtered" ]] && printf '%s\n' "$filtered"
  return 0
}

# make_fixture_tree <root>: materialise every SHIPPED_PATHS entry under <root>
# in the shape it has in the real tree — a directory becomes a directory (with
# a placeholder file so git tracks it), a file becomes a file. Shape is read
# from $REPO_ROOT rather than guessed from the path: `.loom/config` is a
# directory while the extensionless `.loom/bin/loom` is a file, so neither a
# `*.*` split nor any basename heuristic can classify them (#14960).
make_fixture_tree() {
  local root="$1" p
  for p in "${SHIPPED_PATHS[@]}"; do
    if [[ -d "$REPO_ROOT/$p" ]]; then
      mkdir -p "$root/$p"
      echo "clean" >"$root/$p/placeholder.md"
    else
      mkdir -p "$root/$(dirname "$p")"
      echo "clean" >"$root/$p"
    fi
  done
}

# ============================================================================
# The real tree
# ============================================================================
echo ""
echo "=== Shipped files do not name $OLD_SLUG ==="

scan_rc=0
violations="$(scan_old_slug "$REPO_ROOT")" || scan_rc=$?
if [[ $scan_rc -ne 0 ]]; then
  assert_eq "real-tree scan succeeded without fatal error (see diagnostic above)" \
    "0" \
    "$scan_rc"
else
  assert_eq "no old-owner slug in shipped files (first hits: $(head -n 5 <<<"$violations" | tr '\n' '|'))" \
    "" \
    "$violations"
fi

# ============================================================================
# The scanner itself (throwaway repo)
# ============================================================================
echo ""
echo "=== Scanner flags regressions, honours allowlist, and reports fatal errors ==="

FIXTURE="$TMP_WORK/repo"
mkdir -p "$FIXTURE"
git -C "$FIXTURE" init -q
make_fixture_tree "$FIXTURE"
git -C "$FIXTURE" add -A

# Shape mirror: every shipped path must exist in the fixture in the shape the
# real tree gives it (directory vs file) — the property #14960 fixed; a
# regression here would otherwise pass the scans silently, as it did before.
shape_mismatches=""
for p in "${SHIPPED_PATHS[@]}"; do
  if [[ -d "$REPO_ROOT/$p" ]]; then
    [[ -d "$FIXTURE/$p" ]] || shape_mismatches+="dir:$p "
  else
    [[ -f "$FIXTURE/$p" ]] || shape_mismatches+="file:$p "
  fi
done
assert_eq "fixture mirrors each shipped path's real shape (dir/file)" \
  "" \
  "$shape_mismatches"

assert_eq "clean fixture: no hits" "" "$(scan_old_slug "$FIXTURE")"

# Untracked-but-present: git grep reads tracked files only, so a shipped path
# that exists on disk but matches zero tracked paths would read as covered
# while scanning nothing (#15080). Untrack scripts/install in the index only
# and expect exactly one UNTRACKED diagnostic -- not MISSING, not silence.
git -C "$FIXTURE" rm -qr --cached -- scripts/install
assert_eq "untracked shipped path keeps its placeholder on disk" \
  "true" \
  "$([[ -f "$FIXTURE/scripts/install/placeholder.md" ]] && echo true || echo false)"
assert_eq "an untracked-but-present shipped path is reported, not silently unscanned" \
  "UNTRACKED: scripts/install" \
  "$(scan_old_slug "$FIXTURE")"
git -C "$FIXTURE" add -- scripts/install

echo "see https://github.com/$OLD_SLUG/issues/1" >"$FIXTURE/defaults/regressed.md"
echo "image ghcr.io/$OLD_SLUG-worker:latest" >"$FIXTURE/defaults/ghcr-line.md"
echo "    \"**Loom Repository**: https://github.com/$OLD_SLUG\"," >"$FIXTURE/loom-daemon/src/init/scaffolding.rs"
git -C "$FIXTURE" add -A
hits="$(scan_old_slug "$FIXTURE")"
expected_hits="$(printf '%s\n%s' \
  "defaults/ghcr-line.md:1:image ghcr.io/$OLD_SLUG-worker:latest" \
  "defaults/regressed.md:1:see https://github.com/$OLD_SLUG/issues/1")"
assert_eq "regressed GitHub and GHCR lines are reported while historical scaffolding signature is allowlisted" \
  "$expected_hits" \
  "$hits"

echo "history: $OLD_SLUG" >"$FIXTURE/CHANGELOG.md"
mkdir -p "$FIXTURE/docs/adr"
echo "history: $OLD_SLUG" >"$FIXTURE/docs/adr/0001.md"
git -C "$FIXTURE" add -A
assert_eq "history files outside the shipped set are not scanned" \
  "$expected_hits" \
  "$(scan_old_slug "$FIXTURE")"

git -C "$FIXTURE" rm -q -rf scripts/install
assert_eq "a vanished shipped path is reported, not silently skipped" \
  "MISSING: scripts/install" \
  "$(scan_old_slug "$FIXTURE" | grep '^MISSING:')"

# Exported source tree (no .git): git grep exits 128; must fail loudly instead
# of returning an empty clean scan.
EXPORTED="$TMP_WORK/exported"
mkdir -p "$EXPORTED"
make_fixture_tree "$EXPORTED"
echo "see https://github.com/$OLD_SLUG/issues/1" >"$EXPORTED/README.md"

exported_rc=0
exported_err="$(GIT_CEILING_DIRECTORIES="$TMP_WORK" scan_old_slug "$EXPORTED" 2>&1 >/dev/null)" || exported_rc=$?
assert_eq "fatal git grep error in non-git tree propagates non-zero exit" \
  "true" \
  "$([[ $exported_rc -gt 1 ]] && echo true || echo false)"
assert_eq "fatal git grep error emits diagnostic" \
  "true" \
  "$(grep -q 'ERROR: git grep failed' <<<"$exported_err" && echo true || echo false)"

# Broken allowlist ERE: grep -Ev exits 2; must propagate instead of silently
# dropping all hits.
filter_rc=0
filter_err="$(scan_old_slug "$FIXTURE" '[invalid' 2>&1 >/dev/null)" || filter_rc=$?
assert_eq "fatal allowlist filter error propagates non-zero exit" \
  "true" \
  "$([[ $filter_rc -gt 1 ]] && echo true || echo false)"
assert_eq "fatal allowlist filter error emits diagnostic" \
  "true" \
  "$(grep -q 'ERROR: allowlist filter failed' <<<"$filter_err" && echo true || echo false)"

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
