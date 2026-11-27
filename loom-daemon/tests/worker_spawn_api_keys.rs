//! Native worker seam x API-key account pool (issue #8401): explicit env >
//! pool > fail-closed 78, driven through the real `spawn-worker` path.
//!
//! A sibling of `worker_spawn.rs` rather than a section of it: these tests
//! get their own file. The `worker` / `config` helpers below are therefore deliberate copies of
//! that file's (`fixture` is shared via `support/worker_cli.rs`). Keep `worker` in step with it — in
//! particular the `FIXTURE_VERSION` line, without which the OpenCode adapter's
//! pre-exec `--version` probe (#8438) refuses the fixture.
use std::process::Command;

#[path = "support/worker_cli.rs"]
mod worker_cli;
use worker_cli::fixture;

fn worker(root: &std::path::Path, runtime: &str) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    c.args(["spawn-worker", "--"])
        .current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_RUNTIME", runtime)
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        // Never let a test reach the operator's real `~/.loom/api-keys`. Tests
        // that exercise the shared pool override this with a tempdir.
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        )
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture())
        .env("FIXTURE_VERSION", "1.18.31")
        .env_remove("FIXTURE_VERSION_EXIT");
    c
}

fn config(root: &std::path::Path, value: serde_json::Value) {
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(root.join(".loom/config.json"), value.to_string()).unwrap();
}

const POOL_PROVIDER: &str = "loomtest";
const ALPHA_KEY: &str = "fake-alpha-key-must-never-be-logged";
const BETA_KEY: &str = "fake-beta-key-must-never-be-logged";

/// A profile whose credential is pool-managed. `credentialEnv` names a
/// variable no real environment sets, so the pool is what decides.
fn pooled_profile(root: &std::path::Path) {
    config(
        root,
        serde_json::json!({"runtimes":{"defaultModelProfile":"pooled","modelProfiles":{"pooled":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":POOL_PROVIDER,
            "credentialTargets":{"pi":"LOOM_TEST_HARNESS_SECRET","opencode":"LOOM_TEST_HARNESS_SECRET"}
        }}}}),
    );
}

fn register(root: &std::path::Path, name: &str, key: &str) {
    let dir = root.join(".loom/api-keys").join(POOL_PROVIDER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(format!("{name}.env")), format!("LOOM_TEST_PROVIDER_SECRET={key}\n"))
        .unwrap();
}

/// Pin the round-robin cursor so the alternation assertion is deterministic.
fn seed_cursor(root: &std::path::Path) {
    std::fs::write(
        root.join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".rotation_cursor"),
        "0",
    )
    .unwrap();
}

fn launch_field(stderr: &str, field: &str) -> String {
    let line = stderr
        .lines()
        .find(|l| l.starts_with("# LOOM_LAUNCH "))
        .unwrap_or_else(|| panic!("no LOOM_LAUNCH record in: {stderr}"));
    let record: serde_json::Value =
        serde_json::from_str(line.trim_start_matches("# LOOM_LAUNCH ")).unwrap();
    record[field].as_str().unwrap_or("null").to_string()
}

#[test]
fn pooled_credential_alternates_accounts_and_names_only_the_account() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    register(d.path(), "beta", BETA_KEY);
    seed_cursor(d.path());

    let mut seen = Vec::new();
    for _ in 0..4 {
        let log = d.path().join("worker.log");
        let out = worker(d.path(), "opencode")
            .args(["--log", log.to_str().unwrap(), "-p", "hello"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let text = std::fs::read_to_string(&log).unwrap();
        seen.push(launch_field(&text, "credentialAccount"));
        assert_eq!(launch_field(&text, "credentialSource"), "pool");
        assert_eq!(launch_field(&text, "credentialProvider"), POOL_PROVIDER);
        // The launch record, the log, and the child's own stdout must all be
        // free of key material — the `strings`-over-every-retained-log criterion.
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        for haystack in [&text, &stdout] {
            for key in [ALPHA_KEY, BETA_KEY] {
                assert!(!haystack.contains(key), "key material leaked: {haystack}");
            }
        }
        std::fs::remove_file(&log).unwrap();
    }
    assert_eq!(
        seen,
        vec!["alpha", "beta", "alpha", "beta"],
        "consecutive spawns must alternate"
    );
}

#[test]
fn pooled_credential_reaches_the_child_under_the_harness_variable() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    let out = worker(d.path(), "pi")
        .env("LOOM_TEST_EXPECTED_SECRET", ALPHA_KEY)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("credential_pool_matches=true"), "{text}");
}

#[test]
fn an_explicit_credential_env_still_wins_over_the_pool() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    let out = worker(d.path(), "opencode")
        .env("LOOM_TEST_PROVIDER_SECRET", "fake-explicit-secret")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("credential_alias_matches=true"), "{stdout}");
    assert_eq!(launch_field(&stderr, "credentialSource"), "env");
    assert_eq!(launch_field(&stderr, "credentialAccount"), "null");
    assert!(!stderr.contains("fake-explicit-secret") && !stdout.contains("fake-explicit-secret"));
}

#[test]
fn an_all_disabled_pool_fails_closed_at_78_without_key_material() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    register(d.path(), "beta", BETA_KEY);
    std::fs::write(
        d.path()
            .join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".disabled"),
        "alpha\nbeta\n",
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no usable API-key account"), "{stderr}");
    assert!(stderr.contains("disabled by operator"), "{stderr}");
    assert!(stderr.contains("deciding binary:"), "{stderr}");
    for key in [ALPHA_KEY, BETA_KEY] {
        assert!(!stderr.contains(key), "{stderr}");
    }
    assert!(out.stdout.is_empty());
}

#[test]
fn a_host_with_no_pool_keeps_the_pre_pool_behaviour() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    // No accounts registered: the harness is left to its own auth store, and
    // the spawn is NOT failed closed (#8363's trial path).
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(launch_field(&String::from_utf8_lossy(&out.stderr), "credentialSource"), "none");
}

#[test]
fn a_malformed_credential_pool_name_is_rejected_before_launch() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"bad","modelProfiles":{"bad":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":"../escape",
            "credentialTargets":{"pi":"LOOM_TEST_HARNESS_SECRET"}
        }}}}),
    );
    let out = worker(d.path(), "pi")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(78));
    assert!(out.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// Regression tests for the PR #8428 review: the two fail-open paths, on the
// real spawn path, with the controls the Judge used.
// ---------------------------------------------------------------------------

/// The verdict's third table row. Same all-disabled pool as the test above,
/// with the provider directory made unreadable: before the fix this exited 0
/// with `credentialSource:"none"` and the harness launched on its own auth
/// store.
#[cfg(unix)]
#[test]
fn an_unreadable_pool_fails_closed_at_78_instead_of_launching() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    let dir = d.path().join(".loom/api-keys").join(POOL_PROVIDER);
    std::fs::write(dir.join(".disabled"), "alpha\n").unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
    let as_root = std::fs::read_dir(&dir).is_ok();
    let out = worker(d.path(), "opencode")
        .env("LOOM_TEST_EXPECTED_SECRET", ALPHA_KEY)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    // Restore before asserting so the tempdir can always be cleaned up.
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    if as_root {
        return; // permission bits do not apply
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(out.stdout.is_empty(), "the harness must not have launched");
    assert!(
        !stderr.lines().any(|l| l.starts_with("# LOOM_LAUNCH ")),
        "the harness must not have launched: {stderr}"
    );
    assert!(
        stderr.contains("# LOOM_LAUNCH_REJECTED code=78 stage=credential"),
        "must classify the typed rejection boundary: {stderr}"
    );
    assert!(stderr.contains(&dir.display().to_string()), "must name the path: {stderr}");
    assert!(stderr.contains("PermissionDenied"), "must name the error: {stderr}");
    assert!(!stderr.contains(ALPHA_KEY), "{stderr}");
}

/// A torn / zero-length `.bad_accounts.json` must not return an exhausted
/// account to selection.
#[test]
fn a_torn_bad_marks_file_fails_closed_at_78() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    register(d.path(), "alpha", ALPHA_KEY);
    std::fs::write(
        d.path()
            .join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".bad_accounts.json"),
        "",
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("not a valid bad-marks file"), "{stderr}");
    assert!(out.stdout.is_empty());
}

/// Per-provider root resolution: a per-repo pool for some *other* provider
/// must not hide the shared machine-level pool for this profile's provider.
/// Before the fix this spawn ran with `credentialSource:"none"`.
#[test]
fn a_per_repo_pool_for_another_provider_does_not_hide_the_shared_pool() {
    let d = tempfile::tempdir().unwrap();
    let shared = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    let other = d.path().join(".loom/api-keys/othertest");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("local.env"), "OTHERTEST_API_KEY=fake-other-key\n").unwrap();
    let pooled = shared.path().join(POOL_PROVIDER);
    std::fs::create_dir_all(&pooled).unwrap();
    std::fs::write(
        pooled.join("shared-one.env"),
        format!("LOOM_TEST_PROVIDER_SECRET={BETA_KEY}\n"),
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .env("LOOM_SHARED_API_KEYS_DIR", shared.path())
        .env("LOOM_TEST_EXPECTED_SECRET", BETA_KEY)
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(launch_field(&stderr, "credentialSource"), "pool");
    assert_eq!(launch_field(&stderr, "credentialAccount"), "shared-one");
    assert!(String::from_utf8_lossy(&out.stdout).contains("credential_pool_matches=true"));
}

/// An account file that assigns a different variable than the profile's
/// `credentialEnv` is never injected under the profile's harness variable.
#[test]
fn an_account_for_the_wrong_variable_is_not_injected() {
    let d = tempfile::tempdir().unwrap();
    pooled_profile(d.path());
    let dir = d.path().join(".loom/api-keys").join(POOL_PROVIDER);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("stray.env"), format!("SOME_OTHER_API_KEY={ALPHA_KEY}\n")).unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("assigns SOME_OTHER_API_KEY"), "{stderr}");
    assert!(!stderr.contains(ALPHA_KEY), "{stderr}");
    assert!(out.stdout.is_empty());
}

// ---------------------------------------------------------------------------
// Regression tests for the second PR #8428 review round: a profile with no
// `credentialTargets` entry for the launching harness previously never
// consulted the pool at all (`profiles::credentials` returned an empty pair
// list, so `credential::resolve` took the zero-pairs branch and returned
// `Source::None` before `pool_provider` / `is_pooled` ever ran).
// ---------------------------------------------------------------------------

/// A profile with `credentialPool` set but no `credentialTargets` entry for
/// this harness at all. Before the fix this exited 0 with
/// `credentialSource:"none"` even though every account was disabled -- the
/// exact "operator disabled every account to stop spend" failure the
/// all-disabled-pool test above already covers for a *targeted* profile.
#[test]
fn a_profile_with_no_credential_target_for_the_harness_still_consults_the_pool() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"untargeted","modelProfiles":{"untargeted":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":POOL_PROVIDER
        }}}}),
    );
    register(d.path(), "alpha", ALPHA_KEY);
    std::fs::write(
        d.path()
            .join(".loom/api-keys")
            .join(POOL_PROVIDER)
            .join(".disabled"),
        "alpha\n",
    )
    .unwrap();
    let out = worker(d.path(), "opencode")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("no usable API-key account"), "{stderr}");
    assert!(out.stdout.is_empty(), "the harness must not have launched");
}

/// Same untargeted profile, but with a usable account: the pooled value must
/// reach the child under the source variable's own name -- the implicit
/// target inheritance would have used had the value been exported instead.
#[test]
fn a_profile_with_no_credential_target_injects_the_pooled_value_under_the_source_name() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"untargeted","modelProfiles":{"untargeted":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":"LOOM_TEST_PROVIDER_SECRET",
            "credentialPool":POOL_PROVIDER
        }}}}),
    );
    register(d.path(), "alpha", ALPHA_KEY);
    let out = worker(d.path(), "opencode")
        .env("FIXTURE_PRINT_ENV", "LOOM_TEST_PROVIDER_SECRET")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    assert_eq!(launch_field(&stderr, "credentialSource"), "pool");
    assert_eq!(launch_field(&stderr, "credentialAccount"), "alpha");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(&format!("child_env LOOM_TEST_PROVIDER_SECRET={ALPHA_KEY}")),
        "{stdout}"
    );
}

/// `credentialPool` on an array-form `credentialEnv` was silently accepted
/// and ignored -- the array form declares a required set, and one pooled
/// account (a single `KEY=value`) cannot satisfy more than one member of it.
#[test]
fn a_credential_pool_on_an_array_form_credential_env_is_rejected_before_launch() {
    let d = tempfile::tempdir().unwrap();
    config(
        d.path(),
        serde_json::json!({"runtimes":{"defaultModelProfile":"arr","modelProfiles":{"arr":{
            "model":"m",
            "providers":{"pi":"p","opencode":"p"},
            "credentialEnv":["LOOM_TEST_PROVIDER_SECRET"],
            "credentialPool":POOL_PROVIDER
        }}}}),
    );
    let out = worker(d.path(), "pi")
        .env("LOOM_TEST_PROVIDER_SECRET", "fake-explicit-secret")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{stderr}");
    assert!(stderr.contains("single-string credentialEnv"), "{stderr}");
    assert!(out.stdout.is_empty());
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;

// ---- Tier-3 pooled pilot: `agy` (#14945) ----
//
// The Antigravity adapter is the one tier-3 runtime whose manifest declares
// `credentialPool`, so its key comes from the gemini API-key pool selected
// in the worker process itself — these drive the REAL spawn-agy.sh →
// spawn-generic-launch.sh → spawn-generic.sh chain with a fake `agy` CLI,
// exactly the "fake CLI through the worker entry point" the issue's test
// plan asks for. No `.loom/runtimes/` is created, so the manifest
// (declaration included) resolves through the daemon binary's bundled
// fallback — the consumer-install path the bundled table exists for.
mod agy {
    use super::launch_field;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    const POOL_KEY: &str = "fake-agy-pool-key-must-never-be-logged";
    const AMBIENT_KEY: &str = "fake-agy-ambient-key-must-never-be-logged";

    struct AgyWorkspace {
        root: tempfile::TempDir,
        bin: tempfile::TempDir,
        capture: PathBuf,
    }

    impl AgyWorkspace {
        fn new() -> Self {
            Self::in_workspace(".tmp")
        }

        /// A disposable workspace whose directory name is `prefix` followed
        /// by tempfile's random suffix — how the policy's read rule meets a
        /// path that needs JSON escaping, or cannot be a rule at all.
        fn in_workspace(prefix: &str) -> Self {
            let root = tempfile::Builder::new().prefix(prefix).tempdir().unwrap();
            let bin = tempfile::tempdir().unwrap();
            let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
            let src = repo.join("defaults/scripts");
            let scripts = root.path().join(".loom/scripts");
            std::fs::create_dir_all(scripts.join("lib")).unwrap();
            for name in [
                "spawn-agy.sh",
                "spawn-generic-launch.sh",
                "spawn-generic.sh",
            ] {
                std::fs::copy(src.join(name), scripts.join(name)).unwrap();
            }
            std::fs::copy(
                src.join("lib/locate-daemon-bin.sh"),
                scripts.join("lib/locate-daemon-bin.sh"),
            )
            .unwrap();
            for name in [
                "spawn-agy.sh",
                "spawn-generic-launch.sh",
                "spawn-generic.sh",
            ] {
                std::fs::set_permissions(
                    scripts.join(name),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
            // The fake CLI records one `ARG <token>` line per argv element
            // plus the environment facts under test, and prints NOTHING to
            // stdout — a leaked key must have nowhere to hide. The settings
            // are captured flattened, both from the file the `--settings`
            // argument names and from the CLI's documented default location
            // under the isolated HOME. The capture path travels by
            // environment: it sits in the workspace, whose name some tests
            // make unquotable in shell source.
            let capture = root.path().join("agy-capture.txt");
            std::fs::write(
                bin.path().join("agy"),
                r#"#!/bin/sh
flat() { if [ -f "$1" ]; then tr -d '\n\r' <"$1"; else printf absent; fi; }
printf 'ARG %s\n' "$@" >"$AGY_CAPTURE"
printf 'HOME=%s\n' "$HOME" >>"$AGY_CAPTURE"
printf 'KEY=%s\n' "${GEMINI_API_KEY:-<unset>}" >>"$AGY_CAPTURE"
printf 'UPDATER=%s\n' "${AGY_CLI_DISABLE_AUTO_UPDATE:-<unset>}" >>"$AGY_CAPTURE"
printf 'SETTINGS=%s\n' "$(flat "$HOME/settings.json")" >>"$AGY_CAPTURE"
printf 'DEFAULT_SETTINGS=%s\n' "$(flat "$HOME/.gemini/antigravity-cli/settings.json")" >>"$AGY_CAPTURE"
"#,
            )
            .unwrap();
            std::fs::set_permissions(
                bin.path().join("agy"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            Self { root, bin, capture }
        }

        /// Register one enabled `gemini` pool account — the pilot's seat.
        fn register_pool(&self, name: &str) {
            let dir = self.root.path().join(".loom/api-keys/gemini");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(format!("{name}.env")), format!("GEMINI_API_KEY={POOL_KEY}\n"))
                .unwrap();
            std::fs::set_permissions(
                dir.join(format!("{name}.env")),
                std::fs::Permissions::from_mode(0o600),
            )
            .unwrap();
        }

        fn worker(&self) -> Command {
            let mut c = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
            c.args(["spawn-worker", "--"])
                .current_dir(self.root.path())
                .env("LOOM_WORKSPACE", self.root.path())
                .env("LOOM_RUNTIME", "agy")
                .env_remove("LOOM_ROLE")
                .env_remove("LOOM_MODEL")
                .env_remove("LOOM_MODEL_PROFILE")
                .env("LOOM_CONFIG_DEFAULTS_FILE", "")
                .env("LOOM_SHARED_API_KEYS_DIR", "")
                .env("AGY_CAPTURE", &self.capture)
                .env_remove("GEMINI_API_KEY")
                // The updater kill switch is the adapter's to default: what
                // the dispatch host exported must not answer for it.
                .env_remove("AGY_CLI_DISABLE_AUTO_UPDATE")
                .env_remove("LOOM_GENERIC_EXTRA_ARGS")
                // spawn-generic-launch.sh resolves a daemon binary, and the
                // dispatch environment this suite may run under exports its
                // own pins (LOOM_DAEMON_SELF_BIN names the HOST daemon and
                // outranks LOOM_DAEMON_BIN in locate-daemon-bin.sh; the
                // preference marker rides the worker log). Neither may leak
                // into what these tests observe.
                .env_remove("LOOM_DAEMON_SELF_BIN")
                .env_remove("LOOM_RUNTIME_PREFERENCE_MARKER")
                .env("LOOM_DAEMON_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
                .env(
                    "PATH",
                    format!("{}:{}", self.bin.path().display(), std::env::var("PATH").unwrap()),
                );
            c
        }

        fn capture(&self) -> String {
            std::fs::read_to_string(&self.capture).unwrap_or_default()
        }

        fn args(&self) -> Vec<String> {
            self.capture()
                .lines()
                .filter_map(|l| l.strip_prefix("ARG ").map(str::to_string))
                .collect()
        }

        fn fact(&self, prefix: &str) -> String {
            self.capture()
                .lines()
                .find(|l| l.starts_with(prefix))
                .unwrap_or_default()
                .trim_start_matches(prefix)
                .to_string()
        }

        /// The settings the CLI child found at the per-launch path the
        /// `--settings` argument names.
        fn policy(&self) -> serde_json::Value {
            let raw = self.fact("SETTINGS=");
            serde_json::from_str(&raw)
                .unwrap_or_else(|e| panic!("settings are not JSON ({e}): {raw}"))
        }

        /// Everything a launch must carry whatever the operator overrode:
        /// the guarded argv, the policy in both settings locations, and the
        /// updater kill switch the child saw (`updater`).
        fn assert_launches_with_pinned_policy(&self, args: &[String], updater: &str) {
            // The guarded policy leads the argv: the per-launch settings
            // file, then containment.
            assert_eq!(args[0], "--settings", "{args:?}");
            let settings = PathBuf::from(&args[1]);
            assert!(settings.is_file(), "settings file missing: {settings:?}");
            assert_eq!(
                settings,
                PathBuf::from(self.fact("HOME=")).join("settings.json"),
                "--settings must name the file under the isolated HOME"
            );
            assert_eq!(args[2], "--sandbox", "{args:?}");
            assert_eq!(args.iter().filter(|a| *a == "--sandbox").count(), 1, "{args:?}");
            assert_eq!(self.policy(), expected_policy(&self.workspace()));
            // The same policy sits at the CLI's documented default settings
            // location, so it holds whether or not the binary honours
            // `--settings`.
            assert_eq!(self.fact("DEFAULT_SETTINGS="), self.fact("SETTINGS="));
            assert_eq!(self.fact("UPDATER="), updater);
        }

        /// The physical workspace path: what the adapter's shell reports as
        /// its cwd is what the CLI takes as its workspace root.
        fn workspace(&self) -> PathBuf {
            self.root.path().canonicalize().unwrap()
        }
    }

    /// The read-only policy spawn-agy.sh pins (#15474): the gemini provider,
    /// the default execution mode, and explicit rules that hold in every
    /// mode — the workspace readable through the file-read tool, file
    /// writes, shell commands and MCP tools denied.
    fn expected_policy(workspace: &Path) -> serde_json::Value {
        serde_json::json!({
            "modelProvider": "gemini",
            "agentMode": "",
            "toolPermission": "request-review",
            "permissions": {
                "allow": [format!("read_file({})", workspace.display())],
                "deny": ["write_file(*)", "command(*)", "mcp(*)"],
            }
        })
    }

    /// Happy path: prompt once, the configured model once, the guarded
    /// launch flags carrying the read-only permission policy, an isolated
    /// HOME holding that policy, the self-update kill switch defaulted on,
    /// and the POOL's key in the child environment only — an ambient
    /// GEMINI_API_KEY neither wins nor rides along (#14945 criteria 1-2,
    /// #15474).
    #[test]
    fn pooled_agy_launch_passes_prompt_model_and_flags_with_child_only_key() {
        let w = AgyWorkspace::new();
        w.register_pool("alpha");
        let out = w
            .worker()
            .env("GEMINI_API_KEY", AMBIENT_KEY)
            .args(["--dangerously-skip-permissions", "-p", "hello"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stderr}");
        // Prompt once, model once, guarded flags from the stub, in the
        // tier-3 template's documented order (extra args, model, prompt).
        // The Loom-only permission flag is consumed by the template, never
        // forwarded to the CLI.
        let args = w.args();
        assert_eq!(args.len(), 7, "{args:?}");
        assert!(!args.iter().any(|a| a.contains("dangerously")), "{args:?}");
        assert_eq!(args[3], "--model");
        assert_eq!(args[4], "gemini-4-neon");
        assert_eq!(args[5], "-p");
        assert_eq!(args[6], "hello");
        w.assert_launches_with_pinned_policy(&args, "true");
        // HOME was redirected outside every checkout.
        let home = w.fact("HOME=");
        assert!(!home.is_empty());
        assert!(Path::new(&home).starts_with(std::env::temp_dir()), "{home}");
        assert!(!Path::new(&home).starts_with(w.root.path()), "{home}");
        // Only the pool's key reached the child; the ambient one lost.
        assert_eq!(w.fact("KEY="), POOL_KEY);
        // The launch record names the account, never the key.
        assert_eq!(launch_field(&stderr, "credentialSource"), "pool");
        assert_eq!(launch_field(&stderr, "credentialProvider"), "gemini");
        assert_eq!(launch_field(&stderr, "credentialAccount"), "alpha");
        assert_eq!(launch_field(&stderr, "model"), "gemini-4-neon");
        for haystack in [&stderr, &stdout] {
            assert!(!haystack.contains(POOL_KEY), "key material leaked: {haystack}");
            assert!(!haystack.contains(AMBIENT_KEY), "ambient key leaked: {haystack}");
        }
        // The adapter chain's own stdout is a bare newline at most; the
        // point is that no key material has a stdout route.
        assert!(stdout.trim().is_empty(), "{stdout}");
    }

    /// LOOM_MODEL overrides the manifest default once (env > manifest), and
    /// is what the pool's model-class read sees.
    #[test]
    fn loom_model_overrides_the_manifest_default_once() {
        let w = AgyWorkspace::new();
        w.register_pool("alpha");
        let out = w
            .worker()
            .env("LOOM_MODEL", "gemini-4-neon-high")
            .args(["-p", "hello"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        let args = w.args();
        let model = args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(args[model + 1], "gemini-4-neon-high");
        assert_eq!(args.iter().filter(|a| *a == "--model").count(), 1, "{args:?}");
        assert_eq!(launch_field(&stderr, "model"), "gemini-4-neon-high");
    }

    /// Criterion 2's hard half: a missing pool refuses at 78 BEFORE the CLI
    /// runs — an ambient key (or, per the adapter's isolated HOME, a cached
    /// login) cannot substitute for the pilot's spend control.
    #[test]
    fn a_missing_pool_refuses_before_exec_even_with_an_ambient_key() {
        let w = AgyWorkspace::new();
        let out = w
            .worker()
            .env("GEMINI_API_KEY", AMBIENT_KEY)
            .args(["-p", "hello"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        assert!(
            stderr.contains("requires a usable account from the gemini API-key pool"),
            "{stderr}"
        );
        assert!(stderr.contains("api-keys add gemini"), "{stderr}");
        assert!(!stderr.contains(AMBIENT_KEY), "{stderr}");
        assert!(out.stdout.is_empty());
        // The CLI never ran.
        assert_eq!(w.capture(), "");
    }

    /// A disabled pool is the same refusal, without key material.
    #[test]
    fn an_all_disabled_pool_refuses_at_78_without_key_material() {
        let w = AgyWorkspace::new();
        w.register_pool("alpha");
        std::fs::write(w.root.path().join(".loom/api-keys/gemini/.disabled"), "alpha\n").unwrap();
        let out = w.worker().args(["-p", "hello"]).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        assert!(stderr.contains("disabled by operator"), "{stderr}");
        assert!(!stderr.contains(POOL_KEY), "{stderr}");
        assert_eq!(w.capture(), "");
    }

    /// Criterion 3's direct-launch half: at the account's declared cap the
    /// launch refuses before exec; the slot is the same one a live OpenCode
    /// draw would hold (shared `gemini` namespace).
    #[test]
    fn a_direct_launch_at_the_account_cap_refuses_before_exec() {
        let w = AgyWorkspace::new();
        w.register_pool("alpha");
        std::fs::write(
            w.root.path().join(".loom/api-keys/gemini/.limits.json"),
            r#"{"alpha": {"maxConcurrent": 1}}"#,
        )
        .unwrap();
        // One live lease, held by this test process (a live PID): the
        // in-flight shape `spawn-worker`'s own selection would register.
        let inflight = w.root.path().join(".loom/api-keys/gemini/.inflight/alpha");
        std::fs::create_dir_all(&inflight).unwrap();
        let holder = serde_json::json!({
            "pid": std::process::id(),
            "startedAt": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        })
        .to_string();
        std::fs::write(inflight.join(format!("{}-lease.json", std::process::id())), holder)
            .unwrap();
        let out = w.worker().args(["-p", "hello"]).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        assert!(stderr.contains("concurrency cap"), "{stderr}");
        assert!(!stderr.contains(POOL_KEY), "{stderr}");
        assert_eq!(w.capture(), "");
    }

    /// An operator's LOOM_GENERIC_EXTRA_ARGS still rides along (the env >
    /// manifest tier) but no longer REPLACES the guarded argv: the override
    /// that used to launch with `--sandbox` alone keeps the settings file
    /// that carries the permission policy.
    #[test]
    fn a_sandbox_only_override_keeps_the_permission_policy() {
        let w = AgyWorkspace::new();
        w.register_pool("alpha");
        let out = w
            .worker()
            .env("LOOM_GENERIC_EXTRA_ARGS", "--sandbox")
            .args(["-p", "hello"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        let args = w.args();
        assert_eq!(args.len(), 7, "{args:?}");
        w.assert_launches_with_pinned_policy(&args, "true");
        assert_eq!(args[3], "--model");
        assert_eq!(args[4], "gemini-4-neon");
        assert_eq!(args[5], "-p");
        assert_eq!(args[6], "hello");
    }

    /// Extra arguments that do not touch the policy survive it, ahead of the
    /// model and the prompt.
    #[test]
    fn compatible_extra_args_survive_after_the_guarded_policy() {
        let w = AgyWorkspace::new();
        w.register_pool("alpha");
        let out = w
            .worker()
            .env("LOOM_GENERIC_EXTRA_ARGS", "--output-format json --print-timeout=5m")
            .args(["-p", "hello"])
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        let args = w.args();
        w.assert_launches_with_pinned_policy(&args, "true");
        let settings = PathBuf::from(w.fact("HOME=")).join("settings.json");
        assert_eq!(
            args,
            [
                "--settings",
                &settings.display().to_string(),
                "--sandbox",
                "--output-format",
                "json",
                "--print-timeout=5m",
                "--model",
                "gemini-4-neon",
                "-p",
                "hello",
            ]
            .map(String::from)
        );
    }

    /// A refused launch (#15474): exit 78 with the offending argument and a
    /// remediation on stderr, no key material, the CLI never run, and no
    /// per-launch HOME left behind — the policy is checked before anything
    /// is created for the run.
    fn assert_refused(out: &std::process::Output, scratch: &Path, needles: &[&str]) {
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "{stderr}");
        for needle in needles {
            assert!(stderr.contains(needle), "missing {needle:?} in: {stderr}");
        }
        assert!(stderr.contains("REMEDIATION"), "{stderr}");
        assert!(!stderr.contains(POOL_KEY), "key material leaked: {stderr}");
        assert!(out.stdout.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
        // A private TMPDIR is where the stub would have created the HOME.
        let homes = std::fs::read_dir(scratch)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("loom-agy-"))
            .count();
        assert_eq!(homes, 0, "a refused launch created a per-launch HOME");
    }

    /// An LOOM_GENERIC_EXTRA_ARGS override that drops or contradicts the
    /// policy refuses before the task runs; the stub does not try to layer
    /// its policy around a foreign settings file or a mode it did not pin.
    #[test]
    fn an_extra_args_override_that_drops_or_contradicts_the_policy_refuses_before_exec() {
        for (extra, offending) in [
            ("--dangerously-skip-permissions", "--dangerously-skip-permissions"),
            ("--mode auto", "--mode"),
            ("--mode=plan", "--mode=plan"),
            ("--settings /elsewhere/settings.json", "--settings"),
            ("--settings=/elsewhere/settings.json", "--settings=/elsewhere/settings.json"),
            ("--sandbox=false", "--sandbox=false"),
            (
                "--output-format json --dangerously-skip-permissions",
                "--dangerously-skip-permissions",
            ),
        ] {
            let w = AgyWorkspace::new();
            w.register_pool("alpha");
            let scratch = tempfile::tempdir().unwrap();
            let out = w
                .worker()
                .env("TMPDIR", scratch.path())
                .env("LOOM_GENERIC_EXTRA_ARGS", extra)
                .args(["-p", "hello"])
                .output()
                .unwrap();
            assert_refused(
                &out,
                scratch.path(),
                &["spawn-agy", "LOOM_GENERIC_EXTRA_ARGS", offending],
            );
        }
    }

    /// The worker's own argv reaches the CLI as verbatim as extra arguments
    /// do, so it meets the same check. The one exception is the bare
    /// `--dangerously-skip-permissions`, which the tier-3 template consumes
    /// (the happy-path test above launches with it); with a value attached
    /// the template would forward it.
    #[test]
    fn a_worker_argument_that_drops_or_contradicts_the_policy_refuses_before_exec() {
        for (args, offending) in [
            (vec!["--mode", "auto", "-p", "hello"], "--mode"),
            (vec!["--settings", "/elsewhere/settings.json", "-p", "hello"], "--settings"),
            (vec!["--sandbox=false", "-p", "hello"], "--sandbox=false"),
            (
                vec!["--dangerously-skip-permissions=true", "-p", "hello"],
                "--dangerously-skip-permissions=true",
            ),
        ] {
            let w = AgyWorkspace::new();
            w.register_pool("alpha");
            let scratch = tempfile::tempdir().unwrap();
            let out = w
                .worker()
                .env("TMPDIR", scratch.path())
                .args(&args)
                .output()
                .unwrap();
            assert_refused(&out, scratch.path(), &["spawn-agy", offending]);
        }
    }

    /// The self-update kill switch is the adapter's default, and an
    /// operator-exported value still reaches the child unchanged (#15474).
    /// Only a non-empty export is the operator's: an exported empty string
    /// would reach the CLI as "not disabled".
    #[test]
    fn an_operator_exported_updater_setting_wins_over_the_default() {
        for (exported, expected) in [("false", "false"), ("", "true")] {
            let w = AgyWorkspace::new();
            w.register_pool("alpha");
            let out = w
                .worker()
                .env("AGY_CLI_DISABLE_AUTO_UPDATE", exported)
                .args(["-p", "hello"])
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{exported:?}: {stderr}");
            assert_eq!(w.fact("UPDATER="), expected, "{exported:?}");
            w.assert_launches_with_pinned_policy(&w.args(), expected);
        }
    }

    /// The read rule names the workspace, so a path carrying a JSON
    /// metacharacter must still produce the policy the CLI can parse.
    #[test]
    fn a_workspace_path_is_json_escaped_in_the_read_rule() {
        let w = AgyWorkspace::in_workspace("loom\"agy\\ws");
        w.register_pool("alpha");
        let out = w.worker().args(["-p", "hello"]).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "{stderr}");
        w.assert_launches_with_pinned_policy(&w.args(), "true");
    }

    /// A workspace path that cannot be written into a rule at all is a
    /// launch that cannot express the policy.
    #[test]
    fn a_workspace_path_with_a_control_character_refuses_before_exec() {
        let w = AgyWorkspace::in_workspace("loom\tagy");
        w.register_pool("alpha");
        let scratch = tempfile::tempdir().unwrap();
        let out = w
            .worker()
            .env("TMPDIR", scratch.path())
            .args(["-p", "hello"])
            .output()
            .unwrap();
        assert_refused(&out, scratch.path(), &["spawn-agy", "workspace", "control character"]);
    }
}
