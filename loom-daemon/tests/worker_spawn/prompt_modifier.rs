//! #15017: a single-step caller's registered prompt modifier
//! (`LOOM_PROMPT_MODIFIER`) is applied at the one launch seam per runtime, and
//! a run that names one the launch cannot honor is refused at preflight with
//! the `prompt_modifier` class — it never runs plain under the modifier's name.
use super::{argv, config, legacy, stdin_of, worker};

const REGISTRY: &str = r#"{"runtimes":{"promptModifiers":{
    "deep": {"claude": {"append": "ultrathink"}, "pi": {"append": "think hard"}}
}}}"#;

fn registry(root: &std::path::Path) {
    config(root, serde_json::from_str(REGISTRY).unwrap());
}

fn refused_as_prompt_modifier(out: &std::process::Output, why: &str) {
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(78), "{why}: {stderr}");
    assert!(
        stderr.contains("# LOOM_LAUNCH_REJECTED code=78 stage=prompt_modifier"),
        "{why}: {stderr}"
    );
    assert!(stderr.contains("refusing to run without it"), "{why}: {stderr}");
    assert!(out.stdout.is_empty(), "{why}: the harness must never have started");
}

#[test]
fn a_legacy_adapter_gets_the_modified_prompt_and_nothing_else_changes() {
    let d = tempfile::tempdir().unwrap();
    legacy(d.path(), "claude");
    registry(d.path());
    let plain = worker(d.path(), "claude")
        .env_remove("LOOM_PROMPT_MODIFIER")
        .args(["--model", "m", "-p", "/loom:builder 7"])
        .output()
        .unwrap();
    assert!(plain.status.success());
    let plain = argv(&String::from_utf8(plain.stdout).unwrap());

    let modified = worker(d.path(), "claude")
        .env("LOOM_PROMPT_MODIFIER", "deep")
        .args(["--model", "m", "-p", "/loom:builder 7"])
        .output()
        .unwrap();
    assert!(modified.status.success(), "{}", String::from_utf8_lossy(&modified.stderr));
    let modified = argv(&String::from_utf8(modified.stdout).unwrap());

    assert_eq!(plain.len(), modified.len(), "only the prompt value changes");
    let differing: Vec<_> = plain
        .iter()
        .zip(&modified)
        .filter(|(a, b)| a != b)
        .collect();
    assert_eq!(differing.len(), 1, "{differing:?}");
    assert_eq!(differing[0].0, "\"/loom:builder 7\"");
    assert_eq!(differing[0].1, "\"/loom:builder 7\\n\\nultrathink\"");
}

#[test]
fn a_blank_modifier_is_unset_and_the_run_is_byte_identical() {
    let d = tempfile::tempdir().unwrap();
    legacy(d.path(), "claude");
    // No registry at all: a blank name must not even consult it.
    let unset = worker(d.path(), "claude")
        .env_remove("LOOM_PROMPT_MODIFIER")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    let blank = worker(d.path(), "claude")
        .env("LOOM_PROMPT_MODIFIER", "   ")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    assert!(unset.status.success() && blank.status.success());
    assert_eq!(
        argv(&String::from_utf8_lossy(&unset.stdout)),
        argv(&String::from_utf8_lossy(&blank.stdout))
    );
}

#[test]
fn an_unsupported_runtime_unknown_name_or_missing_prompt_is_refused_at_preflight() {
    let d = tempfile::tempdir().unwrap();
    legacy(d.path(), "claude");
    legacy(d.path(), "codex");
    registry(d.path());

    // Registered for claude and pi, but codex has no mapping.
    let out = worker(d.path(), "codex")
        .env("LOOM_PROMPT_MODIFIER", "deep")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    refused_as_prompt_modifier(&out, "unsupported (runtime, modifier) pair");
    assert!(String::from_utf8_lossy(&out.stderr).contains("codex"));

    // A name nobody registered, and a malformed one.
    for name in ["nope", "has space"] {
        let out = worker(d.path(), "claude")
            .env("LOOM_PROMPT_MODIFIER", name)
            .args(["-p", "hello"])
            .output()
            .unwrap();
        refused_as_prompt_modifier(&out, name);
    }

    // Supported pair, but an argv with no prompt to modify: not run plain.
    let out = worker(d.path(), "claude")
        .env("LOOM_PROMPT_MODIFIER", "deep")
        .args(["--model", "m"])
        .output()
        .unwrap();
    refused_as_prompt_modifier(&out, "no prompt argument");
}

#[test]
fn a_native_harness_gets_the_modifier_after_the_expanded_role_prompt() {
    let d = tempfile::tempdir().unwrap();
    registry(d.path());
    let roles = d.path().join(".loom/roles");
    std::fs::create_dir_all(&roles).unwrap();
    std::fs::write(roles.join("builder.json"), "{}").unwrap();
    std::fs::write(roles.join("builder.md"), "Role task: $ARGUMENTS").unwrap();

    let out = worker(d.path(), "pi")
        .env("LOOM_PROMPT_MODIFIER", "deep")
        .args([
            "--dangerously-skip-permissions",
            "-p",
            "/loom:builder 15017",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let prompt = stdin_of(&text);
    assert!(prompt.contains("Role task: 15017"), "{prompt}");
    assert!(
        prompt.ends_with("\n\nthink hard"),
        "modifier follows the whole prompt: {prompt:?}"
    );

    // opencode has no mapping in the registry: refused, harness never started.
    let out = worker(d.path(), "opencode")
        .env("LOOM_PROMPT_MODIFIER", "deep")
        .args(["-p", "hello"])
        .output()
        .unwrap();
    refused_as_prompt_modifier(&out, "native runtime with no mapping");
}
