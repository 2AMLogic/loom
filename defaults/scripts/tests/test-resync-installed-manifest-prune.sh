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
# Groups 5-7 (#15475) extend the same reconcile to the three copy surfaces
# OUTSIDE the pure-copy classifier -- .claude/commands/loom/, .claude/agents/
# and the marker-gated .agents/skills/ -- where a retirement used to remove
# the file and keep the entry. The eligibility stays local to the prune: the
# shared classifier also decides which tracked deletions the commit advice
# stages, and a retired command's deletion must still be staged.
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

# --- the copy surfaces outside .loom/ (#15475) --------------------------------
#
# resync also copies three surfaces the pure-copy classifier does not cover:
# .claude/commands/loom/ and .claude/agents/ (resync_tree) and the
# marker-gated .agents/skills/ (resync_agent_skills). A retirement there
# removed the installed file and kept its manifest entry -- the #14012
# over-claim, on the surfaces that classifier never reached. Each surface
# retires by its own rule, so the layered fixture carries one stale entry per
# rule next to every control that must NOT be pruned:
#
#   commands -- retired through defaults/.loom-retired.list
#     .claude/commands/loom/loom-iteration.md   on disk (tracked), retired  -> removed + PRUNE
#     .claude/commands/loom/loom-parent.md      already absent, retired     -> PRUNE
#     .claude/commands/loom/shipped.md          absent, still shipped       -> restored, keep
#     .claude/commands/loom/pinned.md           on disk, retired, pinned     -> keep
#     .claude/commands/loom/linked.md           a symlink                    -> keep
#   agents -- copied recursively, no retirement-list mapping
#     .claude/agents/retired-agent.md           absent, gone upstream        -> PRUNE
#     .claude/agents/shipped-missing.md         absent, still shipped        -> restored, keep
#   skills -- retire_agent_skills(), ownership proven by the marker
#     .agents/skills/loom-retired/SKILL.md      marked, no source            -> removed + PRUNE
#     .agents/skills/loom-gone/SKILL.md         absent before the run        -> keep (*)
#     .agents/skills/loom-detached/SKILL.md     no marker                    -> keep
#     .agents/skills/loom-pinned/SKILL.md       marked, pinned               -> keep
#     .agents/skills/loom-linkfile/SKILL.md     a symlink                    -> keep
#     .agents/skills/custom/SKILL.md             marked, but not a loom-* dir -> keep
#
#   (*) Nothing proves a pre-absent skill entry was Loom's: the marker lived
#       in the file, so only a removal this run is evidence.
SKILL_MARK="<!-- loom-managed-skill -->"
EXTRA_ENTRIES=(
    ".claude/commands/loom/loom-iteration.md"
    ".claude/commands/loom/loom-parent.md"
    ".claude/commands/loom/shipped.md"
    ".claude/commands/loom/pinned.md"
    ".claude/commands/loom/linked.md"
    ".claude/agents/retired-agent.md"
    ".claude/agents/shipped-agent.md"
    ".claude/agents/shipped-missing.md"
    ".agents/skills/loom-foo/SKILL.md"
    ".agents/skills/loom-retired/SKILL.md"
    ".agents/skills/loom-gone/SKILL.md"
    ".agents/skills/loom-detached/SKILL.md"
    ".agents/skills/loom-pinned/SKILL.md"
    ".agents/skills/loom-linkfile/SKILL.md"
    ".agents/skills/custom/SKILL.md"
)
# What one apply run must leave in installed_files: the four entries
# make_fixture() keeps, plus every control above.
SURVIVORS=(
    ".claude/commands/loom/shipped.md"
    ".claude/commands/loom/pinned.md"
    ".claude/commands/loom/linked.md"
    ".claude/agents/shipped-agent.md"
    ".claude/agents/shipped-missing.md"
    ".agents/skills/loom-foo/SKILL.md"
    ".agents/skills/loom-gone/SKILL.md"
    ".agents/skills/loom-detached/SKILL.md"
    ".agents/skills/loom-pinned/SKILL.md"
    ".agents/skills/loom-linkfile/SKILL.md"
    ".agents/skills/custom/SKILL.md"
    ".github/workflows/x.yml"
    ".loom/docs/pinned-doc.md"
    ".loom/docs/restored-doc.md"
    ".loom/hooks/keeper.sh"
)
# What it must drop: the two reported commands, the agent, the skill, and
# make_fixture()'s own stale hook.
STALE_ENTRIES=(
    ".claude/commands/loom/loom-iteration.md"
    ".claude/commands/loom/loom-parent.md"
    ".claude/agents/retired-agent.md"
    ".agents/skills/loom-retired/SKILL.md"
    ".loom/hooks/retired.sh"
)
# The stale entries already absent before the run: for these the prune is the
# only pending change, so a --dry-run has to preview each one.
PENDING_PRUNES=(
    ".claude/commands/loom/loom-parent.md"
    ".claude/agents/retired-agent.md"
    ".loom/hooks/retired.sh"
)
SURVIVORS_SORTED="$(printf '%s\n' "${SURVIVORS[@]}" | sort)"
STALE_SORTED="$(printf '%s\n' "${STALE_ENTRIES[@]}" | sort)"

# add_extra_surfaces <consumer>
#   Layers the three surfaces onto a make_fixture() consumer (its source
#   checkout is read back from the sidecar make_fixture() wrote) and amends
#   the install commit, so the tree starts clean and the retired command is a
#   TRACKED file -- the deletion the commit advice has to stage.
add_extra_surfaces() {
    local consumer="$1" src cmd ag sk
    src="$(cat "$consumer/.loom/loom-source-path")"
    cmd="$consumer/.claude/commands/loom"
    ag="$consumer/.claude/agents"
    sk="$consumer/.agents/skills"

    mkdir -p "$src/defaults/.claude/commands/loom" "$src/defaults/.claude/agents" \
             "$src/defaults/.agents/skills/loom-foo"
    printf 'SHIPPED\n' > "$src/defaults/.claude/commands/loom/shipped.md"
    printf 'SHIPPED\n' > "$src/defaults/.claude/agents/shipped-agent.md"
    printf 'SHIPPED\n' > "$src/defaults/.claude/agents/shipped-missing.md"
    printf -- '---\n%s\nSHIPPED\n' "$SKILL_MARK" > "$src/defaults/.agents/skills/loom-foo/SKILL.md"
    printf '%s\n' commands/loom/loom-iteration.md commands/loom/loom-parent.md \
        commands/loom/pinned.md commands/loom/linked.md > "$src/defaults/.loom-retired.list"
    git -C "$src" add -A >/dev/null 2>&1
    git -C "$src" commit -qm "extra surfaces" >/dev/null 2>&1

    mkdir -p "$cmd" "$ag" "$sk/loom-foo" "$sk/loom-retired" "$sk/loom-detached" \
             "$sk/loom-pinned" "$sk/loom-linkfile" "$sk/custom"
    printf 'ITERATION\n' > "$cmd/loom-iteration.md"
    printf 'PINNED\n' > "$cmd/pinned.md"
    ln -s ../../.claude/agents/shipped-agent.md "$cmd/linked.md"
    printf 'SHIPPED\n' > "$ag/shipped-agent.md"
    cp "$src/defaults/.agents/skills/loom-foo/SKILL.md" "$sk/loom-foo/SKILL.md"
    printf -- '---\n%s\n' "$SKILL_MARK" > "$sk/loom-retired/SKILL.md"
    printf 'mine\n' > "$sk/loom-detached/SKILL.md"
    printf -- '---\n%s\n' "$SKILL_MARK" > "$sk/loom-pinned/SKILL.md"
    ln -s "$sk/loom-retired/SKILL.md" "$sk/loom-linkfile/SKILL.md"
    printf -- '---\n%s\n' "$SKILL_MARK" > "$sk/custom/SKILL.md"
    printf '%s\n' commands/loom/pinned.md agents-skills/loom-pinned/SKILL.md >> "$consumer/.loom/resync-ignore"
    jq --argjson extra "$(printf '%s\n' "${EXTRA_ENTRIES[@]}" | jq -R . | jq -s .)" \
        '.installed_files += $extra' "$consumer/$META_REL" > "$consumer/$META_REL.extra"
    mv "$consumer/$META_REL.extra" "$consumer/$META_REL"
    git -C "$consumer" add -A >/dev/null 2>&1
    git -C "$consumer" commit -q --amend -m "chore: install Loom v0.19.100" >/dev/null 2>&1
}

# listed_entries <repo>  -- installed_files, one per line, sorted
listed_entries() {
    jq -r '.installed_files[]' "$1/$META_REL" 2>/dev/null | sort
}

# prune_paths <resync output>  -- every manifest entry the run pruned, sorted
prune_paths() {
    sed -n 's/^  .*pruned[^ ]* *manifest entry \([^ ]*\).*/\1/p' <<<"$1" | sort
}

# would_prune_paths <resync output>  -- the --dry-run counterpart
would_prune_paths() {
    sed -n 's/^  .*would prune[^ ]* manifest entry \([^ ]*\).*/\1/p' <<<"$1" | sort
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

# --- (5) the copy surfaces outside .loom/: one apply run ----------------------
echo ""
echo "Test group 5: one apply run reconciles retired commands, agents and generated skills, and keeps every control (#15475)"
REPO5="$(make_fixture extra-apply)"
add_extra_surfaces "$REPO5"
OUT5="$(cd "$REPO5" && bash "$SCRIPT" 2>&1)"
RC5=$?
if [[ $RC5 -eq 0 ]]; then
    pass "(#15475) the resync run exits 0"
else
    fail "(#15475) the resync run exited $RC5 (out=$OUT5)"
fi
# Both reported entries go in ONE run: one deletes the retired command, the
# other only finds it already absent.
if [[ ! -e "$REPO5/.claude/commands/loom/loom-iteration.md" ]] \
   && grep -q "removed.*commands/loom/loom-iteration\.md" <<<"$OUT5"; then
    pass "(#15475) the retired command still on disk is removed and reported"
else
    fail "(#15475) the retired command on disk was not removed (out=$OUT5)"
fi
if [[ "$(prune_paths "$OUT5")" == "$STALE_SORTED" ]]; then
    pass "(#15475) the run prunes exactly the stale command, agent and skill entries"
else
    fail "(#15475) pruned entries are: $(prune_paths "$OUT5" | tr '\n' ' ')"
fi
if [[ "$(listed_entries "$REPO5")" == "$SURVIVORS_SORTED" ]]; then
    pass "(#15475) installed_files is exactly the survivor list"
else
    fail "(#15475) installed_files differs: $(diff <(printf '%s\n' "$SURVIVORS_SORTED") <(listed_entries "$REPO5") | tr '\n' ' ')"
fi
# A still-shipped file missing locally is a copy gap, not a retirement: the
# walk restores it before the prune runs, and the entry stays.
if [[ "$(cat "$REPO5/.claude/commands/loom/shipped.md" 2>/dev/null)" == "SHIPPED" \
      && "$(cat "$REPO5/.claude/agents/shipped-missing.md" 2>/dev/null)" == "SHIPPED" ]]; then
    pass "(#15475) a still-shipped command and agent missing locally are restored, not pruned"
else
    fail "(#15475) a still-shipped file missing locally was not restored (out=$OUT5)"
fi
if [[ "$(cat "$REPO5/.claude/commands/loom/pinned.md" 2>/dev/null)" == "PINNED" \
      && -L "$REPO5/.claude/commands/loom/linked.md" ]]; then
    pass "(#15475) a pinned and a symlinked retired command are left in place"
else
    fail "(#15475) a pinned or symlinked retired command was removed (out=$OUT5)"
fi
# Generated skills: the marker-aware retirement pass is the only ownership
# evidence, so the removal and the prune travel together.
if [[ ! -e "$REPO5/.agents/skills/loom-retired" ]] \
   && grep -q "removed.*agents-skills/loom-retired/SKILL\.md.*retired generated skill" <<<"$OUT5"; then
    pass "(#15475) a marked retired skill is removed together with its now-empty dir"
else
    fail "(#15475) the marked retired skill was not removed (out=$OUT5)"
fi
if [[ "$(cat "$REPO5/.agents/skills/loom-detached/SKILL.md" 2>/dev/null)" == "mine" \
      && -f "$REPO5/.agents/skills/loom-pinned/SKILL.md" \
      && -L "$REPO5/.agents/skills/loom-linkfile/SKILL.md" \
      && -f "$REPO5/.agents/skills/custom/SKILL.md" ]]; then
    pass "(#15475) a detached, pinned, symlinked and non-loom-* skill are left in place"
else
    fail "(#15475) a skill the retirement pass does not own was removed (out=$OUT5)"
fi
if has_entry "$REPO5" ".agents/skills/loom-gone/SKILL.md"; then
    pass "(#15475) a skill entry already absent before the run is kept: no marker is left to prove it was Loom's"
else
    fail "(#15475) a pre-absent skill entry was pruned without ownership evidence"
fi

# --- (6) preview, apply, and a second apply on those surfaces -----------------
echo ""
echo "Test group 6: --dry-run previews the reconcile and changes nothing, the apply matches it, a second apply is a no-op (#15475)"
REPO6="$(make_fixture extra-preview)"
add_extra_surfaces "$REPO6"
BEFORE6="$(cat "$REPO6/$META_REL")"
OUT6="$(cd "$REPO6" && bash "$SCRIPT" --dry-run 2>&1)"
RC6=$?
if [[ $RC6 -eq 2 ]]; then
    pass "(#15475) a pending reconcile is drift: --dry-run exits 2"
else
    fail "(#15475) --dry-run exited $RC6, expected 2 (out=$OUT6)"
fi
if [[ "$(cat "$REPO6/$META_REL")" == "$BEFORE6" && -z "$(git -C "$REPO6" status --porcelain)" ]]; then
    pass "(#15475) --dry-run leaves the manifest and the whole tree byte-identical"
else
    fail "(#15475) --dry-run modified the tree: $(git -C "$REPO6" status --porcelain | tr '\n' ' ')"
fi
# Entries already absent before the run: the prune is the ONLY change pending
# for them, so the preview has to announce each one.
PREVIEW6="$(would_prune_paths "$OUT6")"
pending_seen=0
for entry in "${PENDING_PRUNES[@]}"; do
    grep -qxF -- "$entry" <<<"$PREVIEW6" && pending_seen=$((pending_seen + 1))
done
if [[ "$pending_seen" -eq "${#PENDING_PRUNES[@]}" ]]; then
    pass "(#15475) --dry-run previews the prune of every entry already absent before the run"
else
    fail "(#15475) --dry-run would prune: $(tr '\n' ' ' <<<"$PREVIEW6") (out=$OUT6)"
fi
if grep -q "would remove.*commands/loom/loom-iteration\.md" <<<"$OUT6" \
   && grep -q "would remove.*agents-skills/loom-retired/SKILL\.md" <<<"$OUT6"; then
    pass "(#15475) --dry-run previews both file removals"
else
    fail "(#15475) --dry-run did not preview the removals (out=$OUT6)"
fi
OUT6B="$(cd "$REPO6" && bash "$SCRIPT" 2>&1)"
# Everything the preview named must land, and the apply must drop exactly the
# stale set. The preview is not expected to name that whole set: an entry
# whose file the run would remove is still on disk at prune time under
# --dry-run (#15478).
if [[ -z "$(comm -23 <(printf '%s\n' "$PREVIEW6") <(prune_paths "$OUT6B"))" \
      && "$(prune_paths "$OUT6B")" == "$STALE_SORTED" ]]; then
    pass "(#15475) the apply prunes everything the preview named, and exactly the stale set"
else
    fail "(#15475) preview='$(tr '\n' ' ' <<<"$PREVIEW6")' apply='$(prune_paths "$OUT6B" | tr '\n' ' ')'"
fi
if [[ "$(listed_entries "$REPO6")" == "$SURVIVORS_SORTED" \
      && ! -e "$REPO6/.claude/commands/loom/loom-iteration.md" \
      && ! -e "$REPO6/.agents/skills/loom-retired/SKILL.md" ]]; then
    pass "(#15475) the apply lands it: the survivor list, with both files removed"
else
    fail "(#15475) the apply did not land what the preview described (out=$OUT6B)"
fi
BEFORE6B="$(cat "$REPO6/$META_REL")"
OUT6C="$(cd "$REPO6" && bash "$SCRIPT" 2>&1)"
if [[ "$(cat "$REPO6/$META_REL")" == "$BEFORE6B" && -z "$(prune_paths "$OUT6C")" ]] \
   && ! grep -q "retired from defaults/, #5981\|retired generated skill" <<<"$OUT6C"; then
    pass "(#15475) a second apply leaves the manifest byte-identical and removes nothing"
else
    fail "(#15475) a second apply was not a no-op (out=$OUT6C)"
fi

# --- (7) the commit advice keeps staging a tracked command deletion -----------
#
# collect_resync_dirt() sends a pure-copy-surface path whose defaults/
# counterpart is gone to RESYNC_RETIRED_PATHS, which the advice never stages
# (#6613). That is right for an unlisted orphan under .loom/ and wrong for a
# retired command: its deletion IS resync output a consumer must commit. So
# the new eligibility lives in the prune alone and never in the shared
# classifier -- this pins that.
echo ""
echo "Test group 7: the --output commit advice still stages the tracked deletion of a retired command (#15475)"
REPO7="$(make_fixture extra-staging)"
add_extra_surfaces "$REPO7"
STAGE7="$WORKDIR/extra-staging-output"
OUT7="$(cd "$REPO7" && bash "$SCRIPT" --output "$STAGE7" 2>&1)"
RC7=$?
if [[ $RC7 -eq 0 ]]; then
    pass "(#15475) the --output apply exits 0"
else
    fail "(#15475) the --output apply exited $RC7 (out=$OUT7)"
fi
if [[ -f "$REPO7/.claude/commands/loom/loom-iteration.md" && ! -e "$STAGE7/.claude/commands/loom/loom-iteration.md" ]]; then
    pass "(#15475) the staged resync deleted the retired command in the staging worktree only"
else
    fail "(#15475) the staged deletion landed in the wrong tree (out=$OUT7)"
fi
# Every `git add --` recipe the run printed (bold/reset codes stripped), with
# spaces around each path so the first and last are matched like the rest.
RECIPES7="$(sed -e $'s/\033\\[[0-9;]*m//g' <<<"$OUT7" | grep -o 'git add -- .*' | tr '\n' ' ')"
if [[ -n "$RECIPES7" ]] \
   && grep -qF -- " .claude/commands/loom/loom-iteration.md " <<<" $RECIPES7 "; then
    pass "(#15475) the staging recipe names the tracked command deletion"
else
    fail "(#15475) the staging recipe dropped the command deletion (recipe='$RECIPES7' out=$OUT7)"
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
