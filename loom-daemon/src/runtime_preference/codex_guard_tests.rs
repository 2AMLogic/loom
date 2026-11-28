//! Guard-hook readiness as Codex availability for merging roles (#9390).
use super::*;
use crate::runtime_preference::availability::{availability, Availability, CredentialSource};
use crate::runtime_preference::resolve::Tap;
use crate::tokens_pool::codex_hooks::test_support::{guard_ready, sealed_session_seat};
use std::fs;
use std::path::PathBuf;

const ENV: [&str; 6] = [
    "LOOM_CODEX_PROFILE_ROOT",
    "LOOM_CODEX_PROFILE",
    "LOOM_CODEX_HOME",
    "CODEX_HOME",
    "LOOM_SPAWN_NO_EXPORT",
    "LOOM_CODEX_NO_EXEC",
];

/// A workspace laid out like an installed repo (real role/runtime manifests,
/// an installed bridge, an executable `spawn-codex.sh` stub) with four Codex
/// accounts: `ready`, `untrusted`, `missing` (no hook installed), and
/// `private` (a private-clone session's profile, never judged here).
struct Fixture {
    root: tempfile::TempDir,
    profiles: tempfile::TempDir,
    _shared: tempfile::TempDir,
    prior: Vec<(&'static str, Option<std::ffi::OsString>)>,
    /// Crate-wide `LOOM_CODEX_PROFILE_ROOT` lock (#9964); released after `Drop`.
    _profile_root_lock: crate::tokens_pool::profile_root_env::ProfileRootLock,
    /// Crate-wide `LOOM_CODEX_HOME` lock (#15174); released after `Drop` and
    /// after the profile-root lock above (acquired before it — see
    /// `codex_home_env`'s documented lock order).
    _codex_home_lock: crate::tokens_pool::codex_home_env::CodexHomeLock,
}

/// A private-clone profile holding the image-owned bridge registration
/// exactly as a provisioned seat's controls shape it: no recorded trust, a
/// sealable registration (#15397), so the seat is `sealed` and stays
/// eligible for the guarded draw.
fn private_registration(profile: &Path) {
    let command = crate::tokens_pool::private_workspace::bundle::registration();
    fs::create_dir_all(profile).unwrap();
    fs::write(
        profile.join("hooks.json"),
        serde_json::json!({"hooks": {"PreToolUse": [{"matcher": "*", "hooks": [
            {"type": "command", "command": command, "timeout": 30}
        ]}]}})
        .to_string(),
    )
    .unwrap();
    fs::write(
        profile.join("loom-codex-hooks.json"),
        serde_json::json!({"loomManagedHook": {
            "command": command,
            "commandSha256": sha256_hex(command.as_bytes()),
            "trustBaselineHashes": []
        }})
        .to_string(),
    )
    .unwrap();
    fs::write(profile.join("config.toml"), "").unwrap();
}

/// A private-clone profile with neither recorded nor sealable trust: an
/// extra hook event the seal refuses, so its trust state is `missing` and
/// the guarded draw excludes it (#15397).
fn unseal_private(profile: &Path) {
    let path = profile.join("hooks.json");
    let mut hooks: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    hooks["hooks"]["PostToolUse"] = serde_json::json!([]);
    fs::write(path, hooks.to_string()).unwrap();
}

/// The key Codex records trust for Loom's entry under in a private session.
const SESSION_KEY: &str = "/home/loom/.codex-profile/hooks.json:pre_tool_use:0:0";

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

impl Fixture {
    fn new() -> Self {
        let codex_home_lock = crate::tokens_pool::codex_home_env::lock();
        let profile_root_lock = crate::tokens_pool::profile_root_env::lock();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let root = tempfile::tempdir().unwrap();
        let profiles = tempfile::tempdir().unwrap();
        let shared = tempfile::tempdir().unwrap();
        let loom = root.path().join(".loom");
        for (sub, from) in [
            ("roles", "defaults/roles"),
            ("runtimes", "defaults/runtimes"),
        ] {
            fs::create_dir_all(loom.join(sub)).unwrap();
            for entry in fs::read_dir(repo.join(from)).unwrap().flatten() {
                fs::copy(entry.path(), loom.join(sub).join(entry.file_name())).unwrap();
            }
        }
        fs::create_dir_all(loom.join("scripts")).unwrap();
        fs::create_dir_all(loom.join("hooks")).unwrap();
        fs::write(loom.join("hooks/guard-codex-bridge.sh"), "#!/bin/sh\n").unwrap();
        let adapter = loom.join("scripts/spawn-codex.sh");
        fs::write(&adapter, "#!/bin/sh\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&adapter, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let names = ["ready", "untrusted", "missing", "private"];
        for name in names {
            fs::create_dir_all(profiles.path().join(name)).unwrap();
        }
        let private_state = profiles.path().join(".private-sessions/private");
        fs::create_dir_all(&private_state).unwrap();
        fs::write(private_state.join("workspace.json"), "{}").unwrap();
        // #15397: a sealed private seat, eligible for the guarded draw; the
        // deeper proof is the container's, not this module's.
        private_registration(&profiles.path().join("private"));
        let accounts = names
            .iter()
            .map(|name| {
                format!(
                    r#"{{"provider":"codex","name":"{name}","credential_kind":"codex_home","credential_reference":"{name}","enabled":true}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        fs::write(
            loom.join("accounts.json"),
            format!(r#"{{"version":1,"accounts":[{accounts}]}}"#),
        )
        .unwrap();
        let shared_env = crate::tokens_pool::paths::SHARED_ACCOUNTS_ROOT_ENV;
        let prior = ENV
            .iter()
            .chain([&shared_env])
            .map(|key| (*key, std::env::var_os(key)))
            .collect();
        for key in ENV {
            std::env::remove_var(key);
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", profiles.path());
        std::env::set_var(shared_env, shared.path());
        let fixture = Self {
            root,
            profiles,
            _shared: shared,
            prior,
            _profile_root_lock: profile_root_lock,
            _codex_home_lock: codex_home_lock,
        };
        guard_ready(&fixture.profile("ready"), None);
        // Installed, but its only trust was taken somewhere Codex won't look.
        guard_ready(&fixture.profile("untrusted"), Some(Path::new("/elsewhere")));
        fixture
    }

    fn profile(&self, name: &str) -> PathBuf {
        self.profiles.path().join(name)
    }

    fn codex_for(&self, role: &str) -> Availability {
        let admitted =
            crate::runtime_admission::resolve_and_admit(self.root.path(), role, Some("codex"))
                .unwrap();
        availability(self.root.path(), &Tap::runtime("codex"), &admitted, 0)
    }

    fn ready_everything(&self) {
        guard_ready(&self.profile("missing"), None);
        guard_ready(&self.profile("untrusted"), None);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for (key, value) in self.prior.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

#[test]
fn mutable_and_merging_roles_are_guarded() {
    for role in ["builder", "doctor", "sweep-lifecycle", "champion", "judge"] {
        assert!(guarded(role), "{role}");
    }
    for role in ["curator", "hermit", "guide", "auditor", "architect"] {
        assert!(!guarded(role), "{role}");
    }
}

#[test]
#[serial_test::serial]
fn shared_profiles_are_checked_individually_and_private_ones_only_for_missing_trust() {
    let f = Fixture::new();
    let mut unready = unready_profiles(f.root.path()).unwrap();
    unready.sort();
    assert_eq!(
        unready,
        ["missing", "untrusted"],
        "a sealed private seat is proven in-container, not judged here"
    );
    // #15397: a private profile with `missing` trust can serve no guarded
    // role — the draw excludes it before any Docker call, exactly as it
    // excludes an unprovisioned shared profile.
    unseal_private(&f.profile("private"));
    let mut unready = unready_profiles(f.root.path()).unwrap();
    unready.sort();
    assert_eq!(unready, ["missing", "private", "untrusted"]);
    f.ready_everything();
    private_registration(&f.profile("private"));
    assert_eq!(unready_profiles(f.root.path()).unwrap(), Vec::<String>::new());
    // Only the stale profile loses readiness.
    fs::write(f.profile("missing").join("hooks.json"), "{}").unwrap();
    assert_eq!(unready_profiles(f.root.path()).unwrap(), ["missing"]);
    // ...and so does a checkout with no bridge for the entry to run.
    f.ready_everything();
    unseal_private(&f.profile("private"));
    fs::remove_file(f.root.path().join(".loom/hooks/guard-codex-bridge.sh")).unwrap();
    let mut unready = unready_profiles(f.root.path()).unwrap();
    unready.sort();
    // The shared profiles need the checkout's bridge; the private one does
    // not (its bridge is the image's), but it stays `missing`.
    assert_eq!(unready.len(), 4);
}

#[test]
#[serial_test::serial]
fn guarded_roles_can_use_a_mixed_pool_and_refuse_an_entirely_untrusted_pool() {
    let f = Fixture::new();
    fs::remove_dir_all(f.profile("private")).unwrap();
    for role in ["champion", "judge"] {
        assert!(f.codex_for(role).is_spawnable(), "{role}");
    }
    for role in ["builder", "doctor", "champion", "judge"] {
        let selected = select(f.root.path(), None, Some(role), |_| true).unwrap();
        assert_eq!(selected.id.name, "ready", "{role}");
    }
    let step = crate::tokens_pool::private_workspace::dispatch::Selection::guarded_shared_step(
        f.root.path(),
        "judge",
        None,
    )
    .unwrap();
    assert_eq!(step.name, "ready");
    fs::remove_file(f.profile("ready").join("hooks.json")).unwrap();
    for role in ["champion", "judge"] {
        match f.codex_for(role) {
            Availability::Exhausted { source, detail, .. } => {
                assert_eq!(source, CredentialSource::CodexAccounts);
                for name in ["missing", "untrusted", "ready"] {
                    assert!(detail.contains(name), "{detail}");
                }
            }
            other => panic!("{role}: expected Exhausted, got {other:?}"),
        }
    }
    assert!(select(f.root.path(), None, Some("judge"), |_| true).is_err());
    assert!(select(f.root.path(), None, Some("curator"), |_| true).is_ok());
    assert!(f.codex_for("curator").is_spawnable());
}

#[test]
#[serial_test::serial]
fn healthy_untrusted_profiles_cannot_substitute_for_a_held_ready_seat() {
    use crate::tokens_pool::{AccountId, AccountProvider, TerminalClassification};
    let f = Fixture::new();
    fs::remove_dir_all(f.profile("private")).unwrap();
    crate::tokens_pool::health::record_terminal_at(
        f.root.path(),
        &AccountId {
            provider: AccountProvider::Codex,
            name: "ready".into(),
        },
        TerminalClassification::TokenExpired,
        "test",
        0,
    )
    .unwrap();
    assert!(!f.codex_for("judge").is_spawnable());
    assert!(select(f.root.path(), None, Some("judge"), |_| true).is_err());
    assert!(select(f.root.path(), None, Some("curator"), |_| true).is_ok());
}

#[test]
#[serial_test::serial]
fn unreadable_inventory_refuses_the_guarded_tap_and_draw() {
    let f = Fixture::new();
    fs::write(f.root.path().join(".loom/accounts.json"), "{").unwrap();
    assert!(!f.codex_for("judge").is_spawnable());
    assert!(select(f.root.path(), None, Some("judge"), |_| true).is_err());
}

/// #10102: a session seat whose registration is sealed counts as guarded
/// without any recorded trust (spawn-codex passes the waiver for it), and a
/// tampered one does not.
#[test]
#[serial_test::serial]
fn a_sealed_session_seat_is_guarded_without_recorded_trust() {
    let f = Fixture::new();
    sealed_session_seat(&f.profile("missing"));
    sealed_session_seat(&f.profile("untrusted"));
    assert_eq!(unready_profiles(f.root.path()).unwrap(), Vec::<String>::new());
    assert!(f.codex_for("judge").is_spawnable());
    // An extra hook beside Loom's: no seal, no recorded trust, not guarded.
    let hooks = f.profile("missing").join("hooks.json");
    let mut value: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&hooks).unwrap()).unwrap();
    value["hooks"]["PostToolUse"] = serde_json::json!([{"hooks": [
        {"type": "command", "command": "true", "timeout": 30}
    ]}]);
    fs::write(&hooks, value.to_string()).unwrap();
    assert_eq!(unready_profiles(f.root.path()).unwrap(), ["missing"]);
}

/// #15397: a private-clone profile with `missing` trust is excluded from the
/// guarded draw (`ready`, `unready_profiles`, `select`) before any Docker
/// call, while `sealed` and `recorded` private seats stay eligible.
#[test]
#[serial_test::serial]
fn a_missing_trust_private_clone_profile_is_excluded_from_the_guarded_draw() {
    let f = Fixture::new();

    // `sealed` stays eligible, and can even win the draw when no shared
    // profile is ready.
    let selected = select(f.root.path(), None, Some("judge"), |_| true).unwrap();
    assert!(
        selected.id.name == "ready" || selected.id.name == "private",
        "a sealed private seat is eligible alongside a ready shared one"
    );
    for profile in [
        f.profile("ready"),
        f.profile("untrusted"),
        f.profile("missing"),
    ] {
        let hooks = profile.join("hooks.json");
        if hooks.exists() {
            fs::remove_file(hooks).unwrap();
        }
    }
    let selected = select(f.root.path(), None, Some("judge"), |_| true).unwrap();
    assert_eq!(selected.id.name, "private", "a sealed private seat may serve a guarded role");

    // `recorded` (a trusted_hash at the session key) stays eligible too.
    fs::write(
        f.profile("private").join("config.toml"),
        format!("[hooks.state.\"{SESSION_KEY}\"]\ntrusted_hash = \"fresh\"\n"),
    )
    .unwrap();
    let selected = select(f.root.path(), None, Some("judge"), |_| true).unwrap();
    assert_eq!(selected.id.name, "private", "a recorded private seat may serve a guarded role");

    // `missing` is excluded, and a pool with nothing else refuses the draw.
    // (config.toml is cleared too: the recorded hash above would keep the
    // profile `recorded` even with a spoiled registration.)
    unseal_private(&f.profile("private"));
    fs::write(f.profile("private").join("config.toml"), "").unwrap();
    assert!(select(f.root.path(), None, Some("judge"), |_| true).is_err());
    // An unguarded role never reads the trust state at all.
    assert!(select(f.root.path(), None, Some("curator"), |_| true).is_ok());
}
