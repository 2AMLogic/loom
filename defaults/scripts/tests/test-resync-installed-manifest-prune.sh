#!/usr/bin/env bash
# test-resync-installed-manifest-prune.sh - prune_stale_manifest_entries()
# drops stale install-metadata.json `installed_files` entries (#14012).
#
# When Loom retires a defaults/ file, remove_retired_files() deletes the
# installed copy but the installer-written manifest entry stayed behind, so
# the manifest over-claimed (16 stale entries observed in one consumer repo,
# stable across two commits). The prune reconciles `installed_files` against
# the live source tree at resync time: an entry is dropped only when it is a
# pure-copy-surface path, absent on disk, AND its defaults/ counterpart is
# gone upstream.
#
# This suite pins the four-way classification, because every wrong quadrant
# has a real cost:
#   - pruning a file that still exists on disk destroys the record of a
#     pinned fork (the consumer deliberately kept it);
#   - pruning an entry whose defaults/ counterpart still exists destroys the
#     installer's record of a file a later resync must be able to restore;
#   - leaving a genuinely retired entry in place is the original #14012 bug
#     (the manifest over-claims forever, and a one-off consumer prune only
#     helps until the next retirement lands);
#   - touching an install-once entry (.github/workflows/*) prunes a file
#     resync does not own and cannot re-verify.
#
# Groups 5-8 (#15478) pin the retirement the run itself performs. A retired
# file still on disk is removed before the prune classifies, so the apply
# prunes its entry; the preview, which removes nothing, has to name that same
# prune. A removal that fails leaves the file in place and keeps its entry,
# a listed file defaults/ still ships is a copy gap, not a retirement, and
# the parity holds in --output staging mode as well.
#
# Usage:
#   ./.loom/scripts/tests/test-resync-installed-manifest-prune.sh

set -uo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
HELPERS_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
SCRIPT="$HELPERS_DIR/resync-installed.sh"

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib/assert.sh"

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/test-resync-manifest-prune.XXXXXX")"
# shellcheck disable=SC2329  # invoked indirectly via the EXIT trap below
cleanup() { rm -rf "$WORKDIR" 2>/dev/null || true; }
trap cleanup EXIT

export GIT_AUTHOR_NAME="test" GIT_AUTHOR_EMAIL="test@example.com"
export GIT_COMMITTER_NAME="test" GIT_COMMITTER_EMAIL="test@example.com"

META_REL=".loom/install-metadata.json"

# has_entry <repo> <path>  -- is <path> listed in installed_files?
has_entry() {
    jq -e --arg p "$2" '.installed_files | index($p) != null' "$1/$META_REL" >/dev/null 2>&1
}

# entry_count <repo>
entry_count() {
    jq -r '.installed_files | length' "$1/$META_REL" 2>/dev/null
}

# --- consumer fixture --------------------------------------------------------
#
# make_fixture <name>
#   A consumer repo resolved from a real source checkout via
#   .loom/loom-source-path (the standard consumer shape), seeded with the full
#   four-way classification in its manifest:
#
#   .loom/hooks/retired.sh      absent on disk, absent upstream   -> PRUNE
#   .loom/hooks/keeper.sh       present on disk, shipped         -> keep
#   .loom/docs/pinned-doc.md    locally deleted, still shipped,
#                               pinned in .loom/resync-ignore    -> keep
#   .loom/docs/restored-doc.md  locally deleted, still shipped,
#                               NOT pinned                       -> keep
#                               (resync restores the file before
#                               the prune runs)
#   .github/workflows/x.yml     absent on disk, install-once,
#                               not a pure-copy surface          -> keep
#
#   The commit subject is a routine install subject, not "fixture":
#   resync-installed.sh's local-fix guard (#7864) refuses to run over a
#   non-routine subject.
make_fixture() {
    local name="$1"
    local consumer="$WORKDIR/$name" src="$WORKDIR/$name-src"
    rm -rf "$consumer" "$src"

    mkdir -p "$src/defaults/hooks" "$src/defaults/docs"
    printf 'A\n' > "$src/defaults/hooks/keeper.sh"
    chmod +x "$src/defaults/hooks/keeper.sh"
    printf 'PINNED\n' > "$src/defaults/docs/pinned-doc.md"
    printf 'RESTORED\n' > "$src/defaults/docs/restored-doc.md"
    printf '{\n  "version": "1.2.3"\n}\n' > "$src/package.json"
    git -C "$src" init -q
    git -C "$src" add -A >/dev/null 2>&1
    git -C "$src" commit -qm "source" >/dev/null 2>&1

    mkdir -p "$consumer/.loom/hooks" "$consumer/.loom/scripts" "$consumer/.loom/docs"
    git -C "$consumer" init -q
    printf 'A\n' > "$consumer/.loom/hooks/keeper.sh"
    printf 'PINNED\n' > "$consumer/.loom/docs/pinned-doc.md"
    printf 'RESTORED\n' > "$consumer/.loom/docs/restored-doc.md"
    rm "$consumer/.loom/docs/pinned-doc.md" "$consumer/.loom/docs/restored-doc.md"
    printf 'docs/pinned-doc.md  # local fork: keep the deletion\n' > "$consumer/.loom/resync-ignore"
    printf '%s\n' "$src" > "$consumer/.loom/loom-source-path"
    printf '{\n  "loom_version": "0.19.100",\n  "loom_commit": "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee",\n  "install_date": "2020-01-01",\n  "loom_source": "%s",\n  "installed_files": [\n    ".loom/hooks/retired.sh",\n    ".loom/hooks/keeper.sh",\n    ".loom/docs/pinned-doc.md",\n    ".loom/docs/restored-doc.md",\n    ".github/workflows/x.yml"\n  ]\n}\n' \
        "$src" > "$consumer/$META_REL"
    git -C "$consumer" add -A >/dev/null 2>&1
    git -C "$consumer" commit -qm "chore: install Loom v0.19.100" >/dev/null 2>&1
    echo "$consumer"
}

# --- (1) the prune itself -----------------------------------------------------
echo "Test group 1: a retired+absent manifest entry is pruned; every other quadrant is kept (#14012)"
REPO1="$(make_fixture prune-apply)"
OUT="$(cd "$REPO1" && bash "$SCRIPT" 2>&1)"
RC=$?
if [[ $RC -eq 0 ]]; then
    pass "(#14012) the resync run exits 0"
else
    fail "(#14012) the resync run exited $RC (out=$OUT)"
fi
if grep -q "manifest entry \.loom/hooks/retired\.sh" <<<"$OUT" && grep -q "retired upstream, #14012" <<<"$OUT"; then
    pass "(#14012) the prune is logged, naming the path and the reason"
else
    fail "(#14012) the prune was silent (out=$OUT)"
fi
if ! grep -q "retired\.sh" < <(jq -r '.installed_files[]' "$REPO1/$META_REL" 2>/dev/null); then
    pass "(#14012) the retired+absent entry is gone from installed_files"
else
    fail "(#14012) the retired+absent entry survived the prune"
fi
if has_entry "$REPO1" ".loom/hooks/keeper.sh"; then
    pass "(#14012) a present-on-disk entry is never pruned"
else
    fail "(#14012) a present-on-disk entry was pruned"
fi
if has_entry "$REPO1" ".loom/docs/pinned-doc.md"; then
    pass "(#14012) a shipped-but-locally-deleted (pinned) entry is never pruned"
else
    fail "(#14012) a shipped-but-locally-deleted entry was pruned"
fi
if has_entry "$REPO1" ".loom/docs/restored-doc.md"; then
    pass "(#14012) a shipped-but-locally-deleted (restored this run) entry is never pruned"
else
    fail "(#14012) an entry whose file the run restored was pruned"
fi
if [[ -f "$REPO1/.loom/docs/restored-doc.md" ]]; then
    pass "(#14012) the still-shipped deleted file was restored by the sync, not pruned away"
else
    fail "(#14012) the still-shipped deleted file was not restored"
fi
if has_entry "$REPO1" ".github/workflows/x.yml"; then
    pass "(#14012) an install-once entry outside the pure-copy surfaces is left untouched"
else
    fail "(#14012) an install-once entry was pruned"
fi
if [[ "$(entry_count "$REPO1")" == "4" ]]; then
    pass "(#14012) exactly the one stale entry was dropped (5 -> 4)"
else
    fail "(#14012) installed_files length is '$(entry_count "$REPO1")', expected 4"
fi

# --- (2) a run with nothing to prune is a byte-level no-op --------------------
#
# The #11680 contract: a resync that changed nothing must not touch the
# tracked manifest, or every host re-commits it forever. After the prune the
# manifest is consistent, so the next run must leave it byte-identical (the
# #11680 no-op keeps the volatile stamp fields; there is nothing left to
# prune).
echo ""
echo "Test group 2: a follow-up run with nothing to prune leaves the manifest byte-identical (#14012, #11680)"
BEFORE_CKSUM="$(cksum < "$REPO1/$META_REL")"
OUT2="$(cd "$REPO1" && bash "$SCRIPT" 2>&1)"
if [[ "$(cksum < "$REPO1/$META_REL")" == "$BEFORE_CKSUM" ]]; then
    pass "(#14012) the manifest is byte-identical after a run with nothing to prune"
else
    fail "(#14012) a no-prune run rewrote the manifest (#11680 churn)"
fi
if ! grep -q "pruned manifest entry" <<<"$OUT2"; then
    pass "(#14012) the follow-up run reports no prune"
else
    fail "(#14012) the follow-up run pruned something that should already be gone"
fi

# --- (3) --dry-run previews without writing ----------------------------------
echo ""
echo "Test group 3: --dry-run prints 'would prune' and does not modify the manifest (#14012)"
REPO3="$(make_fixture prune-dry-run)"
BEFORE="$(cat "$REPO3/$META_REL")"
OUT3="$(cd "$REPO3" && bash "$SCRIPT" --dry-run 2>&1)"
RC3=$?
if grep -q "would prune manifest entry .loom/hooks/retired\.sh" <<<"$OUT3"; then
    pass "(#14012) --dry-run previews the prune with the 'would prune' verb"
else
    fail "(#14012) --dry-run did not preview the prune (out=$OUT3)"
fi
if [[ "$(cat "$REPO3/$META_REL")" == "$BEFORE" ]]; then
    pass "(#14012) --dry-run left the manifest byte-identical"
else
    fail "(#14012) --dry-run MODIFIED the manifest"
fi
if [[ $RC3 -eq 2 ]]; then
    pass "(#14012) a pending prune is reported as drift (exit 2), not 'already in sync'"
else
    fail "(#14012) --dry-run with a pending prune exited $RC3, expected 2 (out=$OUT3)"
fi

# --- (4) the apply does exactly what the preview named -----------------------
#
# The plan is only useful to a consumer if it MATCHES: run the same fixture
# --dry-run, then for real, and assert both name the same single path.
echo ""
echo "Test group 4: the apply prunes exactly what the preview named (#14012)"
REPO4="$(make_fixture prune-match)"
PREVIEW="$(cd "$REPO4" && bash "$SCRIPT" --dry-run 2>&1 | sed -n 's/^  .*would prune[^ ]* manifest entry \([^ ]*\).*/\1/p')"
APPLY="$(cd "$REPO4" && bash "$SCRIPT" 2>&1 | sed -n 's/^  .*pruned[^ ]* *manifest entry \([^ ]*\).*/\1/p')"
if [[ -n "$PREVIEW" && "$PREVIEW" == "$APPLY" ]]; then
    pass "(#14012) preview and apply name the same path ('$PREVIEW')"
else
    fail "(#14012) preview named '$PREVIEW' but apply pruned '$APPLY'"
fi
if ! has_entry "$REPO4" ".loom/hooks/retired.sh" && [[ "$(entry_count "$REPO4")" == "4" ]]; then
    pass "(#14012) the apply dropped exactly the previewed entry"
else
    fail "(#14012) the apply did not drop exactly the previewed entry"
fi

# --- (5) a retirement the same run performs: preview == apply ----------------
#
# remove_retired_files() only REPORTS under --dry-run and leaves the file in
# place, so when prune_stale_manifest_entries() classifies a moment later the
# file is still on disk and its entry looked present: the preview named the
# `would remove` but not the `would prune` the apply does for the same
# retirement, and counted one removal fewer (#15478).
#
# make_retired_fixture <name>
#   The consumer shape of make_fixture(), with exactly ONE pending change:
#   the retirement of an ordinary hook -- its source is gone from defaults/,
#   it is listed in defaults/.loom-retired.list, and its tracked installed
#   copy is still on disk and still listed in installed_files. Nothing else
#   drifts, so the summary counts are exact.
#
#   Two more retired-listed hooks ride along as controls. The retirement pass
#   skips both, so the prune must leave their entries alone while their files
#   survive: `pinned.sh` is pinned in .loom/resync-ignore, and `linked.sh` is
#   a dangling destination symlink.
make_retired_fixture() {
    local name="$1"
    local consumer="$WORKDIR/$name" src="$WORKDIR/$name-src"
    rm -rf "$consumer" "$src"

    mkdir -p "$src/defaults/hooks"
    printf 'A\n' > "$src/defaults/hooks/keeper.sh"
    chmod +x "$src/defaults/hooks/keeper.sh"
    printf '%s\n' hooks/old.sh hooks/pinned.sh hooks/linked.sh > "$src/defaults/.loom-retired.list"
    printf '{\n  "version": "1.2.3"\n}\n' > "$src/package.json"
    git -C "$src" init -q
    git -C "$src" add -A >/dev/null 2>&1
    git -C "$src" commit -qm "source" >/dev/null 2>&1

    mkdir -p "$consumer/.loom/hooks"
    git -C "$consumer" init -q
    printf 'A\n' > "$consumer/.loom/hooks/keeper.sh"
    printf 'OLD\n' > "$consumer/.loom/hooks/old.sh"
    printf 'PINNED\n' > "$consumer/.loom/hooks/pinned.sh"
    ln -s missing-target.sh "$consumer/.loom/hooks/linked.sh"
    printf '%s\n' hooks/pinned.sh > "$consumer/.loom/resync-ignore"
    printf '%s\n' "$src" > "$consumer/.loom/loom-source-path"
    printf '{\n  "loom_version": "0.19.100",\n  "loom_commit": "aaaaaaaabbbbbbbbccccccccddddddddeeeeeeee",\n  "install_date": "2020-01-01",\n  "loom_source": "%s",\n  "installed_files": [\n    ".loom/hooks/keeper.sh",\n    ".loom/hooks/old.sh",\n    ".loom/hooks/pinned.sh",\n    ".loom/hooks/linked.sh"\n  ]\n}\n' \
        "$src" > "$consumer/$META_REL"
    git -C "$consumer" add -A >/dev/null 2>&1
    git -C "$consumer" commit -qm "chore: install Loom v0.19.100" >/dev/null 2>&1
    echo "$consumer"
}

# summary_removed <resync output>  -- the removed count of the summary line,
#   read the same way for a preview ("2 would be removed") and an apply
#   ("2 removed")
summary_removed() {
    sed -n 's/^\[resync\] .*[0-9]* file(s) \(would be \)\{0,1\}updated, \([0-9]*\) \(would be \)\{0,1\}removed,.*/\2/p' <<<"$1"
}

# pruned_entries <resync output>  -- every manifest entry the run pruned or
#   would prune, sorted
pruned_entries() {
    sed -n 's/^  *\(would \)\{0,1\}prune[d]* *manifest entry \([^ ]*\).*/\2/p' <<<"$1" | sort
}

echo ""
echo "Test group 5: --dry-run previews and counts the prune of a retired file the same run would remove (#15478)"
REPO5="$(make_retired_fixture retire-preview)"
BEFORE5="$(cat "$REPO5/$META_REL")"
OUT5="$(cd "$REPO5" && bash "$SCRIPT" --dry-run 2>&1)"
RC5=$?
if [[ $RC5 -eq 2 ]]; then
    pass "(#15478) --dry-run exits 2: the retirement is drift"
else
    fail "(#15478) --dry-run exited $RC5, expected 2 (out=$OUT5)"
fi
if grep -q "would remove hooks/old\.sh" <<<"$OUT5"; then
    pass "(#15478) --dry-run previews the removal of the retired hook"
else
    fail "(#15478) --dry-run did not preview the removal (out=$OUT5)"
fi
if grep -q "would prune manifest entry \.loom/hooks/old\.sh" <<<"$OUT5"; then
    pass "(#15478) --dry-run previews the prune of that hook's manifest entry"
else
    fail "(#15478) --dry-run did not preview the prune (out=$OUT5)"
fi
if [[ "$(cat "$REPO5/$META_REL")" == "$BEFORE5" && -z "$(git -C "$REPO5" status --porcelain)" ]]; then
    pass "(#15478) --dry-run leaves the manifest and the whole tree byte-identical"
else
    fail "(#15478) --dry-run modified the tree: $(git -C "$REPO5" status --porcelain | tr '\n' ' ')"
fi
# The apply runs on the SAME fixture, straight after the preview that
# changed nothing: the two reports describe one retirement.
OUT5B="$(cd "$REPO5" && bash "$SCRIPT" 2>&1)"
RC5B=$?
if [[ $RC5B -eq 0 ]]; then
    pass "(#15478) the apply on the same fixture exits 0"
else
    fail "(#15478) the apply exited $RC5B (out=$OUT5B)"
fi
if grep -q "removed.*hooks/old\.sh.*retired from defaults/, #5981" <<<"$OUT5B" \
   && grep -q "pruned.*manifest entry \.loom/hooks/old\.sh" <<<"$OUT5B"; then
    pass "(#15478) the apply removes the hook and prunes its entry"
else
    fail "(#15478) the apply did not remove the hook and prune its entry (out=$OUT5B)"
fi
PREVIEW5_REMOVED="$(summary_removed "$OUT5")"
APPLY5_REMOVED="$(summary_removed "$OUT5B")"
if [[ -n "$PREVIEW5_REMOVED" && "$PREVIEW5_REMOVED" == "$APPLY5_REMOVED" ]]; then
    pass "(#15478) preview and apply report the same removed count ('$PREVIEW5_REMOVED')"
else
    fail "(#15478) removed count differs: preview '$PREVIEW5_REMOVED', apply '$APPLY5_REMOVED'"
fi
if [[ "$PREVIEW5_REMOVED" == "2" && "$APPLY5_REMOVED" == "2" ]]; then
    pass "(#15478) both count two removals for the one retirement: the file and its entry"
else
    fail "(#15478) removed count: preview '$PREVIEW5_REMOVED', apply '$APPLY5_REMOVED', expected 2 and 2"
fi
if [[ "$(pruned_entries "$OUT5")" == ".loom/hooks/old.sh" && "$(pruned_entries "$OUT5B")" == ".loom/hooks/old.sh" ]]; then
    pass "(#15478) preview and apply each prune exactly the retired hook's entry"
else
    fail "(#15478) pruned entries: preview '$(pruned_entries "$OUT5" | tr '\n' ' ')' apply '$(pruned_entries "$OUT5B" | tr '\n' ' ')'"
fi
if [[ ! -e "$REPO5/.loom/hooks/old.sh" ]] && ! has_entry "$REPO5" ".loom/hooks/old.sh"; then
    pass "(#15478) the hook is gone from disk and from installed_files"
else
    fail "(#15478) the hook or its entry survived the apply"
fi
if [[ "$(cat "$REPO5/.loom/hooks/pinned.sh" 2>/dev/null)" == "PINNED" && -L "$REPO5/.loom/hooks/linked.sh" ]] \
   && has_entry "$REPO5" ".loom/hooks/pinned.sh" && has_entry "$REPO5" ".loom/hooks/linked.sh"; then
    pass "(#15478) the pinned and the symlinked retired hook survive with their entries"
else
    fail "(#15478) a pinned or symlinked retired hook, or its entry, did not survive (out=$OUT5B)"
fi
if [[ "$(jq -r '.installed_files[]' "$REPO5/$META_REL" 2>/dev/null | sort | tr '\n' ' ')" \
      == ".loom/hooks/keeper.sh .loom/hooks/linked.sh .loom/hooks/pinned.sh " ]]; then
    pass "(#15478) installed_files is exactly the three survivors"
else
    fail "(#15478) installed_files is: $(jq -r '.installed_files[]' "$REPO5/$META_REL" 2>/dev/null | tr '\n' ' ')"
fi
# install-metadata.json is tracked, so a prune is commit-visible dirt: every
# tracked change the apply makes has to be one the preview announced.
if [[ "$(git -C "$REPO5" status --porcelain --untracked-files=no)" == $' D .loom/hooks/old.sh\n M .loom/install-metadata.json' ]]; then
    pass "(#15478) the apply's tracked changes are exactly the announced hook deletion and the manifest"
else
    fail "(#15478) tracked changes after the apply: $(git -C "$REPO5" status --porcelain --untracked-files=no | tr '\n' ' ')"
fi

# --- (6) a removal that fails keeps its entry --------------------------------
#
# The preview cannot foresee a failed removal, and the apply must not assume
# one: the retirement pass approved this hook, but `rm -f` cannot remove a
# directory, so the path is still on disk when the prune classifies and the
# entry has to stay. An entry is dropped on what the apply DID, never on what
# the retirement pass set out to do.
echo ""
echo "Test group 6: a retired file the apply fails to remove keeps its manifest entry (#15478)"
REPO6="$(make_retired_fixture retire-failure)"
rm "$REPO6/.loom/hooks/old.sh"
mkdir "$REPO6/.loom/hooks/old.sh"
printf 'INNER\n' > "$REPO6/.loom/hooks/old.sh/inner.txt"
git -C "$REPO6" add -A >/dev/null 2>&1
git -C "$REPO6" commit -q --amend -m "chore: install Loom v0.19.100" >/dev/null 2>&1
OUT6="$(cd "$REPO6" && bash "$SCRIPT" 2>&1)"
RC6=$?
if [[ $RC6 -eq 1 ]]; then
    pass "(#15478) the apply exits 1: the removal failed"
else
    fail "(#15478) the apply exited $RC6, expected 1 (out=$OUT6)"
fi
if grep -q "failed to remove retired file hooks/old\.sh" <<<"$OUT6"; then
    pass "(#15478) the failure is reported, naming the hook"
else
    fail "(#15478) the failed removal was not reported (out=$OUT6)"
fi
if [[ -z "$(pruned_entries "$OUT6")" ]]; then
    pass "(#15478) no manifest entry is pruned"
else
    fail "(#15478) the apply pruned: $(pruned_entries "$OUT6" | tr '\n' ' ')"
fi
if [[ -d "$REPO6/.loom/hooks/old.sh" ]] && has_entry "$REPO6" ".loom/hooks/old.sh"; then
    pass "(#15478) the path and its manifest entry are both left in place"
else
    fail "(#15478) the failed removal lost its path or its entry"
fi

# --- (7) a planned removal is still held to the upstream retirement -----------
#
# The --dry-run relaxation of the on-disk check must not loosen the other
# criterion: a listed hook whose source defaults/ still ships is removed by
# the retirement pass, but to the prune it is a copy gap, not a retirement,
# so neither run drops its entry.
echo ""
echo "Test group 7: a retired-listed hook that is still shipped upstream is removed, and its entry is kept (#15478)"
REPO7="$(make_retired_fixture retire-shipped)"
SRC7="$(cat "$REPO7/.loom/loom-source-path")"
printf 'SHIPPED\n' > "$SRC7/defaults/hooks/shipped.sh"
printf '%s\n' hooks/shipped.sh >> "$SRC7/defaults/.loom-retired.list"
git -C "$SRC7" add -A >/dev/null 2>&1
git -C "$SRC7" commit -qm "still-shipped listed hook" >/dev/null 2>&1
printf 'SHIPPED\n' > "$REPO7/.loom/hooks/shipped.sh"
jq '.installed_files += [".loom/hooks/shipped.sh"]' "$REPO7/$META_REL" > "$REPO7/$META_REL.extra"
mv "$REPO7/$META_REL.extra" "$REPO7/$META_REL"
git -C "$REPO7" add -A >/dev/null 2>&1
git -C "$REPO7" commit -q --amend -m "chore: install Loom v0.19.100" >/dev/null 2>&1
OUT7="$(cd "$REPO7" && bash "$SCRIPT" --dry-run 2>&1)"
OUT7B="$(cd "$REPO7" && bash "$SCRIPT" 2>&1)"
RC7B=$?
if grep -q "would remove hooks/shipped\.sh" <<<"$OUT7" \
   && ! grep -q "would prune manifest entry \.loom/hooks/shipped\.sh" <<<"$OUT7"; then
    pass "(#15478) --dry-run previews the removal but no prune for the still-shipped hook"
else
    fail "(#15478) --dry-run mishandled the still-shipped hook (out=$OUT7)"
fi
if [[ $RC7B -eq 0 ]] && grep -q "removed.*hooks/shipped\.sh.*retired from defaults/, #5981" <<<"$OUT7B" \
   && has_entry "$REPO7" ".loom/hooks/shipped.sh"; then
    pass "(#15478) the apply removes the still-shipped hook and keeps its entry"
else
    fail "(#15478) the apply mishandled the still-shipped hook (out=$OUT7B)"
fi
if [[ "$(pruned_entries "$OUT7")" == ".loom/hooks/old.sh" && "$(pruned_entries "$OUT7B")" == ".loom/hooks/old.sh" ]]; then
    pass "(#15478) preview and apply still prune exactly the retired hook's entry"
else
    fail "(#15478) pruned entries: preview '$(pruned_entries "$OUT7" | tr '\n' ' ')' apply '$(pruned_entries "$OUT7B" | tr '\n' ' ')'"
fi

# --- (8) the same parity in --output staging mode -----------------------------
#
# There WRITE_ROOT is the disposable staging worktree, not the consumer, so
# the recorded destination has to be relative to THAT root for the prune to
# match it against the manifest entry.
echo ""
echo "Test group 8: --output staging mode previews and applies the same prune (#15478)"
REPO8="$(make_retired_fixture retire-staging)"
STAGE8="$WORKDIR/retire-staging-output"
OUT8="$(cd "$REPO8" && bash "$SCRIPT" --dry-run --output "$STAGE8-dry" 2>&1)"
RC8=$?
OUT8B="$(cd "$REPO8" && bash "$SCRIPT" --output "$STAGE8" 2>&1)"
RC8B=$?
if [[ $RC8 -eq 2 ]] && grep -q "would prune manifest entry \.loom/hooks/old\.sh" <<<"$OUT8" \
   && [[ ! -e "$STAGE8-dry" && -z "$(git -C "$REPO8" status --porcelain)" ]]; then
    pass "(#15478) a staged --dry-run previews the prune and leaves no residue"
else
    fail "(#15478) the staged --dry-run exited $RC8 (out=$OUT8)"
fi
if [[ $RC8B -eq 0 && "$(pruned_entries "$OUT8")" == "$(pruned_entries "$OUT8B")" \
      && "$(summary_removed "$OUT8")" == "$(summary_removed "$OUT8B")" ]]; then
    pass "(#15478) the staged preview and apply name the same prune and removed count"
else
    fail "(#15478) staged preview '$(summary_removed "$OUT8")'/'$(pruned_entries "$OUT8" | tr '\n' ' ')' vs apply '$(summary_removed "$OUT8B")'/'$(pruned_entries "$OUT8B" | tr '\n' ' ')'"
fi
if [[ ! -e "$STAGE8/.loom/hooks/old.sh" ]] && ! has_entry "$STAGE8" ".loom/hooks/old.sh" \
   && [[ -f "$REPO8/.loom/hooks/old.sh" ]] && has_entry "$REPO8" ".loom/hooks/old.sh"; then
    pass "(#15478) the staged apply drops the hook and its entry in the staging worktree only"
else
    fail "(#15478) the staged apply landed in the wrong tree (out=$OUT8B)"
fi

# --- summary -----------------------------------------------------------------
echo ""
echo "========================================"
echo "Results: $TESTS_PASSED/$TESTS_RUN passed"
echo "========================================"
if [[ $TESTS_FAILED -gt 0 ]]; then
    echo -e "${RED}$TESTS_FAILED test(s) failed${NC}"
    exit 1
fi
echo -e "${GREEN}All tests passed${NC}"
exit 0
