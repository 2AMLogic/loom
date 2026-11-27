//! Which repo paths are Loom-owned or regenerable (#8243).
//!
//! Extracted from `main_health_gate.rs`: the answer to "this table needs
//! another row" is to give the table its own file, not to grow a 1,800-line
//! module. #8078 needed to add one entry (`.loom-local/`) to
//! `LOOM_OWNED_PREFIXES`.
//!
//! This is a pure move. Every constant keeps its name, its value and its
//! documentation, so `git log -M` follows it and the reasoning recorded
//! against each entry — much of it incident-derived — travels with the data
//! it explains.

// ============================================================================
// Dirty-tree ignore list (#3778 transient paths + build-artifact lockfiles,
// #3950)
// ============================================================================

/// Loom-owned transient state path prefixes the gate's dirty-tree check
/// ignores when deciding whether the workspace is safe to sync/reset before a
/// run (#3950). Mirrors `.loom/scripts/check-main-clean.sh`'s
/// `LOOM_OWNED_PREFIXES` — kept in sync by hand since one lives in bash and
/// the other in Rust, except for the daemon-written subset that
/// `DAEMON_RUNTIME_PATTERNS` machine-checks (#15479); both exist to solve the
/// same #3778 problem: Loom's own runtime bookkeeping showing up as "dirty"
/// and false-positiving a check that exists to protect *real* operator edits.
/// A prefix ending in `/` matches a directory subtree; the others match an
/// exact path.
pub(super) const LOOM_OWNED_PREFIXES: &[&str] = &[
    ".loom/sweep-checkpoint/",
    ".loom/sweep-run/",
    ".loom/tokens/",
    // Per-host API-key account pool for native harnesses (#8401), mirroring
    // the bash list. Same never-committed contract as `.loom/tokens/` above:
    // host-local credential material, gitignored by design, so in a repo whose
    // managed .gitignore block predates #8401 it surfaces as untracked dirt
    // and would wedge this gate "dirty" every tick — the `.loom-local/`
    // failure mode below (#8075/#4039), repeated verbatim. Safe to ignore
    // here for the same reason: the gate's remediation is `git reset --hard
    // <remote>` and never `git clean`, so the pool survives.
    ".loom/api-keys/",
    ".loom/accounts.env",
    ".loom/exit-codes/",
    ".loom/stats/",
    ".loom/CANARY",
    ".loom/spawn-loop.pid",
    ".loom/spawn-loop-state.json",
    ".loom/stop-spawn-loop",
    ".loom/locks/",
    ".loom/logs/",
    ".loom/worktrees/",
    ".loom-managed",
    // Host-local config overlay (#4039; added here for #8075, mirroring the
    // bash list). Ungitted by design, so in a repo whose managed .gitignore
    // block predates #8075 it is untracked dirt that would wedge this gate
    // "dirty" forever — the same failure mode the lockfile class below exists
    // to prevent. Safe to ignore here: the gate's remediation is `git reset
    // --hard <remote>` and never `git clean`, so the overlay survives.
    ".loom-local/",
    // Operator stop requests (#15545): `signal.sh stop` writes
    // `.loom/signals/stop-<name>` and the daemon only tests its existence
    // (`check_stop_signals`), so it is operator-written control state, not
    // daemon-written, and stays out of `DAEMON_RUNTIME_PATTERNS`. In a repo
    // whose managed .gitignore block predates the `.loom/signals/` rule it is
    // untracked dirt; the bash mirror's `--quarantine` would stash the stop
    // request away mid-sweep. The trailing slash matches the directory git
    // reports as well as any child path; `.loom/signals-old/` is not matched.
    ".loom/signals/",
    // Daemon-written token-pool runtime files (#15479): the per-repo health
    // cache and its sibling lock (#5014), and the ranking the refresh tick
    // rewrites every cycle (#14584). Written into every workspace now, while
    // their ignore rules only reach a consumer repo's managed .gitignore block
    // after a resync — until then each is untracked dirt that would wedge this
    // gate "dirty" every tick, the `.loom-local/` failure mode above (#8075).
    // The lock is an empty `mkdir` directory, which `git status` never
    // reports; it is listed so the three travel together. Machine-checked
    // against `DAEMON_RUNTIME_PATTERNS` (`init/post_init.rs`) by the parity
    // test below.
    ".loom/account-health.json",
    ".loom/account-health.lock",
    ".loom/account-ranking.codex",
    // The other two daemon-written entries of the audit (#15488), both
    // directories: `.loom/claims/` (`loom-daemon claim` writes
    // `issue-<N>.lock/claim.json`) and `.loom/metrics/` (`validate-phase`'s
    // recovery path writes `recovery-events.json`). Same stale-block hazard
    // as the three above; listed with their trailing `/` so the subtree
    // match above covers every file either writer produces.
    ".loom/claims/",
    ".loom/metrics/",
];

/// Common regenerable lockfile basenames (#3950): a package manager can
/// rewrite one of these with no dependency change (formatting/ordering churn
/// from a `buildGate.command` step like `pnpm install`), leaving tracked-file
/// dirt that would otherwise wedge the dirty-tree check indefinitely — the
/// reported symptom was a lone modified `mcp-loom/package-lock.json`
/// disabling the gate for the whole repo, every tick, forever (a hard reset
/// would have discarded it, but the check refused to run one). Matched by
/// exact basename anywhere in the tree: a small, well-known, documented set,
/// not a repo-specific hardcode.
pub(super) const BUILD_ARTIFACT_LOCKFILE_BASENAMES: &[&str] = &[
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "Cargo.lock",
    "uv.lock",
];

/// The daemon's own re-stamped install-manifest (#4239): rewritten by every
/// `resync-installed.sh` run and has no `defaults/` source counterpart (it is
/// generated, not copied), so it cannot be classified by the byte-match rule
/// below — it is ignorable by exact path instead. A hard reset merely reverts
/// its stamps, which the next resync rewrites anyway.
pub(super) const INSTALL_METADATA_PATH: &str = ".loom/install-metadata.json";

/// Installed↔source surface mapping (#4332) for the loom repo's own
/// dogfooded install: each installed prefix's tracked, edited-upstream
/// counterpart lives under `defaults/`. Mirrors the exact surface table
/// `resync-installed.sh` walks (`defaults/scripts/resync-installed.sh`,
/// search `widened pure-copy surfaces`) — **not** a uniform prefix rewrite:
/// `.loom/bin/` and `.claude/commands/loom/` map into
/// `defaults/.loom/bin/` and `defaults/.claude/commands/loom/`
/// respectively, while the others map straight into `defaults/<name>/`.
/// Only meaningful in a repo that carries a local `defaults/` tree (the loom
/// repo itself); a consumer install has no `defaults/` dir so the byte-match
/// lookup below always misses and classification is unchanged (#4332 is
/// loom-repo-scoped by construction).
///
/// This class is intentionally **not** mirrored into
/// `defaults/scripts/check-main-clean.sh`'s `LOOM_OWNED_PREFIXES` — see the
/// divergence note in that script's header (#4332). That script protects a
/// different property (catching a *builder* writing into the main worktree
/// by mistake, not classifying resync output for the dispatch gate), so a
/// byte-match there could mask real contamination instead of only ignoring
/// safe, known-regenerable dirt.
pub(super) const INSTALLED_SURFACE_PREFIXES: &[(&str, &str)] = &[
    (".loom/hooks/", "defaults/hooks/"),
    (".loom/scripts/", "defaults/scripts/"),
    (".loom/roles/", "defaults/roles/"),
    (".loom/docs/", "defaults/docs/"),
    (".loom/bin/", "defaults/.loom/bin/"),
    (".claude/commands/loom/", "defaults/.claude/commands/loom/"),
];

/// Single-file surfaces of the same installed↔source mapping (#15554):
/// files `resync-installed.sh` copies one at a time with `sync_one`, which
/// no directory prefix can describe. Matched by exact path, so
/// `.loom/pricing.json.bak` is not covered, and classified by the same
/// byte-match rule as [`INSTALLED_SURFACE_PREFIXES`].
///
/// - `.loom/pricing.json` ← `defaults/pricing.json`: the model rate card
///   (#8177), which a vendor price change reaches on a resync. It was
///   missing from this mapping the way it was missing from both shell
///   classifiers at once (#15519), so a resync that refreshed it left the
///   opt-in gate skipping every tick as `dirty-tree` (#4332's stall).
pub(super) const INSTALLED_SURFACE_FILES: &[(&str, &str)] =
    &[(".loom/pricing.json", "defaults/pricing.json")];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::{DAEMON_RUNTIME_PATTERNS, EPHEMERAL_PATTERNS};
    use std::collections::BTreeSet;

    const CHECK_MAIN_CLEAN: &str = include_str!("../../../defaults/scripts/check-main-clean.sh");

    /// The bare paths in a line of the bash array: entries are quoted there,
    /// while this table stores them unquoted.
    fn unquoted(tokens: &str) -> impl Iterator<Item = String> + '_ {
        tokens
            .split_whitespace()
            .map(|t| t.trim_matches(['"', '\'']).to_string())
    }

    /// The entries of `LOOM_OWNED_PREFIXES=( ... )` in
    /// `defaults/scripts/check-main-clean.sh`, the bash mirror of this table.
    fn bash_owned_prefixes() -> BTreeSet<String> {
        let needle = "\nLOOM_OWNED_PREFIXES=(";
        let start = CHECK_MAIN_CLEAN
            .find(needle)
            .expect("check-main-clean.sh must declare LOOM_OWNED_PREFIXES=( ... )");
        let mut entries = BTreeSet::new();
        let mut closed = false;
        for line in CHECK_MAIN_CLEAN[start + needle.len()..].lines() {
            // The array's comments cite issues in parentheses, so the closing
            // `)` is looked for in each line's code only, before any `#`.
            let code = match line.split_once('#') {
                Some((code, _)) => code,
                None => line,
            };
            if let Some(idx) = code.find(')') {
                entries.extend(unquoted(&code[..idx]));
                closed = true;
                break;
            }
            entries.extend(unquoted(code));
        }
        assert!(closed, "LOOM_OWNED_PREFIXES array must be closed with `)`");
        // Carried by the array since #8075. Without it either the parse lost
        // the array or that entry was removed, and both are wrong.
        assert!(
            entries.contains(".loom-local/"),
            "check-main-clean.sh's LOOM_OWNED_PREFIXES does not yield .loom-local/: parsed \
             {entries:?}"
        );
        entries
    }

    /// The table's matching rule, as `is_ignorable_dirt_with_readers` applies
    /// it: a trailing `/` matches a subtree, anything else is an exact path.
    fn loom_owned(path: &str) -> bool {
        LOOM_OWNED_PREFIXES.iter().any(|prefix| {
            if prefix.ends_with('/') {
                path.starts_with(prefix)
            } else {
                path == *prefix
            }
        })
    }

    #[test]
    fn the_host_local_overlay_is_loom_owned() {
        // The one line that broke `main`. #8078 added `.loom-local/` to this
        // table and shipped it with no test. This module has room for one,
        // so the entry gets its coverage here.
        assert!(loom_owned(".loom-local/config.json"));
        assert!(loom_owned(".loom-local/anything/nested.txt"));
    }

    #[test]
    fn both_per_host_credential_pools_are_loom_owned() {
        // The bash mirror (`defaults/scripts/check-main-clean.sh`'s
        // `LOOM_OWNED_PREFIXES`) and this table are kept in sync by hand, and
        // nothing in CI enforces the parity — #8401 added `.loom/api-keys/`
        // to the bash side first and this side was the half that got missed.
        // Pin both pools here so the next credential pool notices.
        assert!(loom_owned(".loom/tokens/alpha.json"));
        assert!(loom_owned(".loom/api-keys/zai/alpha.env"));
    }

    #[test]
    fn a_trailing_slash_matches_a_subtree_and_a_bare_name_does_not() {
        // `.loom-managed` is an exact-path entry; `.loom/tokens/` is a
        // subtree. Getting these two rules the wrong way round would either
        // ignore real operator edits or wedge the gate dirty forever.
        assert!(loom_owned(".loom/tokens/whatever.json"));
        assert!(loom_owned(".loom-managed"));
        assert!(
            !loom_owned(".loom-managed-extra"),
            "an exact-path entry must not match by prefix"
        );
    }

    #[test]
    fn an_ordinary_source_edit_is_never_loom_owned() {
        // The property the table exists to protect: a real operator edit must
        // stay visible as dirt, or the gate resets over someone's work.
        for p in [
            "src/main.rs",
            "defaults/scripts/merge-pr.sh",
            ".loom-local-but-not-really/x",
            "README.md",
        ] {
            assert!(!loom_owned(p), "{p} must remain non-ignorable");
        }
    }

    #[test]
    fn the_daemon_written_runtime_files_are_loom_owned() {
        // #15479: the token pool writes these into every workspace, and
        // neither owned list carried them. In a consumer repo whose managed
        // .gitignore block predates #14594 the file is untracked dirt that
        // wedges this gate "dirty" — the `.loom-local/` failure mode of #8075,
        // repeated. Pinned literally so deleting an entry from
        // `DAEMON_RUNTIME_PATTERNS` cannot retire the coverage with it.
        for p in [
            ".loom/account-health.json",
            ".loom/account-health.lock",
            ".loom/account-ranking.codex",
        ] {
            assert!(loom_owned(p), "{p} must stay in LOOM_OWNED_PREFIXES (#15479)");
        }
    }

    #[test]
    fn the_daemon_written_runtime_directories_are_loom_owned() {
        // #15488: the audit of the remaining EPHEMERAL_PATTERNS candidates
        // found two more daemon-written entries, both directories —
        // `loom-daemon claim` writes `.loom/claims/issue-<N>.lock/claim.json`
        // and `validate-phase`'s recovery path writes
        // `.loom/metrics/recovery-events.json`. Both are listed with a
        // trailing `/`, so what the lists must get right is the subtree match
        // on a child path; the entries themselves are pinned by the parity
        // test below.
        for p in [
            ".loom/claims/issue-42.lock/claim.json",
            ".loom/metrics/recovery-events.json",
        ] {
            assert!(loom_owned(p), "{p} must be Loom-owned (#15488)");
        }
        // A sibling that merely extends the directory's name is not Loom
        // state: the trailing `/` ends the prefix, so a stray there stays
        // visible as contamination.
        for p in [
            ".loom/claims-old/issue-42.lock/claim.json",
            ".loom/metrics-old/recovery-events.json",
        ] {
            assert!(!loom_owned(p), "{p} must not match a directory entry by its name prefix");
        }
    }

    #[test]
    fn every_daemon_runtime_pattern_is_in_both_owned_path_lists() {
        // The parity test behind `DAEMON_RUNTIME_PATTERNS` (#15479): a runtime
        // path the managed block ignores is still dirt in every consumer repo
        // until a resync delivers the rule, so both backstops must carry it
        // from the day it ships — this table for the daemon's dirty-tree gate,
        // the bash array for `check-main-clean.sh`. A path added to the const
        // without both lists fails here instead of in a consumer's sweep.
        let bash = bash_owned_prefixes();
        for p in DAEMON_RUNTIME_PATTERNS {
            assert!(
                EPHEMERAL_PATTERNS.contains(p),
                "DAEMON_RUNTIME_PATTERNS entry {p:?} is missing from EPHEMERAL_PATTERNS — the \
                 managed .gitignore block must ignore it too (#15479)"
            );
            assert!(
                LOOM_OWNED_PREFIXES.contains(p),
                "DAEMON_RUNTIME_PATTERNS entry {p:?} is missing from this table — the daemon's \
                 dirty-tree gate counts the file as dirt in a consumer repo whose managed \
                 .gitignore block is stale (#15479)"
            );
            assert!(
                bash.contains(*p),
                "DAEMON_RUNTIME_PATTERNS entry {p:?} is missing from defaults/scripts/check-main-clean.sh's \
                 LOOM_OWNED_PREFIXES — the backstop exits 3 on the file and --quarantine stashes \
                 it (#15479)"
            );
        }
    }
}
