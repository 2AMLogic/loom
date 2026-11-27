//! The installed `worktree.sh` on a tools-only host, against the real
//! `loom-tools` (#14476, part of #14397; design
//! `docs/design/loom-tools-cli-boundary.md` §13.11).
//!
//! `defaults/scripts/tests/test-loom-tools-worktree.sh` owns the scenarios: it
//! installs the tools binary and the shipped scripts into a checkout whose path
//! contains a space, with no `loom-daemon` in any locator, and runs the real
//! create, enter, reuse and remove path plus every create-time guard against a
//! stub forge. It prints SKIP and exits 0 when no `loom-tools` is built, which
//! is what a bare shell CI leg has; this test hands it the binary cargo just
//! built so the wrapper is exercised on every `cargo test`.
//!
//! The second test is the complementary half: the worktree suites that predate
//! the tools binary, unchanged, with the tools binary pinned where they expect
//! a daemon. Their assertions were written against the shell implementation
//! `loom-daemon` replaced; running them against `loom-tools` is the evidence
//! that the shared handler behaves the same from the tools entry point.

use std::path::PathBuf;
use std::process::Command;

fn suite_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/tests")
}

fn run_suite(name: &str, envs: &[(&str, &str)]) -> (bool, String, String) {
    let out = Command::new("bash")
        .arg(suite_dir().join(name))
        .envs(envs.iter().copied())
        .output()
        .unwrap_or_else(|e| panic!("run {name}: {e}"));
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn the_installed_worktree_wrapper_runs_with_no_daemon() {
    let (ok, stdout, stderr) = run_suite(
        "test-loom-tools-worktree.sh",
        &[("LOOM_TOOLS_TEST_BIN", env!("CARGO_BIN_EXE_loom-tools"))],
    );
    assert!(ok, "{stdout}\n{stderr}");
    assert!(
        !stdout.contains("SKIP"),
        "the suite must run against the built binary:\n{stdout}"
    );
    assert!(stdout.contains("ALL PASSED"), "{stdout}");
}

#[test]
fn retained_worktree_suites_pass_against_the_tools_binary() {
    let tools = env!("CARGO_BIN_EXE_loom-tools");
    for name in [
        "test-worktree-remove.sh",
        "test-worktree-json-purity.sh",
        "test-worktree-issue-lock.sh",
    ] {
        // The pins each suite's harness already honours first, and the one
        // setting the tools entry point needs for a forge-reaching call.
        let (ok, stdout, stderr) = run_suite(
            name,
            &[
                ("LOOM_DAEMON_SELF_BIN", tools),
                ("LOOM_DAEMON_BIN", tools),
                ("LOOM_MODE", "session"),
            ],
        );
        assert!(ok, "{name} against loom-tools:\n{stdout}\n{stderr}");
        assert!(stdout.contains("Failed: 0"), "{name}:\n{stdout}");
    }
}
