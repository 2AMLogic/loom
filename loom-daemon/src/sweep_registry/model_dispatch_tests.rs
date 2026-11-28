//! Launch-boundary regression coverage for runtime-dependent model defaults.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::sweep_registry::test_support::*;
use crate::sweep_registry::{DispatchModel, SweepRegistry};
use crate::types::SweepKind;
use crate::work_finder::{RegistryDispatcher, WorkDispatcher};
use crate::worker_spawn::{profiles, LaunchStage, Options};
use serde_json::{json, Value};
// #9548: gate-reaching tests hold the default serial key; see `crate::write_scope_test_support`.
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

/// The second field holds the crate-wide `LOOM_CODEX_PROFILE_ROOT` lock (#9964);
/// the third the crate-wide `LOOM_CODEX_HOME` lock (#15174), acquired before it
/// and released after (see `codex_home_env`'s documented lock order).
struct Environment(
    Vec<(&'static str, Option<std::ffi::OsString>)>,
    #[allow(dead_code)] crate::tokens_pool::profile_root_env::ProfileRootLock,
    #[allow(dead_code)] crate::tokens_pool::codex_home_env::CodexHomeLock,
);
impl Environment {
    fn isolated(root: &Path) -> Self {
        let codex_home_lock = crate::tokens_pool::codex_home_env::lock();
        let lock = crate::tokens_pool::profile_root_env::lock();
        let keys = [
            "LOOM_RUNTIME",
            "LOOM_RUNTIME_BUILDER",
            "LOOM_RUNTIME_JUDGE",
            "LOOM_RUNTIME_CURATOR",
            "LOOM_CONFIG_DEFAULTS_FILE",
            "LOOM_CODEX_PROFILE_ROOT",
            "LOOM_CODEX_PROFILE",
            "LOOM_CODEX_HOME",
            "CODEX_HOME",
            "LOOM_MODEL_EXPERIMENT",
            "LOOM_MODEL_EXPERIMENT_CANARY",
            "LOOM_MODEL",
            "LOOM_MODEL_PROFILE",
            "LOOM_BACKSTOP_MAX_CONCURRENT",
            "LOOM_BACKSTOP_LEASE_STALE_SECS",
            "LOOM_BACKSTOP_LEASE_DIR",
            "LOOM_SHARED_TOKENS_DIR",
            "LOOM_SHARED_API_KEYS_DIR",
        ];
        let prior = keys
            .into_iter()
            .map(|key| {
                let value = std::env::var_os(key);
                std::env::remove_var(key);
                (key, value)
            })
            .collect();
        for key in [
            "LOOM_CONFIG_DEFAULTS_FILE",
            "LOOM_SHARED_TOKENS_DIR",
            "LOOM_SHARED_API_KEYS_DIR",
        ] {
            std::env::set_var(key, "");
        }
        std::env::set_var("LOOM_CODEX_PROFILE_ROOT", root.join("codex-profiles"));
        std::env::set_var("LOOM_BACKSTOP_LEASE_DIR", root.join("backstop"));
        Self(prior, lock, codex_home_lock)
    }
}
impl Drop for Environment {
    fn drop(&mut self) {
        for (key, value) in self.0.drain(..) {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

fn fallback_config() -> Value {
    json!({"runtimes": {
        "default": "claude", "preference": ["claude", "codex", "opencode"],
        "backstopCeiling": {"maxConcurrent": 1},
        "defaultModelProfile": "fixture-zai", "modelProfiles": {"fixture-zai": {
            "model": "glm-5.3", "providers": {"opencode": "zai-coding-plan"}
        }}
    }})
}

/// The registry, the launch-args record, the fake `gh`'s call log, and the
/// #9548 registration that lets the real write-scope check admit `root` (a
/// managed checkout whose `gh` reports push), which must outlive the dispatch.
fn fixture(
    root: &Path,
    config: &Value,
) -> (
    SweepRegistry,
    std::path::PathBuf,
    std::path::PathBuf,
    crate::write_scope_test_support::WritableRoot,
) {
    let (mut registry, gh_log) = open_pr_guard_registry(root, "", 0, false);
    let inner = registry.config.gh_bin.clone().unwrap();
    let ws = crate::write_scope_test_support::WritableRoot::register_with_gh(root, &inner);
    registry.config.gh_bin = Some(ws.gh.clone());
    let defaults = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("defaults");
    for entry in fs::read_dir(defaults.join("roles")).unwrap().flatten() {
        if entry.path().extension().and_then(|e| e.to_str()) == Some("json") {
            fs::copy(entry.path(), root.join(".loom/roles").join(entry.file_name())).unwrap();
        }
    }
    fs::copy(
        root.join(".loom/scripts/spawn-claude.sh"),
        root.join(".loom/scripts/spawn-codex.sh"),
    )
    .unwrap();
    for runtime in ["claude", "codex", "opencode"] {
        fs::copy(
            defaults.join(format!("runtimes/{runtime}.json")),
            root.join(format!(".loom/runtimes/{runtime}.json")),
        )
        .unwrap();
    }
    fs::create_dir_all(root.join(".loom/tokens")).unwrap();
    fs::write(root.join(".loom/config.json"), config.to_string()).unwrap();
    let record = root.join("launch-args");
    fs::write(
        root.join(".loom/scripts/spawn-claude.sh"),
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$LOOM_RUNTIME\" \"$@\" \"profile=$LOOM_MODEL_PROFILE\" > '{}'\nexit 0\n",
            record.display()
        ),
    )
    .unwrap();
    (registry, record, gh_log, ws)
}

fn captured_options(record: &Path, runtime: &str) -> Options {
    let captured = fs::read_to_string(record).unwrap();
    let args: Vec<_> = captured.lines().collect();
    assert_eq!(args[0], runtime, "actual admitted launch runtime");
    assert!(args
        .iter()
        .any(|arg| arg.contains("/loom:sweep 8715 --claim-owned 8715")));
    Options {
        model: args
            .windows(2)
            .find(|pair| pair[0] == "--model")
            .map(|pair| pair[1].to_owned()),
        ..Options::default()
    }
}

fn assert_native_profile(options: &Options, config: &Value) {
    let selected = profiles::select("opencode", options, config)
        .expect("actual child arguments must resolve the admitted native profile");
    assert_eq!(
        (selected.provider.as_str(), selected.model.as_str()),
        ("zai-coding-plan", "glm-5.3")
    );
}

#[test]
#[serial_test::serial]
fn work_finder_fallback_launch_uses_admitted_native_profile() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let _env = Environment::isolated(root);
    let config = fallback_config();
    let (registry, record, _, _ws) = fixture(root, &config);
    // A one-slot backstop also catches accidentally admitting/reserving twice.
    let registry = Arc::new(Mutex::new(registry));
    assert!(RegistryDispatcher::new(registry).dispatch(8715).unwrap());
    let options = captured_options(&record, "opencode");
    assert_native_profile(&options, &config);
    assert_eq!(options.model, None, "an implicit native model is left to its profile");
}

#[test]
#[serial_test::serial]
fn request_fallback_and_native_first_ignore_claude_experiment() {
    for native_first in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
        std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
        let mut config = fallback_config();
        if native_first {
            config["runtimes"]["default"] = json!("opencode");
            config["runtimes"]["preference"] = json!(["opencode", "claude"]);
        }
        let (mut registry, record, gh_log, _ws) = fixture(root, &config);
        registry
            .dispatch_with_model(
                &SweepKind::Issue(8715),
                None,
                DispatchModel::Request(None),
                None,
                None,
                crate::sweep_registry::DispatchOrigin::Unknown,
            )
            .unwrap();
        let options = captured_options(&record, "opencode");
        assert_native_profile(&options, &config);
        assert_eq!(options.model, None);
        assert!(
            !fs::read_to_string(gh_log).unwrap().contains("--json body"),
            "native dispatch must not fetch a Claude experiment stratum"
        );
    }
}

#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn claude_launch_retains_cost_safe_defaults_aliases_and_experiment() {
    claude_launch_retains_cost_safe_defaults_aliases_and_experiment_body();
}

#[serial_test::serial]
fn claude_launch_retains_cost_safe_defaults_aliases_and_experiment_body() {
    for (preference, model, experiment, expected) in [
        (false, None, false, "sonnet"),
        (true, None, false, "sonnet"),
        (false, Some("opus@xhigh"), false, "claude-opus-5@xhigh"),
        (true, None, true, ""),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        let mut config = fallback_config();
        if preference {
            // Even a configured native binding must use the admitted Claude default.
            config["runtimes"]["default"] = json!("opencode");
            config["runtimes"]["preference"] = json!(["codex", "claude"]);
        } else {
            config["runtimes"]
                .as_object_mut()
                .unwrap()
                .remove("preference");
        }
        if let Some(model) = model {
            config["autonomous"] = json!({"model": model});
        }
        let (registry, record, _, _ws) = fixture(root, &config);
        // The Claude pool is the shared one: the root is a registered git
        // checkout (#9548), where an in-checkout pool is refused (#9135).
        let _pool = crate::write_scope_test_support::SharedTokenPool::one_account();
        let expected = if experiment {
            std::env::set_var("LOOM_MODEL_EXPERIMENT", "experiment");
            std::env::set_var("LOOM_MODEL_EXPERIMENT_CANARY", "1");
            let arm = crate::script_helpers::sweep_experiment::assign_arm(8715);
            crate::script_helpers::sweep_experiment::resolved_arm_model(arm, &config)
        } else {
            expected.into()
        };
        let registry = Arc::new(Mutex::new(registry));
        assert!(RegistryDispatcher::new(registry).dispatch(8715).unwrap());
        assert_eq!(captured_options(&record, "claude").model.as_deref(), Some(expected.as_str()));
    }
}

#[test]
#[serial_test::serial]
fn native_launch_keeps_config_and_explicit_pins_including_invalid_ones() {
    for (configured, explicit, expected, valid) in [
        ("glm-5.3", None, "glm-5.3", true),
        ("opus", None, "claude-opus-5", false),
        ("glm-5.3", Some("sonnet"), "sonnet", false),
        ("opus", Some("other/pinned-model"), "other/pinned-model", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        let mut config = fallback_config();
        config["autonomous"] = json!({"model": configured});
        let (mut registry, record, _, _ws) = fixture(root, &config);
        registry
            .dispatch_with_model(
                &SweepKind::Issue(8715),
                None,
                DispatchModel::Request(explicit),
                None,
                None,
                crate::sweep_registry::DispatchOrigin::Unknown,
            )
            .unwrap();
        let options = captured_options(&record, "opencode");
        assert_eq!(options.model.as_deref(), Some(expected));
        let selection = profiles::select("opencode", &options, &config);
        if valid {
            assert!(selection.is_ok(), "{selection:?}");
        } else {
            let error = selection.unwrap_err();
            assert_eq!(error.code, 78);
            assert!(error
                .message
                .contains("bare model differs from the selected profile"));
        }
    }
}

#[test]
#[serial_test::serial]
fn refused_preferences_and_explicit_runtime_never_claim_or_spawn() {
    for explicit_runtime in [None, Some("codex"), Some("claude")] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let _env = Environment::isolated(root);
        let mut config = fallback_config();
        if explicit_runtime.is_none() {
            config["runtimes"]["preference"] = json!(["claude", "codex"]);
        }
        let (mut registry, record, gh_log, _ws) = fixture(root, &config);
        if let Some(runtime) = explicit_runtime {
            std::env::set_var("LOOM_RUNTIME", runtime);
        }
        let result = registry.dispatch_with_model(
            &SweepKind::Issue(8715),
            None,
            DispatchModel::Request(None),
            None,
            None,
            crate::sweep_registry::DispatchOrigin::Unknown,
        );
        // A pinned Claude runtime keeps the legacy static-admission behavior,
        // even with no pool; it must not silently fall through to OpenCode.
        if explicit_runtime == Some("claude") {
            result.unwrap();
            assert_eq!(captured_options(&record, "claude").model.as_deref(), Some("sonnet"));
        } else {
            assert!(result.is_err());
            assert!(!record.exists(), "rejected dispatch must not spawn");
            assert!(!gh_log.exists(), "rejected dispatch must not probe or flip labels");
            assert!(!registry.config().locks_dir().join("issue-8715").exists());
        }
    }
}

/// #9548 negative control: the dispatch fixture above, but the credential
/// only has `pull`. The real write-scope check in `private_dispatch::prepare`
/// refuses the sweep, so nothing is launched.
#[test]
#[serial_test::serial]
fn a_read_only_credential_refuses_the_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let _env = Environment::isolated(root);
    let config = fallback_config();
    let (mut registry, record, _, _ws) = fixture(root, &config);
    let read_only = crate::write_scope_test_support::WritableRoot::read_only(
        root,
        Some(&root.join("fake-gh.sh")),
    );
    registry.config.gh_bin = Some(read_only.gh.clone());
    let outcome = RegistryDispatcher::new(Arc::new(Mutex::new(registry))).dispatch(8715);
    match outcome {
        Err(e) => assert!(format!("{e:#}").contains("#9548"), "{e:#}"),
        Ok(dispatched) => assert!(!dispatched, "a read-only root must not dispatch"),
    }
    assert!(!record.exists(), "nothing may launch on a refused root");
}

// ---- #12748: a Builder step's drawn model is the launched model ------------

fn builder_launch(
    root: &Path,
    config: &Value,
) -> std::result::Result<
    crate::sweep_registry::step_spawn::StepLaunch,
    crate::sweep_registry::step_spawn::StepRefusal,
> {
    step_launch(root, config, crate::step_role::StepRole::Builder)
}

fn step_launch(
    root: &Path,
    config: &Value,
    role: crate::step_role::StepRole,
) -> std::result::Result<
    crate::sweep_registry::step_spawn::StepLaunch,
    crate::sweep_registry::step_spawn::StepRefusal,
> {
    let (registry, _, _, _ws) = fixture(root, config);
    let _pool = crate::write_scope_test_support::SharedTokenPool::one_account();
    let kind = SweepKind::Step { item: 8715, role };
    // The hermetic fixture skips admission; a draw only exists with it.
    let mut step_config = registry.config.clone();
    step_config.skip_label_flip = false;
    SweepRegistry::resolve_step_launch(&step_config, role, &kind)
}

fn draw_config(model_config: &str, draw: Value) -> Value {
    draw_config_for("builder", model_config, draw)
}

/// `role`'s configured model is `model_config`; its attempt-1 table is `draw`.
fn draw_config_for(role: &str, model_config: &str, draw: Value) -> Value {
    let mut config = fallback_config();
    config["autonomous"] = json!({"model": model_config, "roleRunner": {"model": model_config}});
    config["runtimes"]["roleDraw"] = json!({ role: [{"attempt": 1, "draw": draw}] });
    config
}

#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_builder_draw_naming_a_model_is_the_launched_model_not_the_configured_one() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    // Configured/default `sonnet`; the table draws `opus` with certainty.
    let config = draw_config("sonnet", json!({"claude/opus": 100}));
    let launch = builder_launch(dir.path(), &config).unwrap();
    assert_eq!(launch.model.as_deref(), Some("opus"));
    let stamp = launch.admission.admitted.unwrap().preference.unwrap();
    assert_eq!(stamp.model.as_deref(), Some("opus"), "selected and launched agree");
}

#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_builder_without_a_draw_keeps_the_configured_model() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let mut config = draw_config("sonnet", json!({"claude/opus": 100}));
    config["runtimes"]
        .as_object_mut()
        .unwrap()
        .remove("roleDraw");
    let launch = builder_launch(dir.path(), &config).unwrap();
    assert_eq!(launch.model.as_deref(), Some("sonnet"));
}

#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_drawn_runtime_without_a_model_keeps_the_configured_model() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let config = draw_config("sonnet", json!({"claude": 100}));
    let launch = builder_launch(dir.path(), &config).unwrap();
    assert_eq!(launch.model.as_deref(), Some("sonnet"));
}

#[test]
#[serial_test::serial]
fn an_explicit_pin_outranks_the_draw_and_a_conflicting_draw_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let (registry, _, _, _ws) = fixture(dir.path(), &fallback_config());
    let kind = SweepKind::Step {
        item: 8715,
        role: crate::step_role::StepRole::Builder,
    };
    let config = &registry.config;
    // A request pin keeps its documented precedence over the draw.
    assert_eq!(
        DispatchModel::Request(Some("pinned"))
            .resolve_drawn(config, &kind, "claude", Some("opus"))
            .unwrap()
            .as_deref(),
        Some("pinned")
    );
    // An unpinned draw is the model.
    assert_eq!(
        DispatchModel::Autonomous
            .resolve_drawn(config, &kind, "claude", Some("opus"))
            .unwrap()
            .as_deref(),
        Some("opus")
    );
    // A Claude-shaped draw on Codex is refused, never silently replaced.
    let why = DispatchModel::Autonomous
        .resolve_drawn(config, &kind, "codex", Some("sonnet"))
        .unwrap_err();
    assert!(why.contains("sonnet"), "{why}");
    // No draw: the unchanged resolver (native profile keeps its profile).
    assert_eq!(
        DispatchModel::Autonomous
            .resolve_drawn(config, &kind, "opencode", None)
            .unwrap(),
        DispatchModel::Autonomous.resolve_for_runtime(config, &kind, Some("opencode"))
    );
}

// ---- #12753: the same binding for a non-Builder stage and a walked draw ----

#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_non_builder_draw_naming_a_model_differs_from_its_configured_model() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let config = draw_config_for("curator", "sonnet", json!({"claude/opus": 100}));
    let launch = step_launch(dir.path(), &config, crate::step_role::StepRole::Curator).unwrap();
    assert_eq!(launch.model.as_deref(), Some("opus"), "the drawn model, not `sonnet`");
    let stamp = launch.admission.admitted.unwrap().preference.unwrap();
    assert_eq!(stamp.model.as_deref(), Some("opus"), "selected and launched agree");
    // Control: the same stage with no draw keeps the configured model.
    let mut undrawn = config;
    undrawn["runtimes"]
        .as_object_mut()
        .unwrap()
        .remove("roleDraw");
    let launch = step_launch(dir.path(), &undrawn, crate::step_role::StepRole::Curator).unwrap();
    assert_eq!(launch.model.as_deref(), Some("sonnet"));
}

#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn an_unavailable_draw_candidate_walks_to_the_next_candidates_own_model() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    // No Codex seat is provisioned, so the heavily weighted first pick cannot
    // serve; the walk must land on Claude and launch Claude's own drawn model,
    // not the unavailable candidate's and not the configured one.
    let config =
        draw_config_for("curator", "sonnet", json!({"codex/gpt-5.5": 1000, "claude/opus": 1}));
    let launch = step_launch(dir.path(), &config, crate::step_role::StepRole::Curator).unwrap();
    let admitted = launch.admission.admitted.unwrap();
    assert_eq!(admitted.runtime, "claude");
    assert_eq!(launch.model.as_deref(), Some("opus"));
    assert_eq!(admitted.preference.unwrap().model.as_deref(), Some("opus"));
}

// ---- #14017: a step's effort is resolved for the admitted runtime ----------

#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_step_resolves_its_effort_against_the_admitted_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    // The role draws Claude; `curator@claude` must beat the per-role value
    // and `curator@codex` must not leak onto the Claude launch.
    let mut config = draw_config_for("curator", "sonnet", json!({"claude": 100}));
    config["autonomous"]["roleRunner"]["roleEfforts"] =
        json!({"curator": "medium", "curator@claude": "high", "curator@codex": "low"});
    let launch = step_launch(dir.path(), &config, crate::step_role::StepRole::Curator).unwrap();
    assert_eq!(launch.admission.admitted.unwrap().runtime, "claude");
    assert_eq!(launch.effort.as_deref(), Some("high"));

    // Nothing configured on Claude stays unset: no `--effort` is emitted.
    let mut bare = draw_config_for("curator", "sonnet", json!({"claude": 100}));
    bare["autonomous"]["roleRunner"]["roleEfforts"] = json!({"curator@codex": "low"});
    let launch = step_launch(dir.path(), &bare, crate::step_role::StepRole::Curator).unwrap();
    assert_eq!(launch.effort, None);
}

// ---- #14963: an unqualified roleEfforts entry vs a native profile pick ----

/// `#14963` fixture: the preference walk admits `opencode` pinned to the
/// `strict-zai` model profile, whose `allowedEfforts` is low/high/max (the
/// bundled zai vocabulary), with the role-effort config `efforts`.
fn native_profile_effort_config(efforts: Value) -> Value {
    let mut config = fallback_config();
    config["runtimes"]["default"] = json!("opencode");
    config["runtimes"]["preference"] =
        json!([{"runtime": "opencode", "modelProfile": "strict-zai"}]);
    config["runtimes"]["modelProfiles"]["strict-zai"] = json!({
        "model": "glm-5.3",
        "providers": {"opencode": "zai-coding-plan"},
        "allowedEfforts": ["low", "high", "max"]
    });
    config["autonomous"]["roleRunner"]["roleEfforts"] = efforts;
    config
}

/// The reported incident, at the step-launch site: a bare
/// `roleEfforts.builder` meant for the Claude and Codex picks meets the
/// native profile the walk drew, whose `allowedEfforts` excludes the value —
/// the launch keeps its step (effort dropped to the profile's own) instead of
/// being refused at `profile_selection`.
#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_bare_role_effort_a_native_profile_disallows_is_dropped_not_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let config = native_profile_effort_config(json!({"builder": "medium"}));
    let launch = builder_launch(dir.path(), &config).unwrap();
    let admitted = launch.admission.admitted.unwrap();
    assert_eq!(admitted.runtime, "opencode", "the test must exercise a native pick");
    assert_eq!(admitted.preference.unwrap().model_profile.as_deref(), Some("strict-zai"));
    assert_eq!(
        launch.effort, None,
        "the profile's own effort launches instead of a profile_selection refusal"
    );
}

/// The bounds of the same reconciliation: an unqualified effort the profile
/// DOES allow is forwarded verbatim, and a runtime-qualified pin is forwarded
/// verbatim even when the profile disallows it (select refuses it loudly —
/// the fail-closed behaviour an explicit pin keeps).
#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn an_allowed_bare_effort_and_a_qualified_pin_are_forwarded_verbatim() {
    let allowed = tempfile::tempdir().unwrap();
    let _env1 = Environment::isolated(allowed.path());
    let config = native_profile_effort_config(json!({"builder": "high"}));
    let launch = builder_launch(allowed.path(), &config).unwrap();
    assert_eq!(
        launch.effort.as_deref(),
        Some("high"),
        "an unqualified effort inside allowedEfforts keeps its override"
    );

    let pinned = tempfile::tempdir().unwrap();
    let _env2 = Environment::isolated(pinned.path());
    let config = native_profile_effort_config(json!({"builder@opencode": "medium"}));
    let launch = builder_launch(pinned.path(), &config).unwrap();
    assert_eq!(
        launch.effort.as_deref(),
        Some("medium"),
        "a runtime-qualified pin is never silently dropped"
    );
}

// ---- #14634: the observed Builder refusal, judged by the launcher's select ----

/// `#14634` fixture at the incident's values: a bare `opencode` draw whose
/// launch selects the `zai-full` model profile (the bundled vocabulary —
/// `allowedEfforts` low/high/max, own effort `max`), with the role-effort
/// config `efforts`. No credential is declared, so the walk's availability
/// mirror does not gate the tap (as in the #14963 fixture).
fn zai_full_effort_config(efforts: Value) -> Value {
    let mut config = fallback_config();
    config["runtimes"]["default"] = json!("opencode");
    config["runtimes"]["preference"] = json!(["opencode"]);
    config["runtimes"]["defaultModelProfile"] = json!("zai-full");
    config["runtimes"]["modelProfiles"]["zai-full"] = json!({
        "model": "glm-5.4",
        "providers": {"opencode": "zai-coding-plan"},
        "effort": "max",
        "allowedEfforts": ["low", "high", "max"]
    });
    config["autonomous"]["roleRunner"]["roleEfforts"] = efforts;
    config
}

/// The launcher's own verdict on the launch the step site forwards: the
/// `--effort` it passes and the tap's `modelProfile` the child reads.
fn launcher_verdict(
    launch: &crate::sweep_registry::step_spawn::StepLaunch,
    config: &Value,
) -> std::result::Result<profiles::Selection, crate::worker_spawn::LaunchError> {
    let admitted = launch.admission.admitted.as_ref().unwrap();
    profiles::select(
        &admitted.runtime,
        &Options {
            effort: launch.effort.clone(),
            profile: admitted
                .preference
                .as_ref()
                .and_then(|p| p.model_profile.clone()),
            ..Options::default()
        },
        config,
    )
}

/// The incident: the unqualified `xhigh` is dropped, so the launch the step
/// forwards is one `select` accepts — the profile's own effort reaches the
/// harness, the `(zai-full, max)` launch these repos now show.
#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_bare_xhigh_builder_effort_launches_at_the_profiles_own_effort() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let config = zai_full_effort_config(json!({"builder": "xhigh"}));
    let launch = builder_launch(dir.path(), &config).unwrap();
    assert_eq!(
        launch
            .admission
            .admitted
            .as_ref()
            .map(|admitted| admitted.runtime.as_str()),
        Some("opencode"),
        "the test must exercise a native pick"
    );
    let selection = launcher_verdict(&launch, &config).unwrap();
    assert_eq!(selection.profile.as_deref(), Some("zai-full"));
    assert_eq!(selection.effort.as_deref(), Some("max"));
}

/// An explicit pin keeps its fail-closed refusal: a runtime-qualified `xhigh`
/// is forwarded verbatim and `select` refuses it, typed with the refused
/// argument.
#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_qualified_xhigh_pin_is_forwarded_and_refused_with_its_typed_boundary() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let config = zai_full_effort_config(json!({"builder@opencode": "xhigh"}));
    let launch = builder_launch(dir.path(), &config).unwrap();
    assert_eq!(launch.effort.as_deref(), Some("xhigh"));
    let error = launcher_verdict(&launch, &config).expect_err("select refuses a disallowed pin");
    assert_eq!(error.code, 78);
    assert_eq!(error.stage, Some(LaunchStage::ProfileSelectionEffort));
}

// ---- #14962: a configured role-tier model vs a native profile pick ----

/// `#14962` fixture: a non-Builder stage draws with certainty onto a native
/// pick pinned to the `strict-zai` model profile (model `glm-5.3`, no
/// `allowedEfforts` vocabulary — the model, not the effort, is what this
/// exercises), with the role-model config `models`.
fn native_profile_model_config(models: Value) -> Value {
    let mut config = fallback_config();
    config["runtimes"]["default"] = json!("opencode");
    config["runtimes"]["preference"] =
        json!([{"runtime": "opencode", "modelProfile": "strict-zai"}]);
    config["runtimes"]["modelProfiles"]["strict-zai"] = json!({
        "model": "glm-5.3",
        "providers": {"opencode": "zai-coding-plan"}
    });
    config["autonomous"]["roleRunner"]["roleModels"] = models;
    config
}

/// The reported incident, at the step-launch site: a bare
/// `roleModels.curator` meant for the Claude and Codex picks meets the
/// native profile the walk drew, whose model differs from the resolved pin —
/// the launch keeps its step (the configured model dropped to the profile's
/// own) instead of being refused at `profile_selection`.
#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_configured_role_model_a_native_profile_pick_would_refuse_is_dropped_not_refused() {
    let dir = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(dir.path());
    let config = native_profile_model_config(json!({"curator": "opus"}));
    let launch = step_launch(dir.path(), &config, crate::step_role::StepRole::Curator).unwrap();
    let admitted = launch.admission.admitted.unwrap();
    assert_eq!(admitted.runtime, "opencode", "the test must exercise a native pick");
    let profile = admitted.preference.unwrap().model_profile;
    assert_eq!(profile.as_deref(), Some("strict-zai"));
    assert_eq!(
        launch.model, None,
        "the profile's own model launches instead of a profile_selection refusal"
    );
    // And the launched shape is one `profiles::select` accepts: no `--model`
    // for the profile's own to supply.
    let options = Options {
        model: launch.model,
        profile,
        ..Options::default()
    };
    let selection = profiles::select("opencode", &options, &config).unwrap();
    assert_eq!(selection.model, "glm-5.3");
    assert_eq!(selection.profile.as_deref(), Some("strict-zai"));
}

/// The bounds of the same reconciliation at the step-launch site: the same
/// config drawing Claude still launches `--model` (alias-resolved), and a
/// drawn model pin on the native pick — an explicit statement of intent — is
/// forwarded verbatim even when it differs from the selected profile, so
/// `profiles::select` keeps its loud refusal.
#[test]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn a_configured_role_model_on_claude_and_a_drawn_native_pin_are_forwarded_verbatim() {
    // The Claude draw: the configured pin still launches.
    let claude = tempfile::tempdir().unwrap();
    let _env1 = Environment::isolated(claude.path());
    let mut config = native_profile_model_config(json!({"curator": "opus"}));
    config["runtimes"]["preference"] = json!(["claude", "opencode"]);
    let launch = step_launch(claude.path(), &config, crate::step_role::StepRole::Curator).unwrap();
    assert_eq!(
        launch.admission.admitted.unwrap().runtime,
        "claude",
        "control: this arm must draw the Claude pick"
    );
    assert_eq!(
        launch.model.as_deref(),
        Some("claude-opus-5"),
        "a Claude launch keeps the configured model (alias-resolved)"
    );

    // The drawn pin: `opencode:<profile>/<model>` names BOTH, and the model
    // half is the pick's own statement — never dropped, refused by select.
    let pinned = tempfile::tempdir().unwrap();
    let _env2 = Environment::isolated(pinned.path());
    let mut config = native_profile_model_config(json!({"curator": "opus"}));
    config["runtimes"]["roleDraw"] =
        json!({"curator": [{"attempt": 1, "draw": {"opencode:strict-zai/glm-5.4": 100}}]});
    let launch = step_launch(pinned.path(), &config, crate::step_role::StepRole::Curator).unwrap();
    let admitted = launch.admission.admitted.unwrap();
    assert_eq!(admitted.runtime, "opencode");
    assert_eq!(
        launch.model.as_deref(),
        Some("glm-5.4"),
        "a drawn model pin is never silently dropped"
    );
    let options = Options {
        model: launch.model,
        profile: admitted.preference.and_then(|p| p.model_profile),
        ..Options::default()
    };
    let error = profiles::select("opencode", &options, &config).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(error
        .message
        .contains("bare model differs from the selected profile"));
}

#[test]
#[serial_test::serial]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn judge_recovers_from_claude_exhaustion_on_codex() {
    judge_pool_recovery("codex");
}

#[test]
#[serial_test::serial]
#[serial_test::serial(loom_shared_tokens_dir_env)]
fn judge_recovers_from_claude_exhaustion_on_gemini() {
    judge_pool_recovery("opencode");
}

/// Weigh the same review-requested PR on both ticks, with both registry gates.
fn judge_draw(root: &Path, registry: &Mutex<SweepRegistry>, pr: u32) -> Option<u32> {
    use crate::step_dispatch::{
        driver::probe_exhausted_roles,
        gather::draw_inputs,
        item_draw::{weigh, ItemWeights, WeighContext},
    };
    use crate::work_finder::step_item::{classify, Inputs, PullInput, Step};

    let inputs = Inputs {
        pulls: vec![PullInput {
            number: pr,
            labels: vec!["loom:review-requested".into()],
            same_repo: true,
            ..Default::default()
        }],
        ..Default::default()
    };
    let items = draw_inputs(classify(&inputs), &[], |_, _| None);
    let now = chrono::Utc::now();
    let ctx = WeighContext {
        now,
        build_backoff_engaged: false,
        pool_exhausted_roles: probe_exhausted_roles(root, registry, now).0,
        workspace_idx: 0,
        cooldowns: registry.lock().unwrap().step_cooldowns(now),
    };
    weigh(&items, &ctx, &ItemWeights::default())
        .pool
        .into_iter()
        .find(|candidate| candidate.step == Step::Judge)
        .map(|candidate| candidate.target())
}

fn judge_pool_recovery(runtime: &str) {
    use crate::env_test_support::EnvGuard;
    use crate::step_role::StepRole;
    use crate::sweep_registry::step_spawn::{StepRefusal, StepSpawnRequest};

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let credentials = tempfile::tempdir().unwrap();
    let _env = Environment::isolated(credentials.path());
    let _credentials = EnvGuard::clear(&[
        "GEMINI_API_KEY",
        "LOOM_NATIVE_AUTH_FILE",
        "LOOM_CODEX_NO_EXEC",
        "LOOM_SPAWN_NO_EXPORT",
    ])
    .and_set("LOOM_SHARED_ACCOUNTS_ROOT", Some(std::ffi::OsStr::new("")));
    let mut config = draw_config_for(
        "judge",
        "sonnet",
        json!({"claude": 1, "codex/gpt-6.2-astra": 899, "opencode:quick-flash": 50}),
    );
    // The reported Judge configuration falls back to Claude alone on attempt two.
    config["runtimes"]["roleDraw"]["judge"]
        .as_array_mut()
        .unwrap()
        .push(json!({"attempt": 2, "draw": {"claude": 100}}));
    let (registry, record, _, _ws) = fixture(root, &config);
    assert!(!registry.config.skip_label_flip);
    let pool = crate::write_scope_test_support::SharedTokenPool::one_account();
    crate::tokens_pool::bad_tokens::mark_bad_in_dir(pool.path(), "fake", "exhausted: rate limit")
        .unwrap();
    let registry = Arc::new(Mutex::new(registry));
    let pr = 8715;
    let request = StepSpawnRequest::new(judge_draw(root, &registry, pr).unwrap(), StepRole::Judge);
    let assert_refused = || {
        let err = SweepRegistry::spawn_step_unlocked(&registry, &request).unwrap_err();
        let refusal = err.downcast_ref::<StepRefusal>().unwrap();
        assert_eq!(refusal.reason, "pool_exhausted", "{refusal}");
        assert!(refusal.detail.contains("ClaudeTokens"), "{refusal}");
        assert!(!record.exists());
        crate::step_dispatch::step_end::record_refusal(
            root,
            &registry,
            &request,
            Some(refusal),
            false,
        );
    };
    assert_refused();
    assert_eq!(judge_draw(root, &registry, pr), None);
    if runtime == "codex" {
        crate::tokens_pool::codex_hooks::test_support::guard_ready_in_workspace(
            root,
            &credentials.path().join("codex-profiles/fixture-seat"),
        );
    } else {
        std::env::set_var("GEMINI_API_KEY", "fixture-only");
    }

    {
        let _pin = EnvGuard::set("LOOM_RUNTIME_JUDGE", "claude");
        assert_refused();
        assert_eq!(judge_draw(root, &registry, pr), None);
    }

    use crate::runtime_preference::role_draw::attempts;
    attempts::record_failure(root, "judge");
    assert_eq!(judge_draw(root, &registry, pr), None, "attempt two allows only Claude");
    attempts::record_success(root, "judge");
    assert!(
        registry
            .lock()
            .unwrap()
            .pool_exhausted_roles(chrono::Utc::now())
            .contains("judge"),
        "the refusal has not aged out"
    );
    assert_eq!(
        judge_draw(root, &registry, pr),
        Some(pr),
        "tick two must draw the Judge step with {runtime} eligible"
    );

    let kind = SweepKind::Step {
        item: request.item,
        role: request.role,
    };
    let reg = registry.lock().unwrap();
    let launch = SweepRegistry::resolve_step_launch(&reg.config, request.role, &kind).unwrap();
    let admitted = launch.admission.admitted.as_ref().unwrap();
    assert_eq!(admitted.runtime, runtime);
    let stamp = admitted.preference.as_ref().unwrap();
    assert_eq!(stamp.draw.as_ref().unwrap().attempt, 1);
    let mut child = reg
        .spawn_child_process_for(
            &kind,
            &root.join("step.log"),
            "step-judge-fixture",
            launch.model.as_deref(),
            launch.effort.as_deref(),
            None,
            Some(admitted),
            launch.selection.as_ref(),
            None,
        )
        .unwrap()
        .child;
    assert!(child.wait().unwrap().success());
    let args = fs::read_to_string(record).unwrap();
    assert_eq!(args.lines().next(), Some(runtime));
    assert!(args.lines().any(|arg| arg == "/loom:judge 8715"), "{args}");
    let args: Vec<_> = args.lines().collect();
    let model = args
        .windows(2)
        .find(|pair| pair[0] == "--model")
        .map(|pair| pair[1]);
    if runtime == "codex" {
        assert_eq!(model, Some("gpt-6.2-astra"));
    } else {
        let profile = args.last().unwrap().strip_prefix("profile=").unwrap();
        assert_eq!(profile, "quick-flash");
        let options = Options {
            model: model.map(str::to_owned),
            profile: Some(profile.into()),
            ..Options::default()
        };
        let selected = profiles::select(runtime, &options, &config).unwrap();
        assert_eq!(
            (selected.provider.as_str(), selected.model.as_str()),
            ("google", "gemini-3.5-flash")
        );
    }
}
