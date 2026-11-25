---
disable-model-invocation: true
---
# Champion: Docs-Only Fast Path (merge-risk criterion, #6134)

**Load when**: the `merge-risk` criterion's `FAST_PATH_FILES` (fetched fresh in
this pass by `champion-pr-merge.md`) is confined to the root-level
`WORK_LOG.md` / `WORK_PLAN.md` / `README.md` plus the month-rotated archives
(`work_log/WORK_LOG_YYYY-MM.md`, `work_plan/WORK_PLAN_YYYY-MM.md`). Moved out
of `champion-pr-merge.md` (#15236) so every other merge pass stops paying for
it. Not loaded means `FAST_PATH_RESULT` stays at its non-eligible default and
the PR falls through to the standing-authorization check and the four axes.

Run this block in the same shell as the `FAST_PATH_FILES` read. It defines
three checks (allowlist #6134, closing reference #14182, WORK_PLAN plan-body
receipt #15220; the WORK_LOG record check is Judge-side only) and leaves the
verdict in `$FAST_PATH_RESULT`. Mirrored, not shared, with `judge-docs-only-fast-path.md`:
`test-docs-only-fast-path.sh` pins both copies.


```bash
# Mirrors judge-docs-only-fast-path.md's allowlist exactly (mirrored, not
# shared — a divergence between the two copies is a visible drift, not a
# silent gap; see test-docs-only-fast-path.sh which pins both).
DOCS_FAST_PATH_ALLOWLIST=("WORK_LOG.md" "WORK_PLAN.md" "README.md")
# Archive files (#12636): ONLY the month-rotated archives, month 01-12, one
# directory deep. Never a directory or *.md allowance.
DOCS_FAST_PATH_ARCHIVE_RE='^(work_log/WORK_LOG|work_plan/WORK_PLAN)_[0-9]{4}-(0[1-9]|1[0-2])\.md$'

docs_only_fast_path_check() {
  local file matched
  local saw_any=0
  while IFS= read -r file; do
    [ -z "$file" ] && continue
    saw_any=1
    matched=0
    for allowed in "${DOCS_FAST_PATH_ALLOWLIST[@]}"; do
      if [ "$file" = "$allowed" ]; then
        matched=1
        break
      fi
    done
    if [ "$matched" -ne 1 ] && [[ "$file" =~ $DOCS_FAST_PATH_ARCHIVE_RE ]]; then
      matched=1
    fi
    if [ "$matched" -ne 1 ]; then
      echo "NOT ELIGIBLE: $file"
      return 0
    fi
  done
  if [ "$saw_any" -eq 0 ]; then
    echo "NOT ELIGIBLE: empty file list"
    return 0
  fi
  echo "ELIGIBLE"
}

# FAST_PATH_FILES was fetched fresh, in THIS pass, by champion-pr-merge.md (the
# paginated files API, #4613; the standing-authorization check reads the same
# variable). Do not re-fetch it and never substitute a marker or the PR body.
FAST_PATH_RESULT=$(printf '%s\n' "$FAST_PATH_FILES" | docs_only_fast_path_check)

# Closing-link condition (#14182), mirrored from judge.md Step 3 (mirrored, not
# shared; test-docs-only-fast-path.sh pins both copies). A docs-only diff that
# auto-closes an issue on merge is not a docs-only change. Fails closed: a read
# that errored or returned no "closers=" proof is NOT ELIGIBLE, never "no closers".
docs_only_fast_path_closing_check() {
  # $1 = PR number. Echoes "ELIGIBLE" only when the closing-reference read
  # SUCCEEDED (positively proven by the "closers=" prefix) and returned an
  # empty set. A failed or malformed read is NOT ELIGIBLE, never "no closers"
  # (fail closed, #14182). Deliberately not forge_pr_close_targets: that helper
  # swallows read errors into an empty list, which would read as "no closers".
  local out refs
  if ! out=$(gh pr view "$1" --json closingIssuesReferences \
      --jq '"closers=" + ([.closingIssuesReferences[].number | tostring] | join(","))' 2>/dev/null); then
    echo "NOT ELIGIBLE: closing-reference read failed"
    return 0
  fi
  case "$out" in
    closers=*) refs="${out#closers=}" ;;
    *) echo "NOT ELIGIBLE: closing-reference read returned no proof"; return 0 ;;
  esac
  if [ -n "$refs" ]; then
    echo "NOT ELIGIBLE: closes #${refs//,/, #}"
    return 0
  fi
  echo "ELIGIBLE"
}

if [ "$FAST_PATH_RESULT" = "ELIGIBLE" ]; then
  FAST_PATH_RESULT=$(docs_only_fast_path_closing_check "$PR_NUMBER")
fi

# WORK_PLAN plan-body region gate (#15220), mirrored from judge.md Step 5
# (mirrored, not shared; test-docs-only-fast-path.sh pins both copies). A
# Guide tick once hand-wrote the `guide:plan-body` region and put free prose
# where the `WORK_PLAN check:` receipt line belongs; this gate re-derives the
# receipt on the reviewing side. A diff that changes the region must carry
# exactly one `- **WORK_PLAN check**: written (render-hash <12hex>, <UTC>)`
# line whose hash matches the head region's stamp-stripped sha256; a
# stamp-only edit, free prose, a wrong/absent hash, a WORK_PLAN.md created or
# deleted by the PR, malformed markers, or a failed read is NOT ELIGIBLE.
# `ENVIRONMENT:`-prefixed refusals are tooling failures, never content
# verdicts.
docs_only_fast_path_plan_region() {
  awk '/^<!-- guide:plan-body:start -->$/ { if (s) exit 1; s = NR; next }
       /^<!-- guide:plan-body:end -->$/   { if (!s || e) exit 1; e = NR; next }
       s && !e { print }
       END { if (!s || !e) exit 1 }'
}

docs_only_fast_path_plan_check() {
  # $1 = PR number.
  local pr="$1" out repo raw l in_plan=0 touched=0 created=0 deleted=0
  local shas base_sha head_sha base_file head_file body
  local base_region head_region base_body head_body head_hash hash
  local -a receipt_lines=()
  local receipt_re='^- \*\*WORK_PLAN check\*\*: written \(render-hash ([0-9a-f]{12}), [0-9TZ:-]+\)$'
  if ! out=$(gh pr view "$pr" --json url --jq '.url | capture("^https://[^/]+/(?<r>[^/]+/[^/]+)/pull/[0-9]+/?$").r' 2>/dev/null) || [ -z "$out" ]; then
    echo "NOT ELIGIBLE: ENVIRONMENT: base repository read failed (gh error, not a PR-content verdict) — take the normal review flow; do NOT treat this as a passed or waived #15220 content check"
    return 0
  fi
  repo="$out"
  if ! raw=$(gh pr diff --repo "$repo" "$pr" 2>/dev/null); then
    echo "NOT ELIGIBLE: ENVIRONMENT: PR diff read failed (gh error, not a PR-content verdict) — take the normal review flow; do NOT treat this as a passed or waived #15220 content check"
    return 0
  fi
  # Exact root-file scoping, same header discipline as the worklog check: the
  # monthly work_plan/WORK_PLAN_YYYY-MM.md archives are not the live file, and
  # a non-WORK_PLAN hunk can neither trigger this gate nor produce a refusal.
  while IFS= read -r l; do
    case "$l" in
      'diff --git '*)
        case "$l" in
          *' b/WORK_PLAN.md') in_plan=1; touched=1 ;;
          *) in_plan=0 ;;
        esac
        continue ;;
    esac
    [ "$in_plan" = 1 ] || continue
    case "$l" in
      'new file mode'*) created=1 ;;
      'deleted file mode'*) deleted=1 ;;
    esac
  done < <(printf '%s\n' "$raw")
  if [ "$touched" -eq 0 ]; then
    echo "ELIGIBLE"
    return 0
  fi
  if [ "$created" -eq 1 ]; then
    echo "NOT ELIGIBLE: WORK_PLAN.md newly created by this PR — the plan-body region cannot be verified (#15220)"
    return 0
  fi
  if [ "$deleted" -eq 1 ]; then
    echo "NOT ELIGIBLE: WORK_PLAN.md deleted by this PR — the plan-body region cannot be verified (#15220)"
    return 0
  fi
  if ! shas=$(gh pr view "$pr" --json baseRefOid,headRefOid --jq '"\(.baseRefOid)\t\(.headRefOid)"' 2>/dev/null) || [ -z "$shas" ]; then
    echo "NOT ELIGIBLE: ENVIRONMENT: PR ref-SHA read failed (gh error, not a PR-content verdict) — take the normal review flow; do NOT treat this as a passed or waived #15220 content check"
    return 0
  fi
  base_sha="${shas%%$'\t'*}"
  head_sha="${shas##*$'\t'}"
  if [ -z "$base_sha" ] || [ -z "$head_sha" ]; then
    echo "NOT ELIGIBLE: ENVIRONMENT: PR ref-SHA read failed (gh error, not a PR-content verdict) — take the normal review flow; do NOT treat this as a passed or waived #15220 content check"
    return 0
  fi
  if ! body=$(gh pr view "$pr" --json body --jq '.body' 2>/dev/null); then
    echo "NOT ELIGIBLE: ENVIRONMENT: PR body read failed (gh error, not a PR-content verdict) — take the normal review flow; do NOT treat this as a passed or waived #15220 content check"
    return 0
  fi
  if ! base_file=$(gh api -H "Accept: application/vnd.github.raw" "repos/$repo/contents/WORK_PLAN.md?ref=$base_sha" 2>/dev/null); then
    echo "NOT ELIGIBLE: ENVIRONMENT: WORK_PLAN.md base contents read failed (gh error, not a PR-content verdict) — take the normal review flow; do NOT treat this as a passed or waived #15220 content check"
    return 0
  fi
  if ! head_file=$(gh api -H "Accept: application/vnd.github.raw" "repos/$repo/contents/WORK_PLAN.md?ref=$head_sha" 2>/dev/null); then
    echo "NOT ELIGIBLE: ENVIRONMENT: WORK_PLAN.md head contents read failed (gh error, not a PR-content verdict) — take the normal review flow; do NOT treat this as a passed or waived #15220 content check"
    return 0
  fi
  if ! base_region=$(printf '%s\n' "$base_file" | docs_only_fast_path_plan_region); then
    echo "NOT ELIGIBLE: WORK_PLAN.md base has missing, duplicated or unordered guide:plan-body markers (#15220)"
    return 0
  fi
  if ! head_region=$(printf '%s\n' "$head_file" | docs_only_fast_path_plan_region); then
    echo "NOT ELIGIBLE: WORK_PLAN.md head has missing, duplicated or unordered guide:plan-body markers (#15220)"
    return 0
  fi
  # Raw regions equal → the diff did not change the region (an edit outside
  # the markers); pass without touching the receipt.
  if [ "$base_region" = "$head_region" ]; then
    echo "ELIGIBLE"
    return 0
  fi
  # strip_plan_stamp's exact shape (guide.md #13909): drop line 1 when it is
  # the generated-at stamp. The $() captures above already normalized trailing
  # newlines on both sides, the same normalization WORK_PLAN_RENDER_HASH's
  # input gets.
  base_body="$(printf '%s\n' "$base_region" | sed -e '1{/^_Generated: .* (snapshot; compare against live labels)_$/d;}')"
  head_body="$(printf '%s\n' "$head_region" | sed -e '1{/^_Generated: .* (snapshot; compare against live labels)_$/d;}')"
  if [ "$base_body" = "$head_body" ]; then
    echo "NOT ELIGIBLE: stamp-only edit to the WORK_PLAN guide:plan-body region — update_work_plan() records 'current' and never writes one (#15220)"
    return 0
  fi
  # The hash the receipt must carry: sha256 of the head region's
  # stamp-stripped body plus one trailing newline, first 12 hex — the same
  # computation as guide.md's WORK_PLAN_RENDER_HASH (sha256sum on Linux,
  # shasum on macOS).
  head_hash="$(printf '%s\n' "$head_body" | { sha256sum 2>/dev/null || shasum -a 256; } 2>/dev/null | cut -c1-12)"
  while IFS= read -r l; do
    receipt_lines+=("$l")
  done < <(printf '%s\n' "$body" | grep -E '^- \*\*WORK_PLAN check\*\*: ' || true)
  if [ "${#receipt_lines[@]}" -ne 1 ]; then
    echo "NOT ELIGIBLE: PR body carries ${#receipt_lines[@]} 'WORK_PLAN check:' lines where exactly one is required (#15220)"
    return 0
  fi
  if ! [[ "${receipt_lines[0]}" =~ $receipt_re ]]; then
    echo "NOT ELIGIBLE: the PR body's 'WORK_PLAN check:' line is not a machine-rendered 'written' receipt (render-hash <12hex>, <UTC>) — free prose, 'NOT RUN' or 'render-hash none' cannot verify a changed region (#15220)"
    return 0
  fi
  hash="${BASH_REMATCH[1]}"
  if [ "$hash" != "$head_hash" ]; then
    echo "NOT ELIGIBLE: WORK_PLAN render-hash mismatch — PR body claims '$hash', the head region recomputes '$head_hash' (#15220)"
    return 0
  fi
  echo "ELIGIBLE"
}

if [ "$FAST_PATH_RESULT" = "ELIGIBLE" ]; then
  FAST_PATH_RESULT=$(docs_only_fast_path_plan_check "$PR_NUMBER")
fi

if [ "$FAST_PATH_RESULT" = "ELIGIBLE" ]; then
  echo "PASS (docs-only fast path, #6134): merge risk covered without axis judgment — changed files: $FAST_PATH_FILES"
  # Use this as the ONE_LINE_RATIONALE in Step 2's pre-merge comment, e.g.
  # "docs-only fast path (#6134): diff confined to WORK_LOG.md/WORK_PLAN.md/
  # README.md, verified via the paginated files API". Continue to criterion
  # critical-files — do NOT evaluate the four-axis table below for this PR.
fi
```
