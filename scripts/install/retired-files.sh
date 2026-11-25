#!/usr/bin/env bash
# Retired-file cleanup (content-gated) for install-loom.sh (#3572, #14846).
#
# Sourced by scripts/install-loom.sh and by Section 5b of
# scripts/test-installer.sh. It lives in its own file because install-loom.sh
# runs argv parsing and installs at top level, so a test cannot source it
# (same split as manifest.sh, dogfood-commands.sh, provision-*.sh).
#
# Callers must provide the install.sh logging helpers `info`, `success` and
# `warning`. Defining this file has no side effects beyond the function and
# the LOOM_RETIRED_FILES table below.
#
# A handful of files Loom once shipped were later RETIRED outright (deleted
# upstream, not renamed). The generic stale-file sweep deliberately will NOT
# remove them:
#   * The #3492 ownership-boundary intersection preserves any stale-manifest
#     entry that is no longer in the current defaults/ ownership set -- a
#     retired path is by definition not owned, so it lands in
#     PRESERVED_NOT_OWNED and is never git-rm'd. That defense-in-depth is
#     correct (it protects consumer-authored files captured by pre-#3450
#     over-broad manifests) and must stay.
#   * A retired file that was never in the previous manifest at all is never
#     even a sweep candidate.
# So a retired file lingers on disk in every consumer that installed before the
# retirement. loom_remove_retired_files removes such strays narrowly and safely.
#
# Gate: a frozen allowlist of (retired path -> sha256 of every version Loom
# ever shipped at that path). A file is removed ONLY when it exists AND its
# content hash matches a shipped digest -- i.e. it is byte-identical to
# something Loom shipped and therefore unmodified. A consumer who customized
# the file (hash matches none) is left untouched with an informational line.
# Absent -> no-op. Idempotent by construction (once removed, the existence
# check is false on every subsequent run).
#
# Issue #3572: retire the stray `.claude/commands/loom/release.md` left behind
# by #3563 (which deleted the shipped `/loom:release` skill but deliberately
# did not sweep existing consumer copies). release.md's only placeholder is the
# Claude-Code *runtime* token {{workspace}} -- it is NOT install-time
# substituted (see substitute_template_variables in
# loom-daemon/src/init/templates.rs), so the shipped bytes are identical across
# all consumers and a content-hash gate is exact. The digest set is frozen:
# release.md will never ship again, so no maintenance burden accrues.
#
# The Rust mirror of this table is RETIRED_FILES in
# loom-daemon/src/init/retired.rs; a parity test there parses the heredoc below
# (keep its delimiter and row format).

_loom_sha256() {
  # Portable sha256 -> bare hex digest (shasum on macOS, sha256sum on Linux).
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" 2>/dev/null | awk '{print $1}'
  elif command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" 2>/dev/null | awk '{print $1}'
  else
    echo ""
  fi
}

# Frozen retired-file allowlist. One "<relative-path> <sha256>" row per shipped
# version; multiple rows per path enumerate every version. Comment (#) and
# blank lines are ignored. Append-only — never remove a historical digest.
LOOM_RETIRED_FILES=$(cat <<'RETIRED'
# .claude/commands/loom/release.md — every version shipped in the #3495 -> #3563
# window (retired by #3563 / PR #3571). Any match is unmodified Loom content.
.claude/commands/loom/release.md 11aef217942f45bd03d90a24e5efae9209041cb59f09c888df4dc7e8208910dd
.claude/commands/loom/release.md 0df6c20846c98850413243362c80dea2fd01330c8d97033ef5f7c3989578fe8c
.claude/commands/loom/release.md c45841f8da42d1bda20bc180c8a93d14242238d9a2c1d9f5a1bdac32b5e9e556
.claude/commands/loom/release.md d91e198e977ad7799f44fa1a6827c9836bca6d31c9357ed92fc400a3c88381de
.claude/commands/loom/release.md 0d7030dd14f32f6f382a6430cd04e5f0475825d567aaed7570b73a4c43128ad1
.claude/commands/loom/release.md 4a077ed25cb44add0afbc4d6bda23cb372f5f3c4c2ef23b7a24b586e66e4f3e7
.claude/commands/loom/release.md 5f9930dc72a263866122b18018a64b8fed4bd53ef623d0eef27ed1e31fa0502f
.claude/commands/loom/release.md b7fae9d13d2bfaee3bde514cabe44ac70b6551351a9e49357ede00f82c17cf35
.claude/commands/loom/release.md f6523d9be058e40397f0ce30c08a8f2b60e9b38adae04bd7c919e0cc840acfec
.claude/commands/loom/release.md 29a845f7f8912545d23832551753304df6e72dd4a9c8082c2d8ada1f09f449e1
.claude/commands/loom/release.md 795c1df1d3f3706ba448482b037a0c9e4eb6272a719adb2688b9ddfc91ab4de6
# .claude/agents/loom-shepherd.md — every version shipped from #1259/#1264
# (initial subagent files) through #3871, before the v0.10.0 shepherd/daemon
# deletion (#3884) removed it outright (#4097). .claude/ is merge-copied with
# no orphan removal, so a pre-#3884 install keeps this stray forever unless
# swept here.
.claude/agents/loom-shepherd.md 1cbca10c8b0352fc3db636e0abb6fdf25ddcfb6138b7a21ab5faeb8b21f730a2
.claude/agents/loom-shepherd.md 2be39b1d7978dbb3a8d502eaed2cfff9a8f9832aae801c8683a425405c0a2753
.claude/agents/loom-shepherd.md 35830eb68c9ab3ad1cbeaf6ce2b62c7d919f3e06cc5234c46ad2c6c4f637c8ff
.claude/agents/loom-shepherd.md 6c7e207a5f040675f31447427b4241999adab4d58831e8d5bc605c2ac5d500b2
.claude/agents/loom-shepherd.md 8cfc89428135cb55c91a6ae768bbd3373dbe2a1d22eb088694bc158ce0091d34
RETIRED
)

# loom_remove_retired_files <read_root> <git_root> <confirm>
#   read_root  directory the retired paths are read and hashed under
#              (install-loom.sh passes "$TARGET_PATH").
#   git_root   directory `git rm` runs in, via `git -C`
#              (install-loom.sh passes "$PWD", the install worktree -- it has
#              cd'd there, so this preserves today's behavior).
#   confirm    "yes" removes without asking; "prompt" asks [y/N] first.
# Never relies on the caller's cwd. Always returns 0.
loom_remove_retired_files() {
  local read_root="$1" git_root="$2" confirm="$3"
  local -a retired_remove=() retired_preserve=()
  local retired_paths retired_path file_hash hash_matched allow_path allow_hash f

  # Unique retired paths (a path may have several allowed digests).
  retired_paths=$(printf '%s\n' "$LOOM_RETIRED_FILES" \
    | awk 'NF>=2 && $1 !~ /^#/ {print $1}' | sort -u)
  while IFS= read -r retired_path; do
    [[ -n "$retired_path" ]] || continue
    [[ -f "$read_root/$retired_path" ]] || continue
    file_hash=$(_loom_sha256 "$read_root/$retired_path")
    hash_matched=false
    if [[ -n "$file_hash" ]]; then
      while read -r allow_path allow_hash; do
        [[ -n "$allow_path" && "${allow_path:0:1}" != "#" ]] || continue
        if [[ "$allow_path" == "$retired_path" && "$allow_hash" == "$file_hash" ]]; then
          hash_matched=true
          break
        fi
      done <<< "$LOOM_RETIRED_FILES"
    fi
    if [[ "$hash_matched" == "true" ]]; then
      retired_remove+=("$retired_path")
    else
      retired_preserve+=("$retired_path")
    fi
  done <<< "$retired_paths"

  # Preserved: retired path present, but content matches no shipped version.
  if (( ${#retired_preserve[@]} > 0 )); then
    for f in "${retired_preserve[@]}"; do
      info "preserving ${f} (retired by Loom, but on-disk content matches no shipped version -- likely consumer-customized; leaving in place)"
    done
  fi

  # Removed: retired path present with unmodified, shipped-identical content.
  if (( ${#retired_remove[@]} > 0 )); then
    echo ""
    info "Retired Loom files still present (deleted upstream, unmodified copy on disk):"
    for f in "${retired_remove[@]}"; do
      echo "    - $f"
    done
    echo ""
    local proceed=true confirm_answer=""
    if [[ "$confirm" != "yes" ]]; then
      read -r -p "Remove these ${#retired_remove[@]} retired file(s)? [y/N] " confirm_answer
      [[ "$confirm_answer" =~ ^[Yy]$ ]] || proceed=false
    fi
    if [[ "$proceed" == "true" ]]; then
      local deleted=0
      for f in "${retired_remove[@]}"; do
        if git -C "$git_root" rm --quiet --force "$f" 2>/dev/null; then
          (( deleted++ )) || true
        else
          warning "Could not remove retired file: $f (may have been removed already or is untracked)"
        fi
      done
      success "Removed $deleted retired Loom file(s)"
    else
      info "Skipping retired file removal (user declined)"
    fi
  fi
  return 0
}
