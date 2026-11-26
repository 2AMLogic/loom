//! Tier-3 "generic passthrough" launch-shape resolution (issue #8671).
//!
//! Before this module, onboarding a new tier-3 runtime meant writing a new
//! `spawn-<runtime>.sh` that hardcoded the underlying CLI's binary name and
//! prompt flag (see `defaults/scripts/spawn-aider.sh` pre-#8671) — a new
//! shell script per CLI, which the shell language policy discourages, and
//! one that captured only the prompt flag, never how a model or reasoning
//! effort is passed.
//!
//! This module reads the **launch shape** — headless CLI binary, prompt
//! flag, extra argv, and model/effort mapping — out of the existing
//! per-runtime capability manifest (`defaults/runtimes/<name>.json` /
//! `.loom/runtimes/<name>.json`)'s `launch` object, so a new tier-3 runtime
//! is a manifest edit. It renders the resolved shape as eval-ready
//! `[ -n "${VAR:-}" ] || VAR="value"; export VAR` shell lines that
//! `defaults/scripts/spawn-generic-launch.sh` evals before exec'ing the
//! frozen `spawn-generic.sh` template — the "only when unset or empty" form
//! means the manifest supplies DEFAULTS only, so an already-set
//! `LOOM_GENERIC_*` env var always wins (env > config > default, matching
//! every other adapter's precedence; see `runtime-adapters.md`'s tier-3
//! section).
//!
//! Deliberately does **not** touch `spawn-generic.sh` itself, which is
//! `settled` in `scripts/shell-allowlist.txt` (zero fixes in six months, no
//! irreversible operations) and therefore frozen at its current size — this
//! is the "daemon-subcommand successor" the manifest resolution moves into,
//! per the shell language policy's "new executable logic goes into Rust"
//! rule.

use serde::Deserialize;
use std::ffi::OsStr;
use std::path::Path;

use crate::runtime_admission::{bundled_runtime_manifest, roots};

/// The `launch` object's closed schema. `deny_unknown_fields` is the
/// mechanism behind this module's central safety property: a manifest typo
/// (an unrecognized key under `launch`) is a hard, fail-closed error rather
/// than a silently-ignored field — see [`resolve_launch_env`]'s
/// [`LaunchEnvOutcome::Fatal`] path.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LaunchSpec {
    /// The underlying CLI binary name -> `LOOM_GENERIC_CLI_BIN`.
    cli_bin: Option<String>,
    /// The non-interactive prompt-delivery flag -> `LOOM_GENERIC_PROMPT_FLAG`.
    prompt_flag: Option<String>,
    /// Argv tokens always prepended ahead of the prompt (e.g. aider's
    /// `--yes-always`) -> `LOOM_GENERIC_EXTRA_ARGS`, space-joined. Tokens
    /// must not themselves contain whitespace — this is the same
    /// simplification `spawn-generic.sh`'s own passthrough-args forwarding
    /// already makes.
    #[serde(default)]
    extra_args: Vec<String>,
    /// The model-selection flag -> `LOOM_GENERIC_MODEL_FLAG` (already an
    /// existing `spawn-generic.sh` hook; the manifest just supplies its
    /// default).
    model_flag: Option<String>,
    /// A model passed via environment variable instead of a flag (e.g.
    /// Vibe's `VIBE_ACTIVE_MODEL`) -> `LOOM_GENERIC_MODEL_ENV`, naming the
    /// env var to export `LOOM_MODEL` into.
    model_env: Option<String>,
    /// The reasoning-effort flag -> `LOOM_GENERIC_EFFORT_FLAG`.
    effort_flag: Option<String>,
    /// Text prepended to the effort value (e.g. Codex's
    /// `model_reasoning_effort=`, paired with `effortFlag: "-c"`) ->
    /// `LOOM_GENERIC_EFFORT_VALUE_PREFIX`.
    effort_value_prefix: Option<String>,
}

/// The subset of a runtime manifest this module reads. Every other top-level
/// key (`capabilities`, `tier`, `note`, `capabilityGate`, ...) is ignored —
/// deliberately no `deny_unknown_fields` here, so this module never has an
/// opinion about fields that belong to `runtime_admission::RuntimeManifest`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeManifestDoc {
    #[serde(default)]
    launch: Option<LaunchSpec>,
    /// #14945: a tier-3 runtime whose launch MUST draw its credential from
    /// the API-key account pool (`agy`, the Antigravity pilot). Validated by
    /// [`pooled_tier3`], not by serde.
    #[serde(default)]
    credential_pool: Option<String>,
    #[serde(default)]
    credential_env: Option<String>,
    /// The pooled launch's default model; `LOOM_MODEL` overrides it.
    #[serde(default)]
    model: Option<String>,
}

/// Outcome of [`resolve_launch_env`], mapped to `runtime-launch-env`'s three
/// exit codes by its CLI wrapper (`cli::runtime_launch_cmd`).
#[derive(Debug, PartialEq, Eq)]
pub enum LaunchEnvOutcome {
    /// No manifest reachable at all (neither on disk nor a bundled
    /// fallback) — soft: `spawn-generic-launch.sh` degrades to the legacy
    /// pure-env-var path unchanged, exactly as if this module did not exist.
    NoManifest(String),
    /// A manifest was read (or none exists but the runtime has no `launch`
    /// object at all, which resolves to an empty line list). The eval-ready
    /// default-and-export lines to apply, in a fixed field order — see
    /// [`push_default`] for the exact form.
    Resolved(Vec<String>),
    /// The manifest could not be parsed at all, or its `launch` object
    /// carries a key outside the closed schema above. Fails closed
    /// (`EX_CONFIG`, 78) — never silently ignored.
    Fatal(String),
}

/// Escape `value` for interpolation inside a double-quoted shell string —
/// the four characters that remain live inside `"..."`: backslash, `"`,
/// `$`, and backtick.
fn double_quote_escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '"' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `[ -n "${VAR:-}" ] || VAR="<value>"; export VAR` — set `VAR` to the
/// manifest's value ONLY WHEN it is currently unset or empty, then export it.
/// That is exactly "config supplies a default, env wins" with no `if` needed
/// on the shell side.
///
/// Two details are load-bearing:
///
/// * **`export` is not optional.** `spawn-generic-launch.sh` hands these to
///   `spawn-generic.sh` across an `exec`, which carries the *environment*,
///   not the evaluating shell's plain variables. A bare assignment would be
///   silently dropped at the exec boundary and `spawn-generic.sh` would exit
///   78 on its own required-env check.
/// * **The value is NOT interpolated inside a `${...}` expansion.** The
///   obvious `: "${VAR:=<value>}"` form breaks on a value containing `}`,
///   because the `}` closes the expansion early. Emitting the default as an
///   ordinary double-quoted assignment keeps `}` inert.
fn push_default(lines: &mut Vec<String>, var: &str, value: &str) {
    lines.push(format!(
        "[ -n \"${{{var}:-}}\" ] || {var}=\"{}\"; export {var}",
        double_quote_escape(value)
    ));
}

/// Read `runtime`'s manifest and return the subset of it this module owns —
/// the one manifest read behind [`resolve_launch_env`], [`cli_bin_with`] and
/// [`pooled_tier3`], so the launch path, the availability gate and the pooled
/// tier-3 pilot cannot disagree about which file (or bundled fallback)
/// describes a runtime. `Ok(None)`-shaped docs are the caller's to interpret;
/// `Err` carries the non-`Resolved` outcome to report.
fn read_doc(root: &Path, runtime: &str) -> Result<RuntimeManifestDoc, LaunchEnvOutcome> {
    let (_roles, runtimes_dir, _scripts) = roots(root);
    let manifest_path = runtimes_dir.join(format!("{runtime}.json"));

    let data = match std::fs::read(&manifest_path) {
        Ok(data) => data,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match bundled_runtime_manifest(runtime) {
                Some(bundled) => bundled.as_bytes().to_vec(),
                None => {
                    return Err(LaunchEnvOutcome::NoManifest(format!(
                        "no runtime manifest found for {runtime:?} at {} (and no bundled fallback)",
                        manifest_path.display()
                    )))
                }
            }
        }
        Err(e) => {
            return Err(LaunchEnvOutcome::NoManifest(format!(
                "could not read {}: {e}",
                manifest_path.display()
            )))
        }
    };

    let doc: RuntimeManifestDoc = match serde_json::from_slice(&data) {
        Ok(doc) => doc,
        // The only thing that can make this fail, given `RuntimeManifestDoc`
        // has no `deny_unknown_fields` of its own, is `LaunchSpec`'s closed
        // schema (an unrecognized `launch` key) or the document not being
        // valid JSON at all. Both are manifest authoring errors, not "no
        // manifest" — EX_CONFIG, not the soft `NoManifest` degrade.
        Err(e) => {
            return Err(LaunchEnvOutcome::Fatal(format!("{}: {e}", manifest_path.display())));
        }
    };
    Ok(doc)
}

/// [`read_doc`] narrowed to the `launch` object — the shape
/// [`resolve_launch_env`] renders.
fn read_launch(root: &Path, runtime: &str) -> Result<Option<LaunchSpec>, LaunchEnvOutcome> {
    read_doc(root, runtime).map(|doc| doc.launch)
}

/// The environment variable that pins a tier-3 runtime's CLI binary,
/// overriding the manifest's `launch.cliBin` (see [`push_default`]).
pub const CLI_BIN_ENV: &str = "LOOM_GENERIC_CLI_BIN";

/// The CLI binary `runtime`'s tier-3 launch would exec, when the daemon can
/// know it (issue #9368): a non-empty [`CLI_BIN_ENV`] wins, else the
/// manifest's `launch.cliBin` — the same "env > manifest" precedence
/// [`push_default`] gives the launch itself.
///
/// `None` means **unknown**, never "absent": no manifest, no `launch` object,
/// no `cliBin`, an unparseable manifest, or an override that is not UTF-8. A
/// caller must treat it as unobservable — the launch path reports those cases
/// itself, with full context.
#[must_use]
pub fn cli_bin(root: &Path, runtime: &str) -> Option<String> {
    cli_bin_with(root, runtime, std::env::var_os(CLI_BIN_ENV).as_deref())
}

/// [`cli_bin`] with the [`CLI_BIN_ENV`] value injected rather than read from
/// the process environment, so a test never has to mutate it.
#[must_use]
pub fn cli_bin_with(root: &Path, runtime: &str, env_override: Option<&OsStr>) -> Option<String> {
    if let Some(pinned) = env_override.filter(|value| !value.is_empty()) {
        return pinned.to_str().map(str::to_owned);
    }
    read_launch(root, runtime)
        .ok()
        .flatten()
        .and_then(|launch| launch.cli_bin)
        .filter(|bin| !bin.is_empty())
}

/// Resolve `runtime`'s `launch` object into eval-ready shell lines.
///
/// `root` is the working tree to resolve `.loom/runtimes/<name>.json` /
/// `defaults/runtimes/<name>.json` against (the same two-tier resolution
/// `runtime_admission::roots` uses for every other manifest lookup), falling
/// back to the manifest the daemon binary was built with when neither exists
/// on disk (`runtime_admission::bundled_runtime_manifest` — covers `aider`
/// today).
#[must_use]
pub fn resolve_launch_env(root: &Path, runtime: &str) -> LaunchEnvOutcome {
    let launch = match read_launch(root, runtime) {
        Ok(Some(launch)) => launch,
        Ok(None) => return LaunchEnvOutcome::Resolved(Vec::new()),
        Err(outcome) => return outcome,
    };

    let mut lines = Vec::new();
    if let Some(v) = &launch.cli_bin {
        push_default(&mut lines, "LOOM_GENERIC_CLI_BIN", v);
    }
    if let Some(v) = &launch.prompt_flag {
        push_default(&mut lines, "LOOM_GENERIC_PROMPT_FLAG", v);
    }
    if let Some(v) = &launch.model_flag {
        push_default(&mut lines, "LOOM_GENERIC_MODEL_FLAG", v);
    }
    if let Some(v) = &launch.model_env {
        push_default(&mut lines, "LOOM_GENERIC_MODEL_ENV", v);
    }
    if let Some(v) = &launch.effort_flag {
        push_default(&mut lines, "LOOM_GENERIC_EFFORT_FLAG", v);
    }
    if let Some(v) = &launch.effort_value_prefix {
        push_default(&mut lines, "LOOM_GENERIC_EFFORT_VALUE_PREFIX", v);
    }
    if !launch.extra_args.is_empty() {
        push_default(&mut lines, "LOOM_GENERIC_EXTRA_ARGS", &launch.extra_args.join(" "));
    }
    LaunchEnvOutcome::Resolved(lines)
}

/// A tier-3 manifest's credential-pool declaration (#14945, the Antigravity
/// pilot): this runtime's launch MUST draw `env_var` from `provider`'s
/// API-key accounts, with no environment escape — unlike a native profile's
/// ladder, an ambient variable neither wins nor bypasses the pool, because
/// the account's `maxConcurrent` cap is the pilot's spend control.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PooledTier3 {
    /// API-key pool namespace (`gemini`), shared with every other tap that
    /// declares the same namespace (OpenCode's `quick-flash` profile).
    pub provider: String,
    /// The child variable an account supplies (`GEMINI_API_KEY`). Also the
    /// `expected_env` the selection ladder checks an account's own
    /// assignment against.
    pub env_var: String,
    /// The pooled launch's default model; `LOOM_MODEL` overrides it
    /// (env > manifest, the tier-3 precedence).
    pub model: Option<String>,
}

/// Read `runtime`'s pooled-tier-3 declaration, when its manifest carries one.
///
/// * `Ok(None)` — no declaration (every tier-3 runtime except the pilot):
///   the launch and availability paths behave exactly as before #14945.
/// * `Ok(Some)` — a validated declaration.
/// * `Err` — a malformed declaration (one key without the other, an invalid
///   pool namespace or variable name, an unparseable manifest). The launch
///   path refuses at 78 with this detail; the availability walk, which
///   cannot render it with full context, stays ungated and lets the launch
///   be the loud surface — the same split `native()` already gives an
///   unresolvable profile.
///
/// NoManifest is `Ok(None)`, not an error: a runtime with no manifest
/// anywhere fails earlier and louder on its missing runner.
pub fn pooled_tier3(root: &Path, runtime: &str) -> Result<Option<PooledTier3>, String> {
    let doc = match read_doc(root, runtime) {
        Ok(doc) => doc,
        Err(LaunchEnvOutcome::Fatal(detail)) => return Err(detail),
        Err(_) => return Ok(None),
    };
    let (Some(pool), Some(env_var)) = (&doc.credential_pool, &doc.credential_env) else {
        if doc.credential_pool.is_some() || doc.credential_env.is_some() {
            return Err(format!(
                "runtime {runtime:?} declares credentialPool or credentialEnv without the other; \
                 a pooled tier-3 launch needs both"
            ));
        }
        return Ok(None);
    };
    // An invalid namespace must not be silently ignored (it would point the
    // pool nowhere the operator named) and must not fall back to a
    // derivation — the same refusal `credential::pool_provider` gives a
    // malformed explicit override.
    crate::api_keys_pool::paths::validate_provider(pool)
        .map_err(|_| format!("runtime {runtime:?} declares an invalid credentialPool namespace"))?;
    // A variable name check, never echoing the value: a mistaken string must
    // not be reflected into a log (the same caution `profiles::check_name`
    // takes). `UPPER_SNAKE_CASE`, matching the registry's own check so an
    // account registered for this variable can never mismatch on casing.
    if env_var.is_empty()
        || !env_var
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        || env_var.starts_with(|b: char| b.is_ascii_digit())
    {
        return Err(format!("runtime {runtime:?} declares an invalid credentialEnv variable name"));
    }
    let model = doc.model.filter(|m| !m.trim().is_empty());
    Ok(Some(PooledTier3 {
        provider: pool.clone(),
        env_var: env_var.clone(),
        model,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_manifest(dir: &Path, name: &str, body: &str) {
        let runtimes = dir.join(".loom").join("runtimes");
        fs::create_dir_all(&runtimes).unwrap();
        fs::write(runtimes.join(format!("{name}.json")), body).unwrap();
    }

    #[test]
    fn resolves_every_declared_field_in_fixed_order() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{
                "runtime": "widget",
                "capabilities": {},
                "launch": {
                    "cliBin": "widget-cli",
                    "promptFlag": "--message",
                    "extraArgs": ["--yes-always"],
                    "modelFlag": "--model",
                    "modelEnv": "WIDGET_MODEL",
                    "effortFlag": "-c",
                    "effortValuePrefix": "model_reasoning_effort="
                }
            }"#,
        );
        let LaunchEnvOutcome::Resolved(lines) = resolve_launch_env(dir.path(), "widget") else {
            panic!("expected Resolved");
        };
        assert_eq!(
            lines,
            vec![
                r#"[ -n "${LOOM_GENERIC_CLI_BIN:-}" ] || LOOM_GENERIC_CLI_BIN="widget-cli"; export LOOM_GENERIC_CLI_BIN"#,
                r#"[ -n "${LOOM_GENERIC_PROMPT_FLAG:-}" ] || LOOM_GENERIC_PROMPT_FLAG="--message"; export LOOM_GENERIC_PROMPT_FLAG"#,
                r#"[ -n "${LOOM_GENERIC_MODEL_FLAG:-}" ] || LOOM_GENERIC_MODEL_FLAG="--model"; export LOOM_GENERIC_MODEL_FLAG"#,
                r#"[ -n "${LOOM_GENERIC_MODEL_ENV:-}" ] || LOOM_GENERIC_MODEL_ENV="WIDGET_MODEL"; export LOOM_GENERIC_MODEL_ENV"#,
                r#"[ -n "${LOOM_GENERIC_EFFORT_FLAG:-}" ] || LOOM_GENERIC_EFFORT_FLAG="-c"; export LOOM_GENERIC_EFFORT_FLAG"#,
                r#"[ -n "${LOOM_GENERIC_EFFORT_VALUE_PREFIX:-}" ] || LOOM_GENERIC_EFFORT_VALUE_PREFIX="model_reasoning_effort="; export LOOM_GENERIC_EFFORT_VALUE_PREFIX"#,
                r#"[ -n "${LOOM_GENERIC_EXTRA_ARGS:-}" ] || LOOM_GENERIC_EXTRA_ARGS="--yes-always"; export LOOM_GENERIC_EXTRA_ARGS"#,
            ]
        );
    }

    #[test]
    fn no_launch_object_resolves_to_an_empty_line_list() {
        let dir = tempdir().unwrap();
        write_manifest(dir.path(), "widget", r#"{"runtime": "widget", "capabilities": {}}"#);
        assert_eq!(
            resolve_launch_env(dir.path(), "widget"),
            LaunchEnvOutcome::Resolved(Vec::new())
        );
    }

    #[test]
    fn an_unrecognized_launch_key_fails_closed() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{
                "runtime": "widget",
                "capabilities": {},
                "launch": { "cliBin": "widget-cli", "bogusKey": "oops" }
            }"#,
        );
        let outcome = resolve_launch_env(dir.path(), "widget");
        let LaunchEnvOutcome::Fatal(msg) = outcome else {
            panic!("expected Fatal, got {outcome:?}");
        };
        assert!(msg.contains("bogusKey"), "{msg}");
    }

    #[test]
    fn a_missing_manifest_with_no_bundled_fallback_is_soft() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".loom").join("runtimes")).unwrap();
        let outcome = resolve_launch_env(dir.path(), "totally-unknown-runtime");
        assert!(matches!(outcome, LaunchEnvOutcome::NoManifest(_)), "{outcome:?}");
    }

    #[test]
    fn aider_bundled_fallback_resolves_with_no_on_disk_manifest() {
        // No .loom/runtimes/ directory at all -> `roots()` falls back to
        // `defaults/runtimes/`, which also does not exist under a bare
        // tempdir, so this exercises `bundled_runtime_manifest("aider")`
        // exactly as a fresh consumer install (pre-resync) would.
        let dir = tempdir().unwrap();
        let outcome = resolve_launch_env(dir.path(), "aider");
        let LaunchEnvOutcome::Resolved(lines) = outcome else {
            panic!("expected Resolved from the bundled aider.json, got {outcome:?}");
        };
        assert!(
            lines
                .iter()
                .any(|l| l.contains(r#"LOOM_GENERIC_CLI_BIN="aider""#)),
            "{lines:?}"
        );
    }

    #[test]
    fn values_needing_double_quote_escaping_round_trip_safely() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "launch": {"promptFlag": "--say \"hi\" $x"}}"#,
        );
        let LaunchEnvOutcome::Resolved(lines) = resolve_launch_env(dir.path(), "widget") else {
            panic!("expected Resolved");
        };
        assert_eq!(
            lines,
            vec![
                r#"[ -n "${LOOM_GENERIC_PROMPT_FLAG:-}" ] || LOOM_GENERIC_PROMPT_FLAG="--say \"hi\" \$x"; export LOOM_GENERIC_PROMPT_FLAG"#
            ]
        );
    }

    /// The `}` that would close a `${VAR:=<value>}` expansion early — the
    /// exact reason [`push_default`] does not use that form. Evaluated by a
    /// real shell in `tests/test-spawn-generic-launch.sh`; asserted here as
    /// a plain string so a regression is caught without spawning bash.
    #[test]
    fn a_value_containing_a_closing_brace_is_not_interpolated_into_an_expansion() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "launch": {"promptFlag": "--odd}flag"}}"#,
        );
        let LaunchEnvOutcome::Resolved(lines) = resolve_launch_env(dir.path(), "widget") else {
            panic!("expected Resolved");
        };
        assert_eq!(
            lines,
            vec![
                r#"[ -n "${LOOM_GENERIC_PROMPT_FLAG:-}" ] || LOOM_GENERIC_PROMPT_FLAG="--odd}flag"; export LOOM_GENERIC_PROMPT_FLAG"#
            ]
        );
    }

    /// #9368: the accessor the availability gate reads — manifest value, env
    /// override, no `launch` object, no manifest. The override is injected,
    /// never set on the process.
    #[test]
    fn cli_bin_prefers_the_env_override_and_is_unknown_when_undeclared() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "launch": {"cliBin": "widget-cli"}}"#,
        );
        write_manifest(dir.path(), "bare", r#"{"runtime": "bare", "capabilities": {}}"#);
        write_manifest(dir.path(), "broken", r#"{"launch": {"bogusKey": "oops"}}"#);
        let root = dir.path();

        assert_eq!(cli_bin_with(root, "widget", None).as_deref(), Some("widget-cli"));
        // An empty override is "unset", exactly as `push_default` treats it.
        assert_eq!(
            cli_bin_with(root, "widget", Some(OsStr::new(""))).as_deref(),
            Some("widget-cli")
        );
        assert_eq!(
            cli_bin_with(root, "widget", Some(OsStr::new("pinned-cli"))).as_deref(),
            Some("pinned-cli")
        );
        // The override names the binary even where no manifest could.
        assert_eq!(
            cli_bin_with(root, "totally-unknown-runtime", Some(OsStr::new("pinned-cli")))
                .as_deref(),
            Some("pinned-cli")
        );
        // Unknown, not absent: no `launch` object, no manifest, bad manifest.
        assert_eq!(cli_bin_with(root, "bare", None), None);
        assert_eq!(cli_bin_with(root, "totally-unknown-runtime", None), None);
        assert_eq!(cli_bin_with(root, "broken", None), None);
    }

    // ---- pooled_tier3 declaration parsing (#14945) ----

    #[test]
    fn pooled_tier3_reads_a_full_declaration() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{
                "runtime": "widget",
                "capabilities": {},
                "credentialPool": "gemini",
                "credentialEnv": "GEMINI_API_KEY",
                "model": "gemini-4-neon",
                "launch": {"cliBin": "widget-cli"}
            }"#,
        );
        assert_eq!(
            pooled_tier3(dir.path(), "widget").unwrap(),
            Some(PooledTier3 {
                provider: "gemini".into(),
                env_var: "GEMINI_API_KEY".into(),
                model: Some("gemini-4-neon".into()),
            })
        );
        // A whitespace-only model is no model at all.
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "credentialPool": "p", "credentialEnv": "K", "model": "  "}"#,
        );
        assert_eq!(
            pooled_tier3(dir.path(), "widget").unwrap(),
            Some(PooledTier3 {
                provider: "p".into(),
                env_var: "K".into(),
                model: None
            })
        );
    }

    #[test]
    fn pooled_tier3_is_none_without_a_declaration() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "launch": {"cliBin": "widget-cli"}}"#,
        );
        assert_eq!(pooled_tier3(dir.path(), "widget").unwrap(), None);
        // No manifest at all (and no bundled fallback for this name).
        assert_eq!(pooled_tier3(dir.path(), "totally-unknown-runtime").unwrap(), None);
    }

    #[test]
    fn pooled_tier3_rejects_half_a_declaration() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "credentialPool": "gemini"}"#,
        );
        let error = pooled_tier3(dir.path(), "widget").unwrap_err();
        assert!(error.contains("without the other"), "{error}");
    }

    #[test]
    fn pooled_tier3_rejects_invalid_names_without_echoing_them() {
        let dir = tempdir().unwrap();
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "credentialPool": "../etc", "credentialEnv": "GEMINI_API_KEY"}"#,
        );
        let error = pooled_tier3(dir.path(), "widget").unwrap_err();
        assert!(error.contains("invalid credentialPool"), "{error}");
        assert!(!error.contains("../etc"), "{error}");
        write_manifest(
            dir.path(),
            "widget",
            r#"{"runtime": "widget", "capabilities": {}, "credentialPool": "gemini", "credentialEnv": "gemini-4-neon"}"#,
        );
        let error = pooled_tier3(dir.path(), "widget").unwrap_err();
        assert!(error.contains("invalid credentialEnv"), "{error}");
        // A value-shaped string must not be reflected into the error.
        assert!(!error.contains("gemini-4-neon"), "{error}");
    }

    /// The daemon binary carries the pilot's declaration in its bundled
    /// fallback, so a consumer install with no on-disk manifest still gets
    /// the mandatory pool (the same path the launch-time reader takes).
    #[test]
    fn bundled_agy_manifest_carries_the_pilot_declaration() {
        let dir = tempdir().unwrap();
        assert_eq!(
            pooled_tier3(dir.path(), "agy").unwrap(),
            Some(PooledTier3 {
                provider: "gemini".into(),
                env_var: "GEMINI_API_KEY".into(),
                model: Some("gemini-4-neon".into()),
            })
        );
    }
}
