//! #14945: a tier-3 tap whose manifest declares `credentialPool` (`agy`, the
//! Antigravity pilot) composes the binary gate with the shared gemini
//! API-key pool — the same `api_keys:gemini` source a native tap on the same
//! namespace reports, so one account cap governs both routes and a role
//! draw's `hold_pool_slot` reserves either.
//!
//! Hermetic like `binary_tests.rs`: "installed" is a [`BinaryEnv`] handed to
//! the read, the pool lives in the fixture's `.loom/api-keys/`, and the
//! manifest is the real shipped `defaults/runtimes/agy.json` the fixture
//! copies into `.loom/runtimes/`.
use super::availability::{availability_in, Availability, BinaryEnv, CredentialSource};
use super::resolve::Tap;
use super::tests::{fixture, ClearedRuntimeEnv};
use crate::role_runner::PoolHold;
use crate::runtime_admission::{resolve_and_admit, ResolvedRuntime};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// A role with no `runtimeRequirements` (Curator-class): every runtime is
/// admitted and only availability decides.
const ROLE: &str = "guide";

/// [`fixture`] plus the agy adapter stub it does not ship.
fn workspace() -> tempfile::TempDir {
    let dir = fixture();
    let adapter = dir.path().join(".loom/scripts/spawn-agy.sh");
    fs::write(&adapter, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

fn agy_tap(root: &Path, env: &BinaryEnv) -> Availability {
    let admitted = resolve_and_admit(root, ROLE, Some("agy")).unwrap();
    availability_in(root, &Tap::runtime("agy"), &admitted, 0, env)
}

fn bin_holding(name: &str) -> (tempfile::TempDir, BinaryEnv) {
    let dir = tempfile::tempdir().unwrap();
    let stub = dir.path().join(name);
    fs::write(&stub, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    let env = BinaryEnv {
        cli_bin: None,
        path: Some(dir.path().as_os_str().to_owned()),
    };
    (dir, env)
}

fn register_gemini_seat(root: &Path, name: &str) {
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(root);
    fs::create_dir_all(pool.join("gemini")).unwrap();
    fs::write(
        pool.join("gemini").join(format!("{name}.env")),
        "GEMINI_API_KEY=fake-gemini-pool-key-never-logged\n",
    )
    .unwrap();
}

/// With a registered seat and the CLI installed, the tap is spawnable under
/// the shared `api_keys:gemini` source — the wire name `hold_pool_slot`
/// reserves and a native tap on the same namespace reports.
#[test]
#[serial_test::serial]
fn a_pooled_tier3_tap_reports_the_shared_gemini_pool_source() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    register_gemini_seat(dir.path(), "pilot");
    let (_bin, env) = bin_holding("agy");
    let state = agy_tap(dir.path(), &env);
    assert!(
        matches!(
            &state,
            Availability::Spawnable {
                source: CredentialSource::ApiKeys { provider },
                spawnable: Some(1),
                total: Some(1),
            } if provider == "gemini"
        ),
        "{state:?}"
    );
    assert_eq!(state.source().wire(), "api_keys:gemini");
}

/// Criterion 2's availability half: no registered pool is Unprovisioned —
/// and an ambient GEMINI_API_KEY does NOT rescue it, because the launch
/// path's pool is mandatory (no environment escape, unlike the native
/// ladder's step 1).
#[test]
#[serial_test::serial]
fn a_missing_pool_is_unprovisioned_even_with_an_ambient_key() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    let (_bin, env) = bin_holding("agy");
    std::env::set_var("GEMINI_API_KEY", "fake-ambient-gemini-key-never-logged");
    let state = agy_tap(dir.path(), &env);
    std::env::remove_var("GEMINI_API_KEY");
    let Availability::Exhausted {
        source,
        hold,
        detail,
    } = &state
    else {
        panic!("a missing pool must not be selectable: {state:?}");
    };
    assert_eq!(*hold, PoolHold::Unprovisioned);
    assert_eq!(
        source,
        &CredentialSource::ApiKeys {
            provider: "gemini".into()
        }
    );
    assert!(detail.contains("api-keys add gemini"), "{detail}");
    let summary = state.skip_reason().unwrap().summary();
    assert!(summary.starts_with("api_keys:gemini: "), "{summary}");
}

/// The composition order: an uninstalled CLI is unavailable whatever the
/// pool says, and the hold names the install step (#9368's binary gate
/// stays authoritative ahead of the pool).
#[test]
#[serial_test::serial]
fn the_binary_gate_is_asked_ahead_of_the_pool() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    register_gemini_seat(dir.path(), "pilot");
    let (_bin, env) = bin_holding("something-else");
    let state = agy_tap(dir.path(), &env);
    let Availability::Exhausted {
        source,
        hold,
        detail,
    } = &state
    else {
        panic!("an uninstalled CLI must not be selectable: {state:?}");
    };
    assert_eq!(*source, CredentialSource::Binary);
    assert_eq!(*hold, PoolHold::Unprovisioned);
    assert!(detail.contains("'agy' not found in PATH"), "{detail}");
}

/// Criterion 3's sharing half: a live lease on the gemini account (one
/// OpenCode draw holding the only `maxConcurrent` slot) makes BOTH routes
/// unavailable with the same skip source — the cap is per account, not per
/// runtime.
#[test]
#[serial_test::serial]
fn one_live_lease_on_the_shared_account_caps_both_routes() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    register_gemini_seat(dir.path(), "pilot");
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::limits::set_max_concurrent(&pool, "gemini", "pilot", Some(1)).unwrap();
    // One live spawn, held by this (live) test process.
    crate::api_keys_pool::inflight::register_for(&pool, "gemini", "pilot", std::process::id())
        .unwrap();

    let (_bin, env) = bin_holding("agy");
    let state = agy_tap(dir.path(), &env);
    let Availability::Exhausted { hold, detail, .. } = &state else {
        panic!("a capped shared account must not be selectable: {state:?}");
    };
    assert_eq!(*hold, PoolHold::SelfHealing);
    assert!(detail.contains("at cap"), "{detail}");

    // The native route on the same namespace: a quick-flash-shaped profile
    // whose credentialEnv is unset, so the native ladder consults the pool.
    super::tests::write_config(
        dir.path(),
        &serde_json::json!({"runtimes":{"modelProfiles":{"gemini-flash":{
            "model": "gemini-4-neon",
            "providers": {"opencode": "google"},
            "credentialEnv": "GEMINI_API_KEY",
            "credentialPool": "gemini"
        }}}}),
    );
    let admitted: Result<ResolvedRuntime, _> =
        resolve_and_admit(dir.path(), ROLE, Some("opencode"));
    let admitted = admitted.unwrap();
    let tap = Tap::with_profile("opencode", "gemini-flash");
    let native = availability_in(dir.path(), &tap, &admitted, 0, &env);
    let Availability::Exhausted { hold, detail, .. } = &native else {
        panic!("the native route must share the cap: {native:?}");
    };
    assert_eq!(*hold, PoolHold::SelfHealing);
    assert!(detail.contains("at cap"), "{detail}");
    assert_eq!(state.source().wire(), native.source().wire());
}

/// A non-pooled tier-3 runtime keeps the binary-only gate: `gemini`'s
/// manifest declares no credentialPool, so its tap stays Unobservable with
/// the CLI installed — the pre-#14945 behaviour, unchanged.
#[test]
#[serial_test::serial]
fn a_non_pooled_tier3_runtime_keeps_the_binary_only_gate() {
    let _env = ClearedRuntimeEnv::new();
    let dir = workspace();
    // `gemini` needs its own adapter stub to admit at all.
    let gemini_adapter = dir.path().join(".loom/scripts/spawn-gemini.sh");
    fs::write(&gemini_adapter, "#!/bin/sh\n").unwrap();
    fs::set_permissions(&gemini_adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (_bin, env) = bin_holding("gemini");
    let admitted = resolve_and_admit(dir.path(), ROLE, Some("gemini")).unwrap();
    let state = availability_in(dir.path(), &Tap::runtime("gemini"), &admitted, 0, &env);
    assert!(state.is_spawnable(), "{state:?}");
    assert_eq!(state.source(), &CredentialSource::Unobservable);
}
