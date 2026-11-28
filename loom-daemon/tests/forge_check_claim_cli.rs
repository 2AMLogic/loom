//! CLI contract for `loom-daemon forge check-claim <issue> [#9453 Phase 1]`.
//!
//! `builder.md`'s "Argument Handling" step 2 and CLAUDE.md's "Builder
//! Workflow" step 0 gate every hand-claim on this command's exit code, so the
//! codes are pinned here end to end through the real binary:
//!
//! | Exit | Meaning | stdout |
//! |---|---|---|
//! | `0` | BLOCKED — the reason token names the blocker | the token |
//! | `1` | verified safe to claim | empty |
//! | `5` | no verdict — a leg could not be read, or the current directory cannot be resolved | empty |
//!
//! The legs themselves are unit-tested through the pure `decide` walk in
//! `src/forge_check_claim.rs`; this suite pins the one outcome only the
//! process boundary can produce — an unresolvable cwd, reached before any leg
//! runs.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::process::Command;

/// An unresolvable cwd is a failure to answer too (#15671): it used to leave
/// as an `Err` through `main`'s generic error exit, whose `1` is
/// `EX_SAFE_TO_CLAIM` — the verified "Claim." answer. So a shell parked in a
/// since-reaped worktree (managed worktrees are removed on merge and by the
/// daemon's reaper) was told the issue was safe to claim with nothing probed.
/// The shell starts in its private, empty cwd, removes it, then `exec`s the
/// binary, which inherits a deleted cwd. `LOOM_GH_BIN` names a binary that
/// does not exist, so no leg can reach the real `gh`.
#[cfg(unix)]
#[test]
fn an_unresolvable_cwd_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new("sh")
        .args([
            "-c",
            r#"rmdir "$1" && exec "$2" forge check-claim "$3""#,
            "sh",
        ])
        .arg(dir.path())
        .arg(env!("CARGO_BIN_EXE_loom-daemon"))
        .arg("42")
        .current_dir(dir.path())
        .env("LOOM_GH_BIN", dir.path().join("gh"))
        .env("LOOM_FORGE_TYPE", "github")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_WORKSPACES_PATH", dir.path().join("workspaces.json"))
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(5), "stderr: {stderr}");
    assert_ne!(
        out.status.code(),
        Some(1),
        "an unresolvable cwd must not reuse the verified safe-to-claim exit code"
    );
    assert!(stdout.is_empty(), "stdout: {stdout}");
    assert!(
        !stderr.contains("safe to claim"),
        "an unresolvable cwd must never read as an all-clear: {stderr}"
    );
    assert!(stderr.contains("NOT an all-clear"), "stderr: {stderr}");
    assert!(
        stderr.contains("current directory"),
        "the failure must name the unresolvable cwd: {stderr}"
    );
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;
