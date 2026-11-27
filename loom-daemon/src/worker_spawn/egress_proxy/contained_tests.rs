//! Coverage for a CONTAINED OpenCode launch of a `baseUrlConfig` profile
//! (issue #15527): the bundled Z.ai pair is no longer refused on the host, the
//! proxy URL reaches the in-container copy as a required internal worker
//! argument, and that copy patches the provider config after provisioning.
//!
//! The host half and the in-container half are driven separately in-process:
//! the host half through `prepare` + `contained_worker_args` +
//! `docker_command`, the in-container half through `Options::parse` +
//! `ContainedRedirect`, with the placeholder the host assigned standing in
//! for the container's credential environment. The request leg talks to a
//! real local fake upstream through the real listener.

use super::*;
use crate::worker_spawn::profiles::{self, Selection};
use crate::worker_spawn::Options;
use serde_json::json;
use std::ffi::OsString;

use super::tests::{env_lock, fake_upstream, raw_request};

const SYNTHETIC_HOST_CREDENTIAL: &str = "sk-synthetic-host-credential-15527";

fn on() -> Value {
    json!({"runtimes":{"containment":{"native":"ephemeral","credentialProxy":true}}})
}

/// The selection a normal `opencode` launch of the bundled profile resolves,
/// with the profile's upstream swapped for `upstream` (a local fake) when
/// given. Everything else — credential names, `baseUrlConfig` — is the
/// shipped profile.
fn bundled_selection(name: &str, runtime: &str, upstream: Option<&str>) -> Selection {
    let profile = profiles::bundled()
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("bundled {name}"));
    let (mut selection, _) = profiles::resolve_shape(runtime, name, &profile).unwrap();
    if let (Some(upstream), Some(proxy)) = (upstream, selection.credential_proxy.as_mut()) {
        proxy.upstream = upstream.to_string();
    }
    selection
}

fn argv_of(command: &std::process::Command) -> Vec<String> {
    std::iter::once(command.get_program())
        .chain(command.get_args())
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

fn container_profile() -> containment::Profile {
    containment::Profile {
        image: "img:test".to_string(),
        cpus: None,
        memory: None,
        launch_id: "15527".to_string(),
    }
}

fn os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

fn set_host_credential() {
    std::env::set_var("ZAI_API_KEY", SYNTHETIC_HOST_CREDENTIAL);
    std::env::remove_var("ZHIPU_API_KEY");
    std::env::remove_var("LOOM_NATIVE_CREDENTIAL_PROXY");
}

fn clear_credential_env() {
    std::env::remove_var("ZAI_API_KEY");
    std::env::remove_var("ZHIPU_API_KEY");
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

/// What the container's environment holds after `docker run`: exactly the
/// `-e NAME=VALUE` assignments of the injection (nothing else is forwarded by
/// name for these two variables), resolved through the real credential ladder.
fn container_credential(
    root: &std::path::Path,
    selection: &Selection,
    prepared: &Prepared,
) -> credential::Resolved {
    std::env::remove_var("ZAI_API_KEY");
    std::env::remove_var("ZHIPU_API_KEY");
    for (name, value) in &prepared.injection.assignments {
        std::env::set_var(name, value);
    }
    credential::resolve(root, selection).expect("the placeholder resolves in the container")
}

// ------------------------------------------------- host: no longer refused

#[test]
#[serial_test::serial]
fn both_bundled_zai_profiles_arm_on_a_contained_opencode_launch() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    std::env::set_var("LOOM_TEST_ASSUME_DOCKER", "1");
    for name in ["zai-flash", "zai-full"] {
        set_host_credential();
        let selection = bundled_selection(name, "opencode", None);
        let prepared = prepare(tmp.path(), &selection, &on(), "opencode")
            .unwrap_or_else(|e| panic!("{name} must no longer be refused: {}", e.message))
            .unwrap_or_else(|| panic!("{name} must arm"));

        let worker_args =
            prepared.contained_worker_args(&selection, "opencode", &os(&["--prompt", "hi"]));
        let command = containment::docker_command(
            &container_profile(),
            std::path::Path::new("/ws"),
            std::path::Path::new("/ws"),
            None,
            &worker_args,
            &["ZAI_API_KEY", "ZHIPU_API_KEY"],
            Some(&prepared.injection),
        )
        .unwrap();
        let argv = argv_of(&command);
        clear_credential_env();

        // Both credential names carry the SAME placeholder.
        let placeholders: Vec<&str> = ["ZAI_API_KEY=", "ZHIPU_API_KEY="]
            .iter()
            .map(|prefix| {
                argv.iter()
                    .find_map(|a| a.strip_prefix(prefix))
                    .unwrap_or_else(|| panic!("{name}: {prefix} missing: {argv:?}"))
            })
            .collect();
        assert!(placeholders[0].starts_with(PLACEHOLDER_PREFIX), "{name}: {argv:?}");
        assert_eq!(placeholders[0], placeholders[1], "{name}");
        // Neither is forwarded by name, and the host value is nowhere in argv.
        assert!(
            !argv
                .iter()
                .any(|a| a == "ZAI_API_KEY" || a == "ZHIPU_API_KEY"),
            "{argv:?}"
        );
        assert!(!argv.iter().any(|a| a.contains(SYNTHETIC_HOST_CREDENTIAL)), "{argv:?}");
        // The proxy URL rides the worker argv after the image + script.
        let tail = &argv[argv.len() - 4..];
        assert_eq!(tail[0], "--prompt");
        assert_eq!(tail[2], BASE_URL_ARG, "{argv:?}");
        assert_eq!(tail[3], prepared.base_url(), "{argv:?}");
        assert!(prepared.base_url().starts_with("http://"), "{}", prepared.base_url());
    }
    std::env::remove_var("LOOM_TEST_ASSUME_DOCKER");
}

#[test]
#[serial_test::serial]
fn the_handoff_is_added_only_for_an_opencode_launch_of_a_config_redirected_profile() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    set_host_credential();
    let zai = bundled_selection("zai-flash", "opencode", None);
    let prepared = prepare(tmp.path(), &zai, &on(), "opencode")
        .unwrap()
        .expect("arms");
    // A profile redirected through baseUrlEnv alone has nothing for the
    // container to patch: argv is byte-for-byte what it was.
    let env_only = selection_for_env_profile();
    assert_eq!(
        prepared.contained_worker_args(&env_only, "opencode", &os(&["--prompt", "hi"])),
        os(&["--prompt", "hi"])
    );
    // Any other harness gets no handoff either.
    assert_eq!(
        prepared.contained_worker_args(&zai, "kimi", &os(&["--prompt", "hi"])),
        os(&["--prompt", "hi"])
    );
    clear_credential_env();
}

fn selection_for_env_profile() -> Selection {
    super::tests::selection_with(Some(super::tests::anthropic_proxy()))
}

// ------------------------------------- in-container: patch + request routing

/// The `provision` output for the guarded path: a provider block carrying the
/// real upstream URL, plus unrelated config that must survive.
const PROVISIONED: &str = r#"{"model":"zai-coding-plan/glm-5.3-flash","permission":{"bash":"ask"},"provider":{"zai-coding-plan":{"options":{"apiKey":"{env:ZHIPU_API_KEY}","baseURL":"https://api.z.ai/api/coding/paas/v4"}},"other":{"options":{"baseURL":"https://other.example"}}}}"#;

fn patched_config(command: &std::process::Command) -> Value {
    let raw = command
        .get_envs()
        .find(|(name, _)| *name == std::ffi::OsStr::new("OPENCODE_CONFIG_CONTENT"))
        .and_then(|(_, value)| value)
        .expect("OPENCODE_CONFIG_CONTENT set");
    serde_json::from_str(raw.to_str().unwrap()).unwrap()
}

#[test]
#[serial_test::serial]
fn the_contained_copy_points_the_selected_provider_at_the_proxy_and_the_request_routes_through_it()
{
    let _g = env_lock();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let (upstream_addr, seen) = rt.block_on(fake_upstream("{\"ok\":true}"));
    let upstream = format!("http://{upstream_addr}/api/coding/paas/v4");
    set_host_credential();
    let selection = bundled_selection("zai-flash", "opencode", Some(&upstream));
    let prepared = prepare(tmp.path(), &selection, &on(), "opencode")
        .unwrap()
        .expect("arms");
    let worker_args = prepared.contained_worker_args(&selection, "opencode", &os(&["-p", "x"]));
    let base_url = prepared.base_url().to_string();
    let listener_addr = prepared.bound.addr();
    let credential = container_credential(tmp.path(), &selection, &prepared);

    // ---- in the container: parse, validate, patch after provisioning.
    let options = Options::parse(&worker_args).expect("a current worker accepts the handoff");
    assert_eq!(options.egress_proxy_base_url.as_deref(), Some(base_url.as_str()));
    let redirect = ContainedRedirect::resolve(
        options.egress_proxy_base_url.as_deref().unwrap(),
        &selection,
        "opencode",
        true,
        false,
        &credential,
    )
    .expect("a valid handoff");

    // Guarded role: provisioned config present, unrelated parts survive.
    let mut guarded = std::process::Command::new("opencode");
    guarded.env("OPENCODE_CONFIG_CONTENT", PROVISIONED);
    redirect.apply(&mut guarded, &selection.provider).unwrap();
    let config = patched_config(&guarded);
    assert_eq!(
        config
            .pointer("/provider/zai-coding-plan/options/baseURL")
            .and_then(Value::as_str),
        Some(base_url.as_str())
    );
    assert_eq!(config.pointer("/model"), Some(&json!("zai-coding-plan/glm-5.3-flash")));
    assert_eq!(config.pointer("/permission/bash"), Some(&json!("ask")));
    assert_eq!(
        config.pointer("/provider/other/options/baseURL"),
        Some(&json!("https://other.example")),
        "an unrelated provider must not be touched"
    );
    assert_eq!(
        config.pointer("/provider/zai-coding-plan/options/apiKey"),
        Some(&json!("{env:ZHIPU_API_KEY}"))
    );

    // Free-form launch: no provisioned config at all; the patch still lands.
    let mut free_form = std::process::Command::new("opencode");
    redirect.apply(&mut free_form, &selection.provider).unwrap();
    assert_eq!(
        patched_config(&free_form)
            .pointer("/provider/zai-coding-plan/options/baseURL")
            .and_then(Value::as_str),
        Some(base_url.as_str())
    );

    // ---- the request OpenCode would make with that config + placeholder.
    let placeholder = credential
        .value_for("ZHIPU_API_KEY")
        .and_then(|v| v.to_str())
        .unwrap()
        .to_string();
    assert!(placeholder.starts_with(PLACEHOLDER_PREFIX));
    let Prepared {
        registry, bound, ..
    } = prepared;
    let response = rt.block_on(async {
        let listener = bound.into_tokio().unwrap();
        tokio::spawn(server::serve(listener, registry));
        let request = format!(
            "POST /chat/completions HTTP/1.1\r\nhost: {listener_addr}\r\nauthorization: Bearer \
             {placeholder}\r\ncontent-type: application/json\r\ncontent-length: 13\r\n\r\n{{\"model\":\"x\"}}"
        );
        raw_request(listener_addr, &request).await
    });
    clear_credential_env();
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1, "exactly one request must reach the upstream");
    let head = seen[0].head.to_ascii_lowercase();
    assert!(
        head.starts_with("post /api/coding/paas/v4/chat/completions"),
        "{}",
        seen[0].head
    );
    assert!(
        head.contains(&format!(
            "authorization: bearer {}",
            SYNTHETIC_HOST_CREDENTIAL.to_lowercase()
        )),
        "the upstream must see the substituted host credential: {}",
        seen[0].head
    );
    assert!(!seen[0].head.contains(&placeholder), "{}", seen[0].head);
}

// ------------------------------------------- invalid / conflicting handoffs

fn resolve_with(
    raw: &str,
    selection: &Selection,
    runtime: &str,
    containerized: bool,
    gateway: bool,
    credential: &credential::Resolved,
) -> Result<ContainedRedirect, LaunchError> {
    ContainedRedirect::resolve(raw, selection, runtime, containerized, gateway, credential)
}

#[test]
#[serial_test::serial]
fn an_invalid_or_conflicting_handoff_is_a_config_error() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    set_host_credential();
    let selection = bundled_selection("zai-flash", "opencode", None);
    let prepared = prepare(tmp.path(), &selection, &on(), "opencode")
        .unwrap()
        .expect("arms");
    let placeholder_credential = container_credential(tmp.path(), &selection, &prepared);
    let good = prepared.base_url().to_string();
    assert!(
        resolve_with(&good, &selection, "opencode", true, false, &placeholder_credential).is_ok()
    );

    // Malformed URLs: a path, userinfo, scheme, query, fragment, port.
    for bad in [
        "https://172.17.0.1:4000",
        "http://172.17.0.1",
        "http://172.17.0.1:0",
        "http://172.17.0.1:99999",
        "http://172.17.0.1:4000/v1",
        "http://user@172.17.0.1:4000",
        "http://172.17.0.1:4000?x=1",
        "http://:4000",
        "http://evil host:4000",
        "",
    ] {
        let error = resolve_with(bad, &selection, "opencode", true, false, &placeholder_credential)
            .unwrap_err();
        assert_eq!(error.code, 78, "{bad:?}");
        assert!(error.message.contains(BASE_URL_ARG), "{}", error.message);
    }
    // Not the in-container copy, wrong harness, gateway-routed.
    for (runtime, containerized, gateway) in [
        ("opencode", false, false),
        ("pi", true, false),
        ("opencode", true, true),
    ] {
        let error = resolve_with(
            &good,
            &selection,
            runtime,
            containerized,
            gateway,
            &placeholder_credential,
        )
        .unwrap_err();
        assert_eq!(error.code, 78, "{runtime} {containerized} {gateway}");
    }
    // A profile with no config-side redirect has nothing the handoff may patch.
    let env_only = selection_for_env_profile();
    assert!(
        resolve_with(&good, &env_only, "opencode", true, false, &placeholder_credential).is_err()
    );

    // The container holds a REAL credential (no substitution happened): a
    // proxy URL beside it is a conflicting handoff, never a launch.
    std::env::remove_var("ZAI_API_KEY");
    std::env::remove_var("ZHIPU_API_KEY");
    std::env::set_var("ZAI_API_KEY", SYNTHETIC_HOST_CREDENTIAL);
    let real = credential::resolve(tmp.path(), &selection).unwrap();
    clear_credential_env();
    let error = resolve_with(&good, &selection, "opencode", true, false, &real).unwrap_err();
    assert_eq!(error.code, 78);
    assert!(error.message.contains("placeholder"), "{}", error.message);
    assert!(
        !error.message.contains(SYNTHETIC_HOST_CREDENTIAL),
        "the refusal must never echo a credential"
    );
}

// ----------------------------------------------------- the worker's parser

/// The flag set `Options::parse` accepted before #15527, frozen here. This is
/// the contract of every in-container `loom-daemon` that predates the handoff
/// (the image's own binary, or whichever one `spawn-worker.sh` locates).
fn legacy_parse_accepts(arg: &str) -> bool {
    let flag = arg.split_once('=').map_or(arg, |(flag, _)| flag);
    [
        "--use-wrapper",
        "--dangerously-skip-permissions",
        "-p",
        "--prompt",
        "--model",
        "--profile",
        "--effort",
        "--log",
    ]
    .contains(&flag)
}

#[test]
#[serial_test::serial]
fn a_pre_change_worker_would_refuse_the_handoff_and_a_current_one_requires_a_valid_value() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    set_host_credential();
    let selection = bundled_selection("zai-flash", "opencode", None);
    let prepared = prepare(tmp.path(), &selection, &on(), "opencode")
        .unwrap()
        .expect("arms");
    let worker_args = prepared.contained_worker_args(&selection, "opencode", &os(&["-p", "x"]));
    clear_credential_env();

    // The handoff is a flag the legacy parser rejects: such a worker exits 78
    // ("unsupported worker option") before it reads a credential or builds a
    // harness command, so no harness runs and no upstream request is made.
    let legacy_rejects = worker_args
        .iter()
        .filter_map(|a| a.to_str())
        .any(|a| a.starts_with("--") && !legacy_parse_accepts(a));
    assert!(legacy_rejects, "{worker_args:?}");
    // And the current parser accepts exactly one.
    assert!(Options::parse(&worker_args).is_ok());
    let mut twice = worker_args.clone();
    twice.extend(os(&[BASE_URL_ARG, "http://172.17.0.1:1"]));
    let error = Options::parse(&twice).unwrap_err();
    assert_eq!(error.code, 78);
    // A value is required; the inline form works.
    assert!(Options::parse(&os(&["-p", "x", BASE_URL_ARG])).is_err());
    assert!(Options::parse(&os(&["-p", "x", &format!("{BASE_URL_ARG}=http://h:1")])).is_ok());
}

// -------------------------------------- behaviour that must not change

#[test]
#[serial_test::serial]
fn proxy_off_and_pi_launches_are_unchanged() {
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    set_host_credential();
    let opencode = bundled_selection("zai-flash", "opencode", None);
    // Proxy flag off: contained OpenCode is plain env-passthrough.
    let off = json!({"runtimes":{"containment":{"native":"ephemeral"}}});
    assert!(prepare(tmp.path(), &opencode, &off, "opencode")
        .unwrap()
        .is_none());
    // Flag on for `pi`: baseUrlConfig is inert off opencode and the bundled
    // profile declares no baseUrlEnv, so contained pi still fails closed
    // (placeholder would reach the real upstream) — and unproxied it launches.
    let pi = bundled_selection("zai-flash", "pi", None);
    let error = prepare(tmp.path(), &pi, &on(), "pi").unwrap_err();
    assert!(error.message.contains("placeholder"), "{}", error.message);
    assert!(prepare(tmp.path(), &pi, &off, "pi").unwrap().is_none());
    // A hand-built selection that still carries baseUrlConfig on another
    // harness must not be armed through the contained path either.
    let mut forced = bundled_selection("zai-flash", "opencode", None);
    forced.provider = "zai".into();
    let error = prepare(tmp.path(), &forced, &on(), "pi").unwrap_err();
    assert!(error.message.contains("placeholder"), "{}", error.message);
    // Uncontained proxied OpenCode still binds loopback only, with no handoff.
    let (native, _) = prepare_native(tmp.path(), &opencode, &on(), "opencode")
        .unwrap()
        .expect("arms");
    clear_credential_env();
    assert!(native.base_url().starts_with("http://127.0.0.1:"), "{}", native.base_url());
    assert!(!native.injection.add_host_gateway);
    assert!(native.injection.mask_dirs.is_empty());
}

#[test]
#[serial_test::serial]
fn the_handoff_argument_is_not_accepted_as_a_host_worker_option_alias() {
    // `Options::parse` itself only records it; the host entry point refuses
    // it outside the container (see `worker_spawn::run`), and `resolve`
    // enforces the same rule where the value is consumed.
    let _g = env_lock();
    let tmp = tempfile::tempdir().unwrap();
    set_host_credential();
    let selection = bundled_selection("zai-flash", "opencode", None);
    let prepared = prepare(tmp.path(), &selection, &on(), "opencode")
        .unwrap()
        .expect("arms");
    let credential = container_credential(tmp.path(), &selection, &prepared);
    clear_credential_env();
    let error =
        resolve_with(prepared.base_url(), &selection, "opencode", false, false, &credential)
            .unwrap_err();
    assert!(error.message.contains("internal argument"), "{}", error.message);
}
