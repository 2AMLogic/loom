//! Model/provider choices are data; harness adapters only translate the launch protocol.
use super::{LaunchError, Options};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// One variable (the original form) or the whole set a provider requires.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum CredentialEnv {
    One(String),
    Many(Vec<String>),
}
impl CredentialEnv {
    pub fn names(&self) -> Vec<&str> {
        match self {
            Self::One(name) => vec![name.as_str()],
            Self::Many(names) => names.iter().map(String::as_str).collect(),
        }
    }
    /// The array form declares a *required* set and fails closed when one is
    /// unset. The single-string form stays optional, so profiles that rely on a
    /// harness's own login store keep launching exactly as before.
    fn required(&self) -> bool {
        matches!(self, Self::Many(_))
    }
}

/// Alias: the one declared variable under a harness's own name. Map: an explicit
/// source-variable to child-variable mapping, for multi-variable providers
/// (Bedrock, Vertex AI).
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum CredentialTargets {
    Alias(String),
    Map(BTreeMap<String, String>),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelProfile {
    pub model: String,
    /// Provider IDs are harness vocabulary (Pi `zai`, OpenCode `zai-coding-plan`).
    pub providers: BTreeMap<String, String>,
    pub effort: Option<String>,
    /// Names only. Secrets remain in the inherited environment, this host's
    /// API-key account pool (`.loom/api-keys/`, #8401), or the CLI auth store.
    pub credential_env: Option<CredentialEnv>,
    /// Pool namespace this profile's credential belongs to. Optional: it
    /// defaults to the slug derived from `credential_env` (`ZAI_API_KEY` ->
    /// `zai`). Set it explicitly when two profiles must share one pool, or
    /// when the derivation would pick a misleading name.
    pub credential_pool: Option<String>,
    #[serde(default)]
    pub credential_targets: BTreeMap<String, CredentialTargets>,
    /// Per-profile opt-in to credential substitution (#8674): the contained
    /// worker receives a per-launch placeholder and its provider traffic is
    /// routed through a host-side proxy that swaps in the real credential.
    /// Absent (the default) means the credential is forwarded into the
    /// container's environment exactly as before.
    pub credential_proxy: Option<super::egress_proxy::ProfileProxy>,
    /// Non-secret provider options (region, project) merged into the harness's
    /// per-launch injected configuration under `provider.<id>.options`.
    #[serde(default)]
    pub provider_options: BTreeMap<String, Value>,
    /// A whole provider block (npm package, baseURL, model map) declared in the
    /// injected configuration under `provider.<id>`, for endpoints the harness
    /// does not know natively.
    #[serde(default)]
    pub provider_definition: BTreeMap<String, Value>,
    #[serde(default)]
    pub allowed_efforts: Vec<String>,
    /// How this profile is billed (#10749): `subscription` (a flat-rate plan,
    /// e.g. the z.ai coding plan), `api` (metered) or `local`. Absent: a
    /// profile that reads a provider credential is treated as metered.
    pub billing: Option<String>,
    /// Roles this profile must never be admitted for (#11400), e.g.
    /// `["judge", "curator"]` on `zai-flash`: review and triage stay off the
    /// model. Enforced at runtime admission for every selection source — a
    /// preference entry is skipped as `not-admitted(excludedRole)`, an
    /// explicit pin / role binding / env pin is refused with the reason. Names
    /// must be known roles (`loom-daemon validate` rejects anything else).
    #[serde(default)]
    pub excluded_roles: Vec<String>,
}

/// A narrow overlay on a *bundled* profile (`runtimes.modelProfileOverrides`,
/// #15252). Unlike `runtimes.modelProfiles` — whose entry replaces the bundled
/// profile wholesale, so a copy that drops `excludedRoles` silently admits GLM
/// to Judge/Curator (#11450) — an override changes only the fields below.
/// Credentials, providers, `credentialProxy`, billing and `excludedRoles` always
/// come from the bundled definition; `excludedRolesAdd` can only extend the
/// exclusion list. An explicit `null` field is "absent" (it clears a lower
/// tier's value under `deep_merge`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ModelProfileOverride {
    model: Option<String>,
    effort: Option<String>,
    allowed_efforts: Option<Vec<String>>,
    excluded_roles_add: Option<Vec<String>>,
}

impl ModelProfileOverride {
    fn apply(&self, profile: &mut ModelProfile) {
        if let Some(model) = &self.model {
            profile.model.clone_from(model);
        }
        if let Some(effort) = &self.effort {
            profile.effort = Some(effort.clone());
        }
        if let Some(allowed) = &self.allowed_efforts {
            profile.allowed_efforts.clone_from(allowed);
        }
        for role in self.excluded_roles_add.iter().flatten() {
            // Validated beforehand (`check_model_profile_overrides_config`), so
            // the canonical name exists; dedupe against the bundled entries.
            let canonical = crate::runtime_admission::canonical_role(role);
            let listed = profile
                .excluded_roles
                .iter()
                .any(|existing| crate::runtime_admission::canonical_role(existing) == canonical);
            if !listed {
                profile
                    .excluded_roles
                    .push(canonical.map_or_else(|| role.clone(), str::to_string));
            }
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileConfig {
    default_model_profile: Option<String>,
    #[serde(default)]
    model_profiles: BTreeMap<String, ModelProfile>,
    /// `None` is an explicit `null` — a higher tier clearing a lower tier's
    /// override of that profile.
    /// The whole key may itself be `null` (a tier clearing every override).
    model_profile_overrides: Option<BTreeMap<String, Option<ModelProfileOverride>>>,
}

#[derive(Debug)]
pub struct Selection {
    pub provider: String,
    pub model: String,
    pub effort: Option<String>,
    pub profile: Option<String>,
    /// (source variable in the launching environment, variable set on the child).
    pub credentials: Vec<(String, String)>,
    /// Every variable the profile declares in `credentialEnv`, whether or not a
    /// `credentialTargets` entry renames it. Containment (#8403) forwards these
    /// by NAME into the container, where profile resolution re-runs against the
    /// container's own environment: an unmapped-but-required source dropped
    /// here would fail that inner resolution closed (exit 78).
    pub credential_sources: Vec<String>,
    pub provider_options: Option<Map<String, Value>>,
    pub provider_definition: Option<Map<String, Value>>,
    /// API-key pool namespace override (#8401); see [`ModelProfile::credential_pool`].
    pub credential_pool: Option<String>,
    /// Validated `credentialProxy` block (#8674), when the profile declares one.
    pub credential_proxy: Option<super::egress_proxy::ProfileProxy>,
}

fn check_name(value: &str) -> Result<(), LaunchError> {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Ok(());
    }
    // Never echo the offending string: a mistaken value must not reach a log.
    Err(LaunchError::config(
        "credential mapping must contain environment variable names, not values",
    ))
}

pub fn bundled() -> BTreeMap<String, ModelProfile> {
    serde_json::from_str(include_str!("../../../defaults/model-profiles.json"))
        .expect("bundled profiles")
}

/// Resolve a profile by name, falling back to configuration's default and then
/// the bundled set. Configuration profiles shadow bundled ones of the same name;
/// `runtimes.modelProfileOverrides` (#15252) overlays a bundled one instead.
pub fn lookup(name: Option<&str>, config: &Value) -> Result<(String, ModelProfile), LaunchError> {
    // Fail closed on a malformed override for ANY profile: the same findings
    // `loom-daemon validate` reports, so a bad store config cannot half-apply.
    let override_findings = check_model_profile_overrides_config(config);
    if !override_findings.is_empty() {
        return Err(LaunchError::config(format!(
            "invalid runtimes.modelProfileOverrides: {}",
            override_findings.join("; ")
        )));
    }
    let configured: ProfileConfig = serde_json::from_value(
        config
            .get("runtimes")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
    )
    .map_err(|_| LaunchError::config("invalid runtime model profile configuration"))?;
    let (name, source) = match (name, configured.default_model_profile.as_deref()) {
        (Some(explicit), _) => {
            let from_env = super::nonempty_env("LOOM_MODEL_PROFILE").as_deref() == Some(explicit);
            (
                explicit.to_string(),
                if from_env {
                    "LOOM_MODEL_PROFILE"
                } else {
                    "an explicit request (--profile, or a preference tap's modelProfile)"
                },
            )
        }
        (None, Some(default)) => (default.to_string(), "runtimes.defaultModelProfile"),
        (None, None) => ("zai-flash".to_string(), "the built-in zai-flash fallback"),
    };
    let profile = match configured.model_profiles.get(&name).cloned() {
        Some(profile) => profile,
        None => {
            let mut profile = bundled().remove(&name).ok_or_else(|| {
                let mut known: Vec<String> = configured.model_profiles.keys().cloned().collect();
                known.extend(bundled().into_keys());
                known.sort();
                known.dedup();
                LaunchError::config(format!(
                    "unknown model profile `{name}` (from {source}); known profiles: {}",
                    known.join(", ")
                ))
            })?;
            if let Some(Some(overlay)) = configured
                .model_profile_overrides
                .as_ref()
                .and_then(|o| o.get(&name))
            {
                overlay.apply(&mut profile);
            }
            profile
        }
    };
    if profile.model.trim().is_empty()
        || profile.effort.as_ref().is_some_and(|s| s.trim().is_empty())
    {
        return Err(LaunchError::config("model profile has an empty model or effort"));
    }
    Ok((name, profile))
}

/// Unit name of the admission refusal a profile's `excludedRoles` produces,
/// rendered in a preference walk as `not-admitted(excludedRole)` (#11400).
pub const EXCLUDED_ROLE: &str = "excludedRole";

/// The model profile a native launch of `runtime` would resolve, when that
/// profile lists `role` (canonical name) in its `excludedRoles` (#11400).
///
/// `profile` is the explicitly chosen profile (a preference tap's
/// `modelProfile`); `None` falls back exactly as [`lookup`] does for a launch —
/// `LOOM_MODEL_PROFILE`, then `runtimes.defaultModelProfile`, then the bundled
/// `zai-flash` default — so a bare `opencode` binding is judged against the
/// profile it would actually run. Returns `None` (no objection) for a
/// non-native runtime, an unknown profile (the launch itself fails closed on
/// that), or a profile with no provider for this harness (it cannot be the
/// model that harness runs).
#[must_use]
pub fn excluded_role(
    runtime: &str,
    profile: Option<&str>,
    role: &str,
    config: &Value,
) -> Option<String> {
    if !super::is_native(runtime) {
        return None;
    }
    let env_profile = super::nonempty_env("LOOM_MODEL_PROFILE");
    let requested = profile
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .or(env_profile.as_deref());
    let (name, resolved) = lookup(requested, config).ok()?;
    if !resolved.providers.contains_key(runtime) {
        return None;
    }
    resolved
        .excluded_roles
        .iter()
        .any(|listed| crate::runtime_admission::canonical_role(listed) == Some(role))
        .then_some(name)
}

/// Issues #14962 / #15205: whether `model` is a qualified `provider/model`
/// pin (two non-empty halves) — the shape [`select`] honours verbatim, before
/// any profile lookup, when no profile was explicitly requested. Shared by
/// [`profile_rejects_model`] and [`profile_rejects_effort`] so both mirror
/// `select`'s early return on one condition.
fn is_qualified_model(model: &str) -> bool {
    model
        .split_once('/')
        .is_some_and(|(provider, rest)| !provider.is_empty() && !rest.is_empty())
}

/// Issue #14963: the model profile a native launch of `runtime` would resolve,
/// when that profile declares a non-empty `allowedEfforts` that does not
/// include `effort` — the exact predicate [`select`] refuses the launch on
/// (`stage=profile_selection`, exit 78), factored out so the daemon's launch
/// sites can drop an *unqualified* effort override instead of losing the step.
///
/// Shares [`excluded_role`]'s lookup path verbatim — `profile` is the
/// explicitly chosen profile (a preference tap's `modelProfile`); `None` falls
/// back exactly as [`lookup`] does for a launch: `LOOM_MODEL_PROFILE`, then
/// `runtimes.defaultModelProfile`, then the bundled `zai-flash` default — so
/// the daemon's reconciliation judges the profile the child would actually
/// select. Returns `None` (no objection) for a non-native runtime, an unknown
/// profile (the launch itself fails closed on that, with its own diagnostic),
/// or a profile with no provider for this harness (it cannot be the model that
/// harness runs); `Some(profile_name)` when the profile's `allowedEfforts`
/// lists at least one level and does not include `effort`.
///
/// Issue #15205: `model` is the final model the launch will carry (`None` /
/// empty when it launches with the profile's own). With **no** explicitly
/// requested profile (tap `modelProfile` or `LOOM_MODEL_PROFILE`), a qualified
/// `provider/model` (two non-empty halves) returns `None`: [`select`] honours
/// that pin verbatim — effort included — before any profile lookup, so
/// `allowedEfforts` is never consulted and the effort is not a refusal. This
/// is the same carve-out, on the same condition, as [`profile_rejects_model`].
/// With an explicit profile, or a bare / malformed model, the effort is still
/// judged against the resolved profile.
#[must_use]
pub fn profile_rejects_effort(
    runtime: &str,
    profile: Option<&str>,
    model: Option<&str>,
    effort: &str,
    config: &Value,
) -> Option<String> {
    if !super::is_native(runtime) {
        return None;
    }
    let env_profile = super::nonempty_env("LOOM_MODEL_PROFILE");
    let requested = profile
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .or(env_profile.as_deref());
    // Mirror `select`'s early return (see `profile_rejects_model`).
    if requested.is_none() && model.is_some_and(is_qualified_model) {
        return None;
    }
    let (name, resolved) = lookup(requested, config).ok()?;
    if !resolved.providers.contains_key(runtime) {
        return None;
    }
    effort_outside_allowed(effort, &resolved.allowed_efforts).then_some(name)
}

/// The predicate [`select`] refuses a launch on (`reasoning effort is
/// unsupported by the selected model profile`): a non-empty `allowedEfforts`
/// that does not list `effort`. One definition shared by `select`,
/// [`profile_rejects_effort`] and [`check_model_profile_overrides_config`]
/// (#15268), so `validate` and the launch refusal cannot drift.
fn effort_outside_allowed(effort: &str, allowed: &[String]) -> bool {
    !allowed.is_empty() && !allowed.iter().any(|a| a == effort)
}

/// Issue #14962: the model profile a native launch of `runtime` would
/// resolve, when that profile's model differs from `model` — the
/// predicate [`select`] refuses the launch on (see the qualified-pin carve-out below) (`stage=profile_selection`,
/// exit 78), factored out so the daemon's launch sites can drop a
/// *configured-tier* model override instead of losing the step.
///
/// Shares [`excluded_role`]'s lookup path verbatim — `profile` is the
/// explicitly chosen profile (a preference tap's `modelProfile`); `None`
/// falls back exactly as [`lookup`] does for a launch: `LOOM_MODEL_PROFILE`,
/// then `runtimes.defaultModelProfile`, then the bundled `zai-flash` default
/// — so the daemon's reconciliation judges the profile the child would
/// actually select. Returns `None` (no objection) for a non-native runtime,
/// an unknown profile (the launch itself fails closed on that, with its own
/// diagnostic), or a profile with no provider for this harness (it cannot be
/// the model that harness runs); `Some(profile_name)` when the resolved
/// profile's `model` differs from `model`. The comparison is the full
/// configured string against the profile's model, exactly as [`select`]
/// compares them — including a *qualified* `provider/model` value when a
/// profile was explicitly requested (tap `modelProfile` or
/// `LOOM_MODEL_PROFILE`), which is refused unless it literally equals the
/// profile's model. With **no** explicitly requested profile, a qualified
/// `provider/model` (two non-empty halves) returns `None`: [`select`] honours
/// it verbatim before any profile lookup, so it is not a refusal.
#[must_use]
pub fn profile_rejects_model(
    runtime: &str,
    profile: Option<&str>,
    model: &str,
    config: &Value,
) -> Option<String> {
    if !super::is_native(runtime) {
        return None;
    }
    let env_profile = super::nonempty_env("LOOM_MODEL_PROFILE");
    let requested = profile
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .or(env_profile.as_deref());
    // Mirror `select`'s early return: with no explicitly requested profile, a
    // qualified `provider/model` is honoured verbatim and never compared
    // against a profile (documented `roleModels` shape, configuring-resources.md).
    if requested.is_none() && is_qualified_model(model) {
        return None;
    }
    let (name, resolved) = lookup(requested, config).ok()?;
    if !resolved.providers.contains_key(runtime) {
        return None;
    }
    (resolved.model != model).then_some(name)
}

/// `loom-daemon validate` findings for `excludedRoles` (#11400): every entry,
/// in configured `runtimes.modelProfiles` and in the bundled set, must name a
/// known role. An unknown name would silently exclude nothing.
#[must_use]
pub fn check_excluded_roles_config(config: &Value) -> Vec<String> {
    fn unknown_roles(origin: &str, name: &str, roles: &[String]) -> Option<String> {
        let unknown: Vec<&str> = roles
            .iter()
            .map(String::as_str)
            .filter(|role| crate::runtime_admission::canonical_role(role).is_none())
            .collect();
        (!unknown.is_empty()).then(|| {
            format!("{origin}.{name}.excludedRoles: unknown role name(s): {}", unknown.join(", "))
        })
    }
    let mut findings = Vec::new();
    if let Some(Value::Object(map)) = config.pointer("/runtimes/modelProfiles") {
        for (name, profile) in map {
            match profile.get("excludedRoles") {
                None | Some(Value::Null) => {}
                Some(Value::Array(entries)) => {
                    let roles: Vec<String> = entries
                        .iter()
                        .map(|e| e.as_str().map_or_else(|| e.to_string(), str::to_string))
                        .collect();
                    findings.extend(unknown_roles("runtimes.modelProfiles", name, &roles));
                }
                Some(_) => findings.push(format!(
                    "runtimes.modelProfiles.{name}.excludedRoles must be an array of role names"
                )),
            }
        }
    }
    for (name, profile) in bundled() {
        findings.extend(unknown_roles("model-profiles.json", &name, &profile.excluded_roles));
    }
    findings
}

/// `loom-daemon validate` findings for `runtimes.modelProfileOverrides`
/// (#15252). Also run by [`lookup`], so a launch refuses (exit 78) on exactly
/// what validate reports. Explicit `null` (a profile or a field) is absent, not
/// an error. Rejected: a non-bundled name, a name also defined under
/// `runtimes.modelProfiles`, any key outside `model` / `effort` /
/// `allowedEfforts` / `excludedRolesAdd` (`excludedRoles` itself gets its own
/// message — the bundled list can be extended, never replaced or reduced), a
/// blank `model` / `effort` / `allowedEfforts` entry, and an unknown role in
/// `excludedRolesAdd`.
#[must_use]
pub fn check_model_profile_overrides_config(config: &Value) -> Vec<String> {
    const PATH: &str = "runtimes.modelProfileOverrides";
    let overrides = match config.pointer("/runtimes/modelProfileOverrides") {
        None | Some(Value::Null) => return Vec::new(),
        Some(Value::Object(map)) => map,
        Some(_) => {
            return vec![format!(
                "{PATH} must be an object keyed by bundled profile name"
            )];
        }
    };
    fn blank(value: &Value) -> bool {
        value.as_str().is_none_or(|s| s.trim().is_empty())
    }
    let bundled = bundled();
    let configured = config
        .pointer("/runtimes/modelProfiles")
        .and_then(Value::as_object);
    let mut findings = Vec::new();
    for (name, overlay) in overrides {
        let at = format!("{PATH}.{name}");
        let fields = match overlay {
            Value::Null => continue,
            Value::Object(fields) => fields,
            _ => {
                findings.push(format!("{at} must be an object"));
                continue;
            }
        };
        if !bundled.contains_key(name) {
            findings.push(format!(
                "{at}: `{name}` is not a bundled model profile (bundled: {}); \
                 define a new profile under runtimes.modelProfiles instead",
                bundled.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        if configured.is_some_and(|m| m.get(name).is_some_and(|p| !p.is_null())) {
            findings.push(format!(
                "{at}: `{name}` is also defined under runtimes.modelProfiles; use exactly one \
                 (modelProfileOverrides changes a bundled profile, modelProfiles replaces it)"
            ));
        }
        for (key, value) in fields {
            if value.is_null() {
                continue;
            }
            match key.as_str() {
                "model" | "effort" if blank(value) => {
                    findings.push(format!("{at}.{key} must be a non-blank string"));
                }
                "model" | "effort" => {}
                "allowedEfforts" => match value.as_array() {
                    Some(items) if !items.iter().any(blank) => {}
                    _ => findings
                        .push(format!("{at}.allowedEfforts must be an array of non-blank strings")),
                },
                "excludedRolesAdd" => match value.as_array() {
                    Some(items) => {
                        let unknown: Vec<String> = items
                            .iter()
                            .filter(|item| {
                                item.as_str()
                                    .and_then(crate::runtime_admission::canonical_role)
                                    .is_none()
                            })
                            .map(|item| {
                                item.as_str()
                                    .map_or_else(|| item.to_string(), str::to_string)
                            })
                            .collect();
                        if !unknown.is_empty() {
                            findings.push(format!(
                                "{at}.excludedRolesAdd: unknown role name(s): {}",
                                unknown.join(", ")
                            ));
                        }
                    }
                    None => findings
                        .push(format!("{at}.excludedRolesAdd must be an array of role names")),
                },
                "excludedRoles" => findings.push(format!(
                    "{at}.excludedRoles cannot be set in an override: the bundled excludedRoles \
                     can be extended with excludedRolesAdd but never replaced or reduced"
                )),
                other => findings.push(format!(
                    "{at}.{other}: unknown override key (allowed: model, effort, allowedEfforts, \
                     excludedRolesAdd)"
                )),
            }
        }
        // #15268: each field is valid alone, but the effective pair (override,
        // else the bundled sibling) must satisfy the predicate `select` refuses
        // a launch on. A non-bundled name is left to the finding above.
        if let Some(base) = bundled.get(name) {
            let effort = match fields.get("effort") {
                None | Some(Value::Null) => base.effort.as_deref(),
                Some(value) => value.as_str().filter(|s| !s.trim().is_empty()),
            };
            let allowed: Option<Vec<String>> = match fields.get("allowedEfforts") {
                None | Some(Value::Null) => Some(base.allowed_efforts.clone()),
                Some(value) => value.as_array().and_then(|items| {
                    items
                        .iter()
                        .map(|i| {
                            i.as_str()
                                .filter(|s| !s.trim().is_empty())
                                .map(str::to_string)
                        })
                        .collect()
                }),
            };
            // A malformed field was already reported above; don't double-report.
            if let (Some(effort), Some(allowed)) = (effort, allowed) {
                if effort_outside_allowed(effort, &allowed) {
                    findings.push(format!(
                        "{at}: effective effort `{effort}` is not in the effective allowedEfforts \
                         [{}]; every launch of `{name}` would be refused (exit 78) — set effort \
                         and allowedEfforts consistently",
                        allowed.join(", ")
                    ));
                }
            }
        }
    }
    findings
}

/// One harness's credential mapping: (source variable, child variable) pairs,
/// plus the variables that must be present before launch.
#[derive(Debug, Default)]
pub struct CredentialMapping {
    pub pairs: Vec<(String, String)>,
    pub required: Vec<String>,
}

/// Resolve that mapping. Names are validated; values are never read here.
pub fn credentials(
    profile: &ModelProfile,
    runtime: &str,
) -> Result<CredentialMapping, LaunchError> {
    let Some(declared) = &profile.credential_env else {
        if profile.credential_targets.contains_key(runtime) {
            return Err(LaunchError::config(
                "credentialTargets requires credentialEnv to declare the source variables",
            ));
        }
        return Ok(CredentialMapping::default());
    };
    let sources = declared.names();
    for name in &sources {
        check_name(name)?;
    }
    let pairs = match profile.credential_targets.get(runtime) {
        // No explicit target for this harness. A single-string credentialEnv
        // still has an implicit target: the harness reads the source variable
        // under its own name (Pi reads ZAI_API_KEY as ZAI_API_KEY) — exactly
        // what unset-variable inheritance would have delivered had the value
        // been exported instead of pooled. Keying the pool decision on
        // "does a rename pair exist" silently skipped the pool for every
        // profile that omits an explicit target; treating it as (VAR, VAR)
        // keeps this in the same one-variable ladder as the pool. The array
        // form is a required-in-environment set with no single implicit
        // target, so it stays unmapped here.
        None => match declared {
            CredentialEnv::One(name) => vec![(name.clone(), name.clone())],
            CredentialEnv::Many(_) => Vec::new(),
        },
        Some(CredentialTargets::Alias(target)) => {
            check_name(target)?;
            if sources.len() != 1 {
                return Err(LaunchError::config("a single credentialTargets name needs exactly one credentialEnv variable; use a variable map"));
            }
            vec![(sources[0].to_string(), target.clone())]
        }
        Some(CredentialTargets::Map(map)) => {
            let mut pairs = Vec::new();
            for (source, target) in map {
                check_name(source)?;
                check_name(target)?;
                if !sources.contains(&source.as_str()) {
                    return Err(LaunchError::config(
                        "credentialTargets maps a variable that credentialEnv does not declare",
                    ));
                }
                pairs.push((source.clone(), target.clone()));
            }
            pairs
        }
    };
    let required = if declared.required() {
        sources.iter().map(|s| (*s).to_string()).collect()
    } else {
        Vec::new()
    };
    Ok(CredentialMapping { pairs, required })
}

/// Required variables that are unset or empty in the launching environment.
pub fn missing(required: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|name| std::env::var_os(name.as_str()).is_none_or(|value| value.is_empty()))
        .cloned()
        .collect()
}

fn provider_block(
    field: &BTreeMap<String, Value>,
    runtime: &str,
    what: &str,
) -> Result<Option<Map<String, Value>>, LaunchError> {
    match field.get(runtime) {
        None => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map.clone())),
        Some(_) => Err(LaunchError::config(format!("{what} must be a JSON object"))),
    }
}

/// Provider configuration is data the harness reads, so a credential VALUE in it
/// would be written into the injected configuration string. Reject that: the
/// harness's own `{env:VAR}` indirection resolves against the mapped child
/// environment instead.
fn reject_literal_secrets(
    blocks: &[&Map<String, Value>],
    names: &[&str],
) -> Result<(), LaunchError> {
    if blocks.is_empty() {
        return Ok(());
    }
    let text = serde_json::to_string(blocks).unwrap_or_default();
    for name in names {
        // Short values collide with ordinary words; a real credential is long.
        if let Some(value) = std::env::var(name).ok().filter(|v| v.len() >= 8) {
            if text.contains(&value) {
                return Err(LaunchError::config(format!(
                    "provider configuration embeds the value of {name}; reference it as {{env:{name}}} instead"
                )));
            }
        }
    }
    Ok(())
}

/// Everything a harness needs from a profile, resolved and fail-closed.
pub fn resolve(
    runtime: &str,
    name: &str,
    profile: &ModelProfile,
) -> Result<Selection, LaunchError> {
    let (selection, required) = resolve_shape(runtime, name, profile)?;
    require_set(name, &required)?;
    Ok(selection)
}

/// Fail closed (78) when a required credential variable is unset or empty.
pub fn require_set(name: &str, required: &[String]) -> Result<(), LaunchError> {
    let unset = missing(required);
    if !unset.is_empty() {
        return Err(LaunchError::config(format!(
            "model profile '{name}' requires environment variables that are unset: {}",
            unset.join(", ")
        )));
    }
    Ok(())
}

/// [`resolve`] minus its "required variables are set" check, returning the
/// set it would have checked (#9473). A launch routed through the LLM gateway
/// never reads those variables — the gateway's virtual key replaces them — so
/// [`select`] and `worker profile-check` apply [`require_set`] only when the
/// profile is not routed.
pub fn resolve_shape(
    runtime: &str,
    name: &str,
    profile: &ModelProfile,
) -> Result<(Selection, Vec<String>), LaunchError> {
    let provider = profile
        .providers
        .get(runtime)
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            LaunchError::config("model profile has no provider binding for this harness")
        })?
        .clone();
    let mapping = credentials(profile, runtime)?;
    if let Some(pool) = &profile.credential_pool {
        crate::api_keys_pool::paths::validate_provider(pool).map_err(|_| {
            LaunchError::config("model profile credentialPool is not a valid pool name")
        })?;
        if matches!(profile.credential_env, Some(CredentialEnv::Many(_))) {
            return Err(LaunchError::config(
                "model profile credentialPool applies only to a single-string credentialEnv; \
                 an array-form credentialEnv declares a required set of variables that one \
                 pooled account (a single KEY=value) cannot satisfy",
            ));
        }
    }
    let mut credential_proxy = profile.credential_proxy.clone();
    if let Some(proxy) = &mut credential_proxy {
        // Fail at selection, not at dispatch: a malformed upstream is a
        // misconfiguration that must never reach a launch that then decides
        // what to do about it.
        proxy.validate()?;
        // The config-side base URL override writes OpenCode's provider
        // configuration; no other harness has a config contract this module
        // writes. INERT on any other runtime rather than refused: a profile
        // like the bundled `zai-flash` binds BOTH `opencode` (which needs it)
        // and `pi` (which must keep launching with the proxy flag off,
        // #11714). When the flag is on and the armed runtime is left with no
        // usable redirection, `egress_proxy::prepare_scoped` fails closed.
        if runtime != "opencode" {
            proxy.base_url_config = None;
        }
        // Both counts must be 1, not just `pairs`: an array-form
        // `credentialEnv` with a `credentialTargets` map that only covers
        // one of its declared names still produces `pairs.len() == 1`, but
        // `credential_sources` (the full declared set, #8437) carries the
        // rest forward for by-name forwarding — and nothing downstream
        // withholds a name outside the mapped pair. Refuse that shape here
        // rather than proxy one variable while forwarding the others in the
        // clear.
        let declared_count = profile
            .credential_env
            .as_ref()
            .map(|env| env.names().len())
            .unwrap_or(0);
        if mapping.pairs.len() != 1 || declared_count != 1 {
            return Err(LaunchError::config(
                "model profile credentialProxy requires exactly one credential variable; a \
                 multi-variable provider has no single value to substitute, and every \
                 credentialEnv variable it declares must be part of that single mapped pair",
            ));
        }
    }
    let provider_options = provider_block(&profile.provider_options, runtime, "providerOptions")?;
    let provider_definition =
        provider_block(&profile.provider_definition, runtime, "providerDefinition")?;
    let blocks: Vec<&Map<String, Value>> = provider_options
        .iter()
        .chain(provider_definition.iter())
        .collect();
    let declared = profile
        .credential_env
        .as_ref()
        .map(CredentialEnv::names)
        .unwrap_or_default();
    reject_literal_secrets(&blocks, &declared)?;
    let selection = Selection {
        provider,
        model: profile.model.clone(),
        effort: profile.effort.clone(),
        profile: Some(name.to_string()),
        credentials: mapping.pairs,
        credential_sources: declared.iter().map(|s| (*s).to_string()).collect(),
        provider_options,
        provider_definition,
        credential_pool: profile.credential_pool.clone(),
        credential_proxy,
    };
    Ok((selection, mapping.required))
}

pub fn select(runtime: &str, options: &Options, config: &Value) -> Result<Selection, LaunchError> {
    // A qualified explicit model is deliberately independent of the default profile.
    if options.profile.is_none() {
        if let Some((provider, model)) = options.model.as_deref().and_then(|m| m.split_once('/')) {
            if provider.is_empty() || model.is_empty() {
                return Err(LaunchError::config("model must be provider/model"));
            }
            return Ok(Selection {
                provider: provider.into(),
                model: model.into(),
                effort: options.effort.clone(),
                profile: None,
                credentials: Vec::new(),
                credential_sources: Vec::new(),
                provider_options: None,
                provider_definition: None,
                credential_pool: None,
                credential_proxy: None,
            });
        }
    }
    let (name, profile) = lookup(options.profile.as_deref(), config)?;
    let (mut selection, required) = resolve_shape(runtime, &name, &profile)?;
    // #9473: a routed launch takes its key from the LLM gateway, so its
    // provider variables need not be set. An opted-in profile the gateway
    // refuses is left to `spawn-worker`'s own `plan` call, which reports it.
    if matches!(super::llm_gateway::plan(runtime, &selection, config), Ok(None)) {
        require_set(&name, &required)?;
    }
    if let Some(model) = options.model.clone() {
        if model != profile.model {
            return Err(LaunchError::config("bare model differs from the selected profile; use provider/model or define a model profile"));
        }
    }
    if let Some(effort) = options.effort.clone() {
        selection.effort = Some(effort);
    }
    if let Some(e) = &selection.effort {
        if effort_outside_allowed(e, &profile.allowed_efforts) {
            return Err(LaunchError::config(
                "reasoning effort is unsupported by the selected model profile",
            ));
        }
    }
    Ok(selection)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown_message(name: Option<&str>, config: &Value) -> String {
        lookup(name, config)
            .expect_err("profile must be unknown")
            .message
    }

    #[test]
    fn unknown_profile_error_names_source_and_known_profiles() {
        let _g = env_lock();
        let saved = std::env::var_os("LOOM_MODEL_PROFILE");
        std::env::remove_var("LOOM_MODEL_PROFILE");
        let config =
            serde_json::json!({"runtimes":{"modelProfiles":{"mine":{"model":"m","providers":{}}}}});

        let explicit = unknown_message(Some("nope"), &config);
        assert!(explicit.contains("unknown model profile `nope`"), "{explicit}");
        assert!(explicit.contains("explicit request"), "{explicit}");
        assert!(explicit.contains("mine") && explicit.contains("zai-flash"), "{explicit}");

        std::env::set_var("LOOM_MODEL_PROFILE", "from-env");
        let env = unknown_message(Some("from-env"), &config);
        assert!(env.contains("from LOOM_MODEL_PROFILE"), "{env}");
        std::env::remove_var("LOOM_MODEL_PROFILE");

        let default =
            unknown_message(None, &serde_json::json!({"runtimes":{"defaultModelProfile":"gone"}}));
        assert!(default.contains("`gone` (from runtimes.defaultModelProfile)"), "{default}");
        assert!(default.contains("zai-flash"), "{default}");

        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
    }

    /// Serialize env mutation across these two tests — `resolve` reads
    /// `missing()` against the process environment for a `Many` (required)
    /// `credentialEnv`.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn profile_with_credentials(
        credential_env: CredentialEnv,
        credential_targets: BTreeMap<String, CredentialTargets>,
    ) -> ModelProfile {
        ModelProfile {
            model: "test-model".to_string(),
            providers: BTreeMap::from([("test-runtime".to_string(), "test-provider".to_string())]),
            effort: None,
            credential_env: Some(credential_env),
            credential_targets,
            credential_pool: None,
            credential_proxy: None,
            provider_options: BTreeMap::new(),
            provider_definition: BTreeMap::new(),
            allowed_efforts: Vec::new(),
            billing: None,
            excluded_roles: Vec::new(),
        }
    }

    /// Regression test for #8454: #8421 replaced the old
    /// `credential_env`/`credential_target` pair with `credentials` (the
    /// mapped pairs only), which silently drops every declared source that
    /// has no `credentialTargets` entry — including the DEFAULT bundled
    /// `zai-flash` profile, whose `ZAI_API_KEY` has no target at all. #8437's
    /// `credential_sources` fixes that by carrying every declared name
    /// forward regardless of mapping, but nothing pinned it: a future
    /// `Selection` refactor that keeps `credentials` and drops
    /// `credential_sources` would compile clean and silently reintroduce this
    /// bug. This asserts both fields directly off `resolve()`, not off a
    /// hand-built fixture, so it fails the moment that wiring breaks.
    #[test]
    fn credential_sources_includes_every_declared_variable_even_when_unmapped() {
        let _guard = env_lock();
        // A `Many` (array-form) `credentialEnv` is a required set (see
        // `CredentialEnv::required`) — `resolve()` fails closed if either is
        // unset, so both must be present for this to reach the assertions.
        std::env::set_var("LOOM_TEST_8454_CRED_A", "value-a");
        std::env::set_var("LOOM_TEST_8454_CRED_B", "value-b");

        let credential_targets = BTreeMap::from([(
            "test-runtime".to_string(),
            CredentialTargets::Map(BTreeMap::from([(
                "LOOM_TEST_8454_CRED_A".to_string(),
                "MAPPED_TARGET".to_string(),
            )])),
        )]);
        let profile = profile_with_credentials(
            CredentialEnv::Many(vec![
                "LOOM_TEST_8454_CRED_A".to_string(),
                "LOOM_TEST_8454_CRED_B".to_string(),
            ]),
            credential_targets,
        );

        let selection = resolve("test-runtime", "test-profile", &profile);

        std::env::remove_var("LOOM_TEST_8454_CRED_A");
        std::env::remove_var("LOOM_TEST_8454_CRED_B");

        let selection = selection.expect("both required sources are set");
        // Every declared source is carried forward, mapped or not — this is
        // the field containment (#8437) chains ahead of `credentials` so an
        // unmapped-but-required source still reaches the container.
        assert_eq!(
            selection.credential_sources,
            vec!["LOOM_TEST_8454_CRED_A", "LOOM_TEST_8454_CRED_B"]
        );
        // Only the mapped source produces a (source, target) pair.
        assert_eq!(
            selection.credentials,
            vec![("LOOM_TEST_8454_CRED_A".to_string(), "MAPPED_TARGET".to_string())]
        );
    }

    /// Same regression, for the string form with no `credentialTargets` at
    /// all — the exact shape of the bundled `zai-flash` profile the issue
    /// names (`"credentialEnv": "ZAI_API_KEY"`, no `credentialTargets` key).
    #[test]
    fn credential_sources_string_form_with_no_targets_forwards_by_name_only() {
        // The single-string form is optional (`CredentialEnv::required` is
        // false for `One`), so `resolve()` does not fail closed on an unset
        // variable here and no env mutation is needed.
        let profile = profile_with_credentials(
            CredentialEnv::One("ZAI_API_KEY".to_string()),
            BTreeMap::new(),
        );

        let selection = resolve("test-runtime", "zai-flash", &profile)
            .expect("optional credentialEnv resolves");

        assert_eq!(selection.credential_sources, vec!["ZAI_API_KEY"]);
        // PR #8428's pool-bypass fix: no explicit target still means an
        // implicit (VAR, VAR) pair, the same name inheritance would have
        // used — otherwise `credential::resolve` never consults the pool for
        // an untargeted profile (it sees zero pairs and returns early).
        assert_eq!(
            selection.credentials,
            vec![("ZAI_API_KEY".to_string(), "ZAI_API_KEY".to_string())]
        );
    }

    /// #9473: `resolve_shape` reports the required set instead of failing on
    /// it (a gateway-routed launch never reads those variables), while
    /// `resolve` — every unrouted caller — still fails closed exactly as before.
    #[test]
    fn resolve_shape_defers_the_required_check_that_resolve_still_enforces() {
        let credential_targets = BTreeMap::from([(
            "test-runtime".to_string(),
            CredentialTargets::Map(BTreeMap::from([(
                "LOOM_TEST_9473_NEVER_SET".to_string(),
                "CHILD_KEY".to_string(),
            )])),
        )]);
        let profile = profile_with_credentials(
            CredentialEnv::Many(vec!["LOOM_TEST_9473_NEVER_SET".to_string()]),
            credential_targets,
        );
        let (selection, required) = resolve_shape("test-runtime", "gw", &profile).unwrap();
        assert_eq!(required, vec!["LOOM_TEST_9473_NEVER_SET"]);
        assert_eq!(
            selection.credentials,
            vec![("LOOM_TEST_9473_NEVER_SET".to_string(), "CHILD_KEY".to_string())]
        );
        let error = resolve("test-runtime", "gw", &profile).unwrap_err();
        assert!(error.message.contains("LOOM_TEST_9473_NEVER_SET"), "{}", error.message);
    }

    /// The bundled `zai-full` profile is `zai-flash` with the full-strength
    /// model: same providers, credentials, proxy and effort, and the same
    /// Judge/Curator exclusion (GLM never runs those roles). `zai-flash` stays
    /// the built-in fallback; a deployment opts in with
    /// `runtimes.defaultModelProfile: zai-full`.
    #[test]
    fn the_bundled_zai_full_profile_is_zai_flash_with_glm_5_3() {
        let _g = env_lock();
        let raw: serde_json::Value =
            serde_json::from_str(include_str!("../../../defaults/model-profiles.json")).unwrap();
        let mut flash = raw["zai-flash"].clone();
        let full = raw["zai-full"].clone();
        assert_eq!(full["model"], "glm-5.3");
        assert_eq!(full["excludedRoles"], serde_json::json!(["judge", "curator"]));
        flash["model"] = serde_json::json!("glm-5.3");
        assert_eq!(full, flash, "zai-full differs from zai-flash only in its model");

        let saved = std::env::var_os("LOOM_MODEL_PROFILE");
        std::env::remove_var("LOOM_MODEL_PROFILE");
        let config = serde_json::json!({"runtimes":{"defaultModelProfile":"zai-full"}});
        let (name, profile) = lookup(None, &config).expect("zai-full resolves as a default");
        assert_eq!(name, "zai-full");
        assert_eq!(profile.model, "glm-5.3");
        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
    }

    /// #11714: the bundled `zai-flash` profile binds BOTH `pi` and `opencode`,
    /// and its `credentialProxy` block declares `baseUrlConfig` — an OpenCode
    /// config contract. Selection must not refuse the `pi` binding for that:
    /// an ordinary launch runs with the proxy flag off, so a hard refusal here
    /// broke every default `pi` launch. `baseUrlConfig` is simply dropped from
    /// a non-opencode selection (inert); the opencode selection keeps it.
    #[test]
    fn the_bundled_zai_flash_profile_resolves_for_both_runtimes_it_binds() {
        let profile = bundled()
            .get("zai-flash")
            .cloned()
            .expect("bundled zai-flash");
        for runtime in ["pi", "opencode"] {
            let (selection, _) = resolve_shape(runtime, "zai-flash", &profile)
                .expect("every bound runtime resolves");
            let proxy = selection
                .credential_proxy
                .expect("the proxy block survives selection");
            assert_eq!(
                proxy.base_url_config.as_deref(),
                (runtime == "opencode").then_some("baseURL"),
                "{runtime}: baseUrlConfig must be kept only for opencode"
            );
        }
    }

    /// Regression for the Judge's blocking finding on #8701: an array-form
    /// `credentialEnv` with a `credentialTargets` map that only covers ONE
    /// of its declared names produces exactly one `credentials` pair — the
    /// old `mapping.pairs.len() != 1` guard alone let this through — while
    /// `credential_sources` still carries the unmapped variable forward for
    /// by-name forwarding, which `credentialProxy`'s "exactly one variable"
    /// promise does not withhold. `resolve()` must refuse this shape, not
    /// just the two-pairs-mapped shape the pre-existing test covered.
    #[test]
    fn credential_proxy_refuses_a_declared_but_unmapped_variable() {
        let _guard = env_lock();
        std::env::set_var("LOOM_TEST_8701_CRED_A", "value-a");
        std::env::set_var("LOOM_TEST_8701_CRED_B", "value-b");

        let credential_targets = BTreeMap::from([(
            "test-runtime".to_string(),
            CredentialTargets::Map(BTreeMap::from([(
                "LOOM_TEST_8701_CRED_A".to_string(),
                "MAPPED_TARGET".to_string(),
            )])),
        )]);
        let mut profile = profile_with_credentials(
            CredentialEnv::Many(vec![
                "LOOM_TEST_8701_CRED_A".to_string(),
                "LOOM_TEST_8701_CRED_B".to_string(),
            ]),
            credential_targets,
        );
        profile.credential_proxy = Some(super::super::egress_proxy::ProfileProxy {
            upstream: "https://api.anthropic.com".to_string(),
            header: super::super::egress_proxy::HeaderStyle::AuthorizationBearer,
            base_url_env: vec!["ANTHROPIC_BASE_URL".to_string()],
            observe: false,
            base_url_config: None,
        });

        let error = resolve("test-runtime", "test-profile", &profile);

        std::env::remove_var("LOOM_TEST_8701_CRED_A");
        std::env::remove_var("LOOM_TEST_8701_CRED_B");

        let error = error.expect_err("a declared-but-unmapped variable must be refused");
        assert!(error.message.contains("every credentialEnv variable"), "{}", error.message);
    }

    /// Issue #14963: `profile_rejects_effort` mirrors `select`'s
    /// `allowedEfforts` refusal predicate on `excluded_role`'s exact lookup
    /// path, so the daemon's launch sites can drop an *unqualified* effort
    /// instead of losing the step to a `profile_selection` refusal. The full
    /// drop-vs-keep precedence lives with the resolver
    /// (`role_runner/tests/model_resolution.rs`); these tests pin the
    /// predicate itself.
    #[test]
    fn profile_rejects_effort_matches_selects_allowed_efforts_predicate() {
        let _g = env_lock();
        let saved = std::env::var_os("LOOM_MODEL_PROFILE");
        std::env::remove_var("LOOM_MODEL_PROFILE");
        let config = serde_json::json!({"runtimes":{"modelProfiles":{
            "strict": {"model": "m", "providers": {"opencode": "zai-coding-plan"},
                        "allowedEfforts": ["low", "high", "max"]},
            "open": {"model": "m", "providers": {"opencode": "zai-coding-plan"}},
            "other-harness": {"model": "m", "providers": {"pi": "zai"}}
        }}});

        // The reported incident's exact shape: a `medium` effort meeting a
        // profile that allows only low/high/max.
        assert_eq!(
            profile_rejects_effort("opencode", Some("strict"), None, "medium", &config),
            Some("strict".to_string())
        );
        // An effort the profile allows is no objection.
        assert_eq!(profile_rejects_effort("opencode", Some("strict"), None, "high", &config), None);
        // An empty `allowedEfforts` (here: absent) never rejects.
        assert_eq!(profile_rejects_effort("opencode", Some("open"), None, "medium", &config), None);
        // A non-native runtime is never judged (Claude/Codex forward the flag
        // to their own CLIs, which own their level vocabularies).
        assert_eq!(profile_rejects_effort("claude", Some("strict"), None, "medium", &config), None);
        // A profile with no provider entry for this harness cannot be the
        // model that harness runs.
        assert_eq!(
            profile_rejects_effort("opencode", Some("other-harness"), None, "medium", &config),
            None
        );
        // An unknown profile is left to the launch's own fail-closed
        // diagnostic, never judged here.
        assert_eq!(profile_rejects_effort("opencode", Some("nope"), None, "medium", &config), None);

        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
    }

    /// Issue #14963: a bare native runtime (a preference tap with no
    /// `modelProfile`) is judged against the profile the launch would
    /// actually select — the `LOOM_MODEL_PROFILE` / `defaultModelProfile` /
    /// bundled `zai-flash` fallback chain, exactly as `excluded_role` judges
    /// it. The bundled `zai-flash` allows low/high/max, so the incident's
    /// bare `medium` meets it too.
    #[test]
    fn profile_rejects_effort_falls_back_to_the_launchs_own_profile() {
        let _g = env_lock();
        let saved = std::env::var_os("LOOM_MODEL_PROFILE");

        std::env::remove_var("LOOM_MODEL_PROFILE");
        assert_eq!(
            profile_rejects_effort("opencode", None, None, "medium", &serde_json::json!({})),
            Some("zai-flash".to_string()),
            "the bundled zai-flash fallback rejects the incident's medium"
        );
        assert_eq!(
            profile_rejects_effort("opencode", None, None, "high", &serde_json::json!({})),
            None
        );

        std::env::set_var("LOOM_MODEL_PROFILE", "zai-full");
        assert_eq!(
            profile_rejects_effort("opencode", None, None, "medium", &serde_json::json!({})),
            Some("zai-full".to_string()),
            "LOOM_MODEL_PROFILE names the profile a bare tap is judged against"
        );

        std::env::remove_var("LOOM_MODEL_PROFILE");
        let config = serde_json::json!({"runtimes":{"defaultModelProfile":"zai-full", "modelProfiles":{
            "zai-full": {"model": "glm-5.3", "providers": {"opencode": "zai-coding-plan"},
                          "allowedEfforts": ["low", "high", "max"]}
        }}});
        assert_eq!(
            profile_rejects_effort("opencode", None, None, "medium", &config),
            Some("zai-full".to_string()),
            "runtimes.defaultModelProfile names the profile a bare tap is judged against"
        );

        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
    }

    /// Issue #15205: with no explicitly requested profile, a qualified
    /// `provider/model` pin is honoured by `select` verbatim — effort
    /// included, `allowedEfforts` never consulted — so `profile_rejects_effort`
    /// must not call it a refusal. An explicit profile (tap or
    /// `LOOM_MODEL_PROFILE`), a bare model and a malformed qualified model all
    /// keep today's judgment against the resolved profile.
    #[test]
    fn profile_rejects_effort_honours_a_qualified_model_pin_like_select() {
        let _g = env_lock();
        let saved = std::env::var_os("LOOM_MODEL_PROFILE");
        std::env::remove_var("LOOM_MODEL_PROFILE");
        let none = serde_json::json!({});
        let pin = Some("google/gemini-2.5-pro");

        // The gap: the bundled zai-flash rejects `medium`, but `select`
        // returns the pin's selection before ever reading its allowedEfforts.
        assert_eq!(profile_rejects_effort("opencode", None, pin, "medium", &none), None);
        // ... which the real `select` confirms, so the carve-out cannot drift.
        let selected = select(
            "opencode",
            &Options {
                model: pin.map(str::to_string),
                effort: Some("medium".to_string()),
                ..Options::default()
            },
            &none,
        )
        .expect("select honours a qualified pin without consulting allowedEfforts");
        assert_eq!(selected.effort.as_deref(), Some("medium"));

        // An explicit profile (tap) keeps judging the effort against it.
        assert_eq!(
            profile_rejects_effort("opencode", Some("zai-flash"), pin, "medium", &none),
            Some("zai-flash".to_string())
        );
        // So does LOOM_MODEL_PROFILE.
        std::env::set_var("LOOM_MODEL_PROFILE", "zai-full");
        assert_eq!(
            profile_rejects_effort("opencode", None, pin, "medium", &none),
            Some("zai-full".to_string())
        );
        std::env::remove_var("LOOM_MODEL_PROFILE");

        // Bare, empty-half and absent models fall through to today's behaviour.
        for model in [
            Some("glm-5.3-flash"),
            Some("google/"),
            Some("/gemini"),
            Some(""),
            None,
        ] {
            assert_eq!(
                profile_rejects_effort("opencode", None, model, "medium", &none),
                Some("zai-flash".to_string()),
                "model {model:?} is not a qualified pin"
            );
        }

        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
    }

    /// Issue #14962: `profile_rejects_model` mirrors `select`'s
    /// model-mismatch refusal predicate on `excluded_role`'s exact lookup
    /// path, so the daemon's launch sites can drop a *configured-tier* model
    /// instead of losing the step to a `profile_selection` refusal. The full
    /// drop-vs-keep precedence lives with the resolver
    /// (`role_runner/tests/model_resolution.rs`); these tests pin the
    /// predicate itself.
    #[test]
    fn profile_rejects_model_matches_selects_model_predicate() {
        let _g = env_lock();
        let saved = std::env::var_os("LOOM_MODEL_PROFILE");
        std::env::remove_var("LOOM_MODEL_PROFILE");
        let config = serde_json::json!({"runtimes":{"modelProfiles":{
            "strict": {"model": "glm-5.3", "providers": {"opencode": "zai-coding-plan"}},
            "other-harness": {"model": "glm-5.3", "providers": {"pi": "zai"}}
        }}});

        // The reported incident's exact shape: a Claude-shaped model meeting
        // the native profile the draw pinned.
        assert_eq!(
            profile_rejects_model("opencode", Some("strict"), "claude-opus-5", &config),
            Some("strict".to_string())
        );
        // The profile's own model is no objection.
        assert_eq!(profile_rejects_model("opencode", Some("strict"), "glm-5.3", &config), None);
        // A qualified provider/model that does not literally equal the
        // profile's model is the same objection (`select` compares the full
        // string once a profile is selected).
        assert_eq!(
            profile_rejects_model("opencode", Some("strict"), "zai-coding-plan/glm-5.3", &config),
            Some("strict".to_string())
        );
        // A non-native runtime is never judged (Claude/Codex forward `--model`
        // to their own CLIs, which own their vocabularies).
        assert_eq!(profile_rejects_model("claude", Some("strict"), "claude-opus-5", &config), None);
        // A profile with no provider entry for this harness cannot be the
        // model that harness runs.
        assert_eq!(
            profile_rejects_model("opencode", Some("other-harness"), "claude-opus-5", &config),
            None
        );
        // An unknown profile is left to the launch's own fail-closed
        // diagnostic, never judged here.
        assert_eq!(profile_rejects_model("opencode", Some("nope"), "claude-opus-5", &config), None);

        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
    }

    /// Issue #14962: a bare native runtime (a preference tap with no
    /// `modelProfile`) is judged against the profile the launch would
    /// actually select — the `LOOM_MODEL_PROFILE` / `defaultModelProfile` /
    /// bundled `zai-flash` fallback chain, exactly as `excluded_role` and
    /// `profile_rejects_effort` judge it. The bundled `zai-flash` runs
    /// `glm-5.3-flash`, so the incident's `claude-opus-5` meets it too.
    #[test]
    fn profile_rejects_model_falls_back_to_the_launchs_own_profile() {
        let _g = env_lock();
        let saved = std::env::var_os("LOOM_MODEL_PROFILE");

        std::env::remove_var("LOOM_MODEL_PROFILE");
        assert_eq!(
            profile_rejects_model("opencode", None, "claude-opus-5", &serde_json::json!({})),
            Some("zai-flash".to_string()),
            "the bundled zai-flash fallback rejects the incident's model"
        );
        assert_eq!(
            profile_rejects_model("opencode", None, "glm-5.3-flash", &serde_json::json!({})),
            None,
            "the bundled zai-flash's own model (glm-5.3-flash) is no objection"
        );
        // `select` honours a qualified `provider/model` verbatim when no
        // profile was requested (it never consults a profile), so it is no
        // objection — the documented `roleModels` shape.
        assert_eq!(
            profile_rejects_model(
                "opencode",
                None,
                "google/gemini-2.5-pro",
                &serde_json::json!({})
            ),
            None,
            "a qualified pin on a bare pick is honoured by select"
        );
        // Malformed halves are not the early-return shape; they fall through.
        assert_eq!(
            profile_rejects_model("opencode", None, "google/", &serde_json::json!({})),
            Some("zai-flash".to_string())
        );
        // An explicitly requested profile still refuses a differing qualified pin.
        assert_eq!(
            profile_rejects_model(
                "opencode",
                Some("zai-flash"),
                "google/gemini-2.5-pro",
                &serde_json::json!({})
            ),
            Some("zai-flash".to_string())
        );

        std::env::set_var("LOOM_MODEL_PROFILE", "zai-full");
        assert_eq!(
            profile_rejects_model("opencode", None, "claude-opus-5", &serde_json::json!({})),
            Some("zai-full".to_string()),
            "LOOM_MODEL_PROFILE names the profile a bare tap is judged against"
        );

        std::env::remove_var("LOOM_MODEL_PROFILE");
        let config = serde_json::json!({"runtimes":{"defaultModelProfile":"zai-full", "modelProfiles":{
            "zai-full": {"model": "glm-5.3", "providers": {"opencode": "zai-coding-plan"}}
        }}});
        assert_eq!(
            profile_rejects_model("opencode", None, "claude-opus-5", &config),
            Some("zai-full".to_string()),
            "runtimes.defaultModelProfile names the profile a bare tap is judged against"
        );

        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
    }

    // ===== runtimes.modelProfileOverrides (#15252) =====

    fn with_no_profile_env<T>(f: impl FnOnce() -> T) -> T {
        let _g = env_lock();
        let saved = std::env::var_os("LOOM_MODEL_PROFILE");
        std::env::remove_var("LOOM_MODEL_PROFILE");
        let out = f();
        if let Some(v) = saved {
            std::env::set_var("LOOM_MODEL_PROFILE", v);
        }
        out
    }

    fn overrides(value: Value) -> Value {
        serde_json::json!({"runtimes": {"modelProfileOverrides": value}})
    }

    #[test]
    fn an_override_changes_the_model_and_keeps_every_other_bundled_field() {
        with_no_profile_env(|| {
            let config = overrides(serde_json::json!({"zai-full": {
                "model": "glm-5.4", "effort": "high", "allowedEfforts": ["high", "max"]
            }}));
            let (name, got) = lookup(Some("zai-full"), &config).unwrap();
            let bundled = bundled().remove("zai-full").unwrap();
            assert_eq!(name, "zai-full");
            assert_eq!(got.model, "glm-5.4");
            assert_eq!(got.effort.as_deref(), Some("high"));
            assert_eq!(got.allowed_efforts, ["high", "max"]);
            // Everything not overridable is inherited verbatim.
            assert_eq!(got.excluded_roles, ["judge", "curator"]);
            assert_eq!(got.providers, bundled.providers);
            assert_eq!(got.billing, bundled.billing);
            assert_eq!(got.credential_pool, bundled.credential_pool);
            assert_eq!(got.credential_targets.len(), bundled.credential_targets.len());
            assert_eq!(
                got.credential_proxy.as_ref().map(|p| format!("{p:?}")),
                bundled.credential_proxy.as_ref().map(|p| format!("{p:?}"))
            );
            // select() launches the overridden model.
            let options = Options {
                profile: Some("zai-full".into()),
                ..Options::default()
            };
            let selection = select("opencode", &options, &config).unwrap();
            assert_eq!(selection.model, "glm-5.4");
        });
    }

    #[test]
    fn an_override_never_lifts_the_bundled_excluded_roles() {
        with_no_profile_env(|| {
            let config = overrides(serde_json::json!({"zai-full": {"model": "glm-5.4"}}));
            for role in ["judge", "curator"] {
                assert_eq!(
                    excluded_role("opencode", Some("zai-full"), role, &config).as_deref(),
                    Some("zai-full"),
                    "{role} must stay excluded"
                );
            }
            assert_eq!(excluded_role("opencode", Some("zai-full"), "builder", &config), None);
        });
    }

    #[test]
    fn excluded_roles_add_extends_and_dedupes_by_canonical_role() {
        with_no_profile_env(|| {
            let config = overrides(serde_json::json!({"zai-full": {
                "excludedRolesAdd": ["doctor", "judge", "Issue_Curator", "doctor"]
            }}));
            let (_, got) = lookup(Some("zai-full"), &config).unwrap();
            assert_eq!(got.excluded_roles, ["judge", "curator", "doctor"]);
            assert_eq!(
                excluded_role("opencode", Some("zai-full"), "doctor", &config).as_deref(),
                Some("zai-full")
            );
        });
    }

    #[test]
    fn an_empty_override_and_explicit_nulls_are_no_ops() {
        with_no_profile_env(|| {
            for value in [
                serde_json::json!({"zai-full": {}}),
                serde_json::json!({"zai-full": null}),
                serde_json::json!({"zai-full": {"model": null, "effort": null,
                    "allowedEfforts": null, "excludedRolesAdd": null}}),
                serde_json::json!(null),
            ] {
                let config = overrides(value.clone());
                assert!(check_model_profile_overrides_config(&config).is_empty(), "{value}");
                let (_, got) = lookup(Some("zai-full"), &config).unwrap();
                assert_eq!(got.model, "glm-5.3", "{value}");
                assert_eq!(got.excluded_roles, ["judge", "curator"], "{value}");
            }
        });
    }

    /// #15268: the effective (override, else bundled) `effort` must be in the
    /// effective `allowedEfforts`, the predicate `select` refuses a launch on.
    #[test]
    fn an_override_leaving_effort_outside_allowed_efforts_is_rejected_by_validate_and_launch() {
        with_no_profile_env(|| {
            // Bundled `zai-full`: effort "max", allowedEfforts [low, high, max].
            let bad = [
                (serde_json::json!({"effort": "medium"}), "`medium`", "[low, high, max]"),
                (serde_json::json!({"allowedEfforts": ["low", "high"]}), "`max`", "[low, high]"),
                (
                    serde_json::json!({"effort": "high", "allowedEfforts": ["low", "max"]}),
                    "`high`",
                    "[low, max]",
                ),
            ];
            for (fields, effort, allowed) in bad {
                let config = overrides(serde_json::json!({"zai-full": fields.clone()}));
                let findings = check_model_profile_overrides_config(&config);
                assert_eq!(findings.len(), 1, "{fields}: {findings:?}");
                for part in ["zai-full", effort, allowed] {
                    assert!(findings[0].contains(part), "{fields}: {findings:?}");
                }
                // The same config makes `select` refuse an unqualified launch
                // (via `lookup`, which re-runs the check, for any profile).
                assert!(lookup(Some("zai-full"), &config).is_err(), "{fields}");
            }
        });
    }

    #[test]
    fn consistent_noop_and_empty_allowed_efforts_overrides_report_no_effort_finding() {
        with_no_profile_env(|| {
            for value in [
                // Both fields set, consistently.
                serde_json::json!({"zai-full": {"effort": "medium",
                    "allowedEfforts": ["low", "medium"]}}),
                // Only a field that stays consistent with the bundled sibling.
                serde_json::json!({"zai-full": {"allowedEfforts": ["max"]}}),
                serde_json::json!({"zai-full": {"effort": "low"}}),
                // Neither, or only `model`.
                serde_json::json!({"zai-full": {}}),
                serde_json::json!({"zai-full": {"model": "glm-5.4"}}),
                // An empty effective allowedEfforts never rejects an effort.
                serde_json::json!({"zai-full": {"allowedEfforts": []}}),
                serde_json::json!({"zai-full": {"effort": "medium", "allowedEfforts": []}}),
                // `null` falls back to the bundled value.
                serde_json::json!({"zai-full": {"effort": null, "allowedEfforts": null}}),
            ] {
                let config = overrides(value.clone());
                assert!(check_model_profile_overrides_config(&config).is_empty(), "{value}");
                assert!(lookup(Some("zai-full"), &config).is_ok(), "{value}");
            }
        });
    }

    #[test]
    fn a_non_bundled_override_gets_no_extra_effort_finding() {
        with_no_profile_env(|| {
            let config = overrides(serde_json::json!({
                "not-bundled": {"effort": "medium", "allowedEfforts": ["low"]}
            }));
            let findings = check_model_profile_overrides_config(&config);
            assert_eq!(findings.len(), 1, "{findings:?}");
            assert!(findings[0].contains("not a bundled model profile"), "{findings:?}");
        });
    }

    #[test]
    fn an_override_applies_to_the_default_profile_too() {
        with_no_profile_env(|| {
            let config = serde_json::json!({"runtimes": {
                "defaultModelProfile": "zai-full",
                "modelProfileOverrides": {"zai-full": {"model": "glm-5.4"}}
            }});
            assert_eq!(lookup(None, &config).unwrap().1.model, "glm-5.4");
            // A profile with no override is untouched.
            assert_eq!(lookup(Some("zai-flash"), &config).unwrap().1.model, "glm-5.3-flash");
        });
    }

    #[test]
    fn invalid_overrides_are_rejected_by_validate_and_by_lookup() {
        with_no_profile_env(|| {
            let cases = [
                (serde_json::json!({"zai-full": {"providers": {}}}), "unknown override key"),
                (serde_json::json!({"zai-full": {"billing": "api"}}), "unknown override key"),
                (
                    serde_json::json!({"zai-full": {"excludedRoles": []}}),
                    "never replaced or reduced",
                ),
                (
                    serde_json::json!({"zai-full": {"excludedRolesAdd": ["wizard"]}}),
                    "unknown role name(s): wizard",
                ),
                (
                    serde_json::json!({"zai-full": {"excludedRolesAdd": "judge"}}),
                    "excludedRolesAdd must be an array",
                ),
                (serde_json::json!({"zai-full": {"model": "  "}}), "model must be a non-blank"),
                (serde_json::json!({"zai-full": {"model": 5}}), "model must be a non-blank"),
                (serde_json::json!({"zai-full": {"effort": ""}}), "effort must be a non-blank"),
                (
                    serde_json::json!({"zai-full": {"allowedEfforts": ["high", " "]}}),
                    "allowedEfforts must be an array of non-blank",
                ),
                (serde_json::json!({"zai-full": "glm-5.4"}), "must be an object"),
                (
                    serde_json::json!({"not-bundled": {"model": "m"}}),
                    "not a bundled model profile",
                ),
            ];
            for (value, expect) in cases {
                let config = overrides(value.clone());
                let findings = check_model_profile_overrides_config(&config);
                assert!(findings.iter().any(|f| f.contains(expect)), "{value}: {findings:?}");
                // Launch refuses too (for any profile, so a bad config cannot half-apply).
                let message = lookup(Some("zai-flash"), &config)
                    .expect_err("a malformed override must fail closed")
                    .message;
                assert!(message.contains(expect), "{value}: {message}");
            }
            let not_object = overrides(serde_json::json!(["zai-full"]));
            assert_eq!(check_model_profile_overrides_config(&not_object).len(), 1);
        });
    }

    #[test]
    fn a_name_in_both_model_profiles_and_overrides_is_rejected() {
        with_no_profile_env(|| {
            let both = serde_json::json!({"runtimes": {
                "modelProfiles": {"zai-full": {"model": "mine", "providers": {}}},
                "modelProfileOverrides": {"zai-full": {"model": "glm-5.4"}}
            }});
            let findings = check_model_profile_overrides_config(&both);
            assert!(findings.iter().any(|f| f.contains("also defined under")), "{findings:?}");
            assert!(lookup(Some("zai-full"), &both).is_err());
            // A null override (a higher tier clearing it) is not a collision.
            let cleared = serde_json::json!({"runtimes": {
                "modelProfiles": {"zai-full": {"model": "mine", "providers": {}}},
                "modelProfileOverrides": {"zai-full": null}
            }});
            assert!(check_model_profile_overrides_config(&cleared).is_empty());
            assert_eq!(lookup(Some("zai-full"), &cleared).unwrap().1.model, "mine");
        });
    }
}
