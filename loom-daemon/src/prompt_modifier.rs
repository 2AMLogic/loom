//! Registered prompt modifiers for single-step runs (Issue #15017).
//!
//! A caller that runs exactly one step may name a *modifier*: a registered
//! piece of text Loom adds to the role prompt, for example a keyword that asks
//! the harness for extra reasoning on the opening turn. The caller names it
//! with [`ENV`]; the registry that says what each name means per runtime is
//! config (`runtimes.promptModifiers`), so no caller-owned name is hard-coded
//! here and no role file is edited.
//!
//! ```json
//! { "runtimes": { "promptModifiers": {
//!     "deep": { "claude": { "append": "ultrathink" } }
//! } } }
//! ```
//!
//! Three properties make a modified run distinguishable from a plain one:
//!
//! 1. **Recorded** — [`current`] is stamped on every provenance-stamped span as
//!    [`ATTRIBUTE`] (so on `loom.dispatch.step`, `loom.role_attempt` and
//!    `loom.runtime.usage`), absent when no modifier is set.
//! 2. **Folded into the digest** — `hyperparams::digest_with` adds the name to
//!    the canonical serialization only when set, so two runs that differ only
//!    in the modifier hash differently and an unmodified run keeps its digest.
//! 3. **Fail closed** — [`resolve`] refuses a name that is malformed, not
//!    registered, or has no entry for the launch's runtime. The launcher turns
//!    that into a `prompt_modifier` preflight rejection; a run never proceeds
//!    plain under the modifier's name.
//!
//! The only transform is `append`: text added after the prompt. Text added
//! *before* it would stop a `/loom:<role>` invocation from expanding.

use serde_json::Value;
use std::sync::OnceLock;

/// The environment variable a single-step caller sets to name the modifier.
/// Empty or whitespace-only means unset.
pub const ENV: &str = "LOOM_PROMPT_MODIFIER";

/// The span attribute carrying the modifier name; absent when none is set.
pub const ATTRIBUTE: &str = "loom.prompt_modifier";

/// Longest modifier name accepted.
const MAX_NAME_BYTES: usize = 64;

/// The trimmed modifier name in `raw`, or `None` when it is unset or blank.
#[must_use]
pub fn normalize(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

/// The modifier this process runs under, read from [`ENV`] once.
///
/// Cached so the span stamper and the hyperparameter digest can never report
/// different values within one process.
#[must_use]
pub fn current() -> Option<&'static str> {
    static CURRENT: OnceLock<Option<String>> = OnceLock::new();
    CURRENT
        .get_or_init(|| normalize(std::env::var(ENV).ok().as_deref()))
        .as_deref()
}

/// A name is 1..=64 bytes of `[A-Za-z0-9._:-]`: safe as a span attribute, a
/// config key and a log token.
#[must_use]
pub fn is_valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_BYTES
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

/// What a registered (runtime, modifier) pair does to a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transform {
    append: String,
}

impl Transform {
    /// `prompt` with the modifier's text added after a blank line.
    #[must_use]
    pub fn apply(&self, prompt: &str) -> String {
        format!("{prompt}\n\n{}", self.append)
    }
}

/// Why a named modifier cannot be applied to a launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// The name is not 1..=64 bytes of `[A-Za-z0-9._:-]`.
    InvalidName,
    /// `runtimes.promptModifiers` has no entry for the name.
    Unregistered,
    /// The name is registered, but not for this launch's runtime.
    UnsupportedRuntime,
    /// The (runtime, name) entry exists but has no non-empty `append` string.
    Malformed,
}

impl Rejection {
    /// One line naming the problem, for the launcher's stderr.
    #[must_use]
    pub fn describe(self, name: &str, runtime: &str) -> String {
        match self {
            Self::InvalidName => format!(
                "{ENV}={name:?} is not a valid modifier name (1-{MAX_NAME_BYTES} bytes of \
                 [A-Za-z0-9._:-]); refusing to run without it (#15017)"
            ),
            Self::Unregistered => format!(
                "prompt modifier {name:?} is not registered under runtimes.promptModifiers; \
                 refusing to run without it (#15017)"
            ),
            Self::UnsupportedRuntime => format!(
                "prompt modifier {name:?} has no entry for runtime {runtime:?} under \
                 runtimes.promptModifiers.{name}; refusing to run without it (#15017)"
            ),
            Self::Malformed => format!(
                "runtimes.promptModifiers.{name}.{runtime} must be an object with a non-empty \
                 \"append\" string; refusing to run without it (#15017)"
            ),
        }
    }
}

/// The transform `name` maps to on `runtime`, from the effective `config`.
///
/// # Errors
/// A [`Rejection`] when the name is malformed, unregistered, or has no valid
/// entry for `runtime`. Never `Ok` for a pair that would run unmodified.
pub fn resolve(config: &Value, runtime: &str, name: &str) -> Result<Transform, Rejection> {
    if !is_valid_name(name) {
        return Err(Rejection::InvalidName);
    }
    let entry = config
        .get("runtimes")
        .and_then(|r| r.get("promptModifiers"))
        .and_then(|m| m.get(name))
        .ok_or(Rejection::Unregistered)?;
    let for_runtime = entry.get(runtime).ok_or(Rejection::UnsupportedRuntime)?;
    let append = for_runtime
        .get("append")
        .and_then(Value::as_str)
        .filter(|text| !text.trim().is_empty())
        .ok_or(Rejection::Malformed)?;
    Ok(Transform {
        append: append.trim().to_owned(),
    })
}

/// Rewrite the value of the launch's `-p` / `--prompt` argument with
/// `transform`, in place.
///
/// Returns `false` when `args` carries no prompt to modify (an interactive
/// launch), so the caller can refuse rather than run plain. Arguments after a
/// bare `--` are data and never rewritten; both `--prompt value` and
/// `--prompt=value` forms are handled.
#[must_use]
pub fn apply_to_args(args: &mut [std::ffi::OsString], transform: &Transform) -> bool {
    let mut i = 0;
    while i < args.len() {
        let Some(arg) = args[i].to_str().map(str::to_owned) else {
            i += 1;
            continue;
        };
        if arg == "--" {
            break;
        }
        if matches!(arg.as_str(), "-p" | "--prompt") {
            if let Some(value) = args.get(i + 1).and_then(|v| v.to_str()).map(str::to_owned) {
                args[i + 1] = transform.apply(&value).into();
                return true;
            }
            return false;
        }
        if let Some(value) = arg.strip_prefix("--prompt=") {
            args[i] = format!("--prompt={}", transform.apply(value)).into();
            return true;
        }
        i += 1;
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::ffi::OsString;

    fn config() -> Value {
        json!({"runtimes": {"promptModifiers": {
            "deep": {"claude": {"append": " ultrathink "}},
            "broken": {"claude": {"append": "  "}, "codex": "x"},
        }}})
    }

    fn os(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    #[test]
    fn blank_means_unset() {
        assert_eq!(normalize(None), None);
        assert_eq!(normalize(Some("")), None);
        assert_eq!(normalize(Some("  \t")), None);
        assert_eq!(normalize(Some(" deep ")), Some("deep".to_owned()));
    }

    #[test]
    fn names_are_bounded_and_token_shaped() {
        assert!(is_valid_name("deep"));
        assert!(is_valid_name("multi-agent.v2:a_b"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("has space"));
        assert!(!is_valid_name("new\nline"));
        assert!(!is_valid_name(&"x".repeat(65)));
        assert!(is_valid_name(&"x".repeat(64)));
    }

    #[test]
    fn a_registered_pair_resolves_and_appends_after_a_blank_line() {
        let transform = resolve(&config(), "claude", "deep").unwrap();
        assert_eq!(transform.apply("/loom:builder 7"), "/loom:builder 7\n\nultrathink");
    }

    #[test]
    fn every_unsupported_shape_is_a_named_rejection_never_ok() {
        let c = config();
        assert_eq!(resolve(&c, "claude", "bad name"), Err(Rejection::InvalidName));
        assert_eq!(resolve(&c, "claude", "nope"), Err(Rejection::Unregistered));
        assert_eq!(resolve(&json!({}), "claude", "deep"), Err(Rejection::Unregistered));
        // Registered, but only for claude: a runtime with no mapping.
        assert_eq!(resolve(&c, "codex", "deep"), Err(Rejection::UnsupportedRuntime));
        assert_eq!(resolve(&c, "pi", "deep"), Err(Rejection::UnsupportedRuntime));
        // Entry present but unusable: blank text, or not an object.
        assert_eq!(resolve(&c, "claude", "broken"), Err(Rejection::Malformed));
        assert_eq!(resolve(&c, "codex", "broken"), Err(Rejection::Malformed));
    }

    #[test]
    fn descriptions_name_the_modifier_and_say_the_run_is_refused() {
        for rejection in [
            Rejection::InvalidName,
            Rejection::Unregistered,
            Rejection::UnsupportedRuntime,
            Rejection::Malformed,
        ] {
            let text = rejection.describe("deep", "codex");
            assert!(text.contains("deep"), "{text}");
            assert!(text.contains("refusing to run without it"), "{text}");
        }
        assert!(Rejection::UnsupportedRuntime
            .describe("deep", "codex")
            .contains("codex"));
    }

    #[test]
    fn apply_to_args_rewrites_only_the_prompt_value() {
        let transform = resolve(&config(), "claude", "deep").unwrap();

        let mut spaced = os(&["--model", "m", "-p", "/loom:builder 7", "--log", "l"]);
        assert!(apply_to_args(&mut spaced, &transform));
        assert_eq!(
            spaced,
            os(&[
                "--model",
                "m",
                "-p",
                "/loom:builder 7\n\nultrathink",
                "--log",
                "l"
            ])
        );

        let mut inline = os(&["--prompt=hello"]);
        assert!(apply_to_args(&mut inline, &transform));
        assert_eq!(inline, os(&["--prompt=hello\n\nultrathink"]));
    }

    #[test]
    fn apply_to_args_reports_when_there_is_no_prompt_to_modify() {
        let transform = resolve(&config(), "claude", "deep").unwrap();
        let mut none = os(&["--model", "m"]);
        assert!(!apply_to_args(&mut none, &transform));
        assert_eq!(none, os(&["--model", "m"]), "nothing rewritten");

        // A trailing flag with no value, and a prompt-looking token after `--`.
        let mut dangling = os(&["--model", "m", "-p"]);
        assert!(!apply_to_args(&mut dangling, &transform));
        let mut after_dashes = os(&["--", "-p", "data"]);
        assert!(!apply_to_args(&mut after_dashes, &transform));
        assert_eq!(after_dashes, os(&["--", "-p", "data"]));
    }
}
