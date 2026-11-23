//! Per-account limits an operator *declares* for an API-key account: a
//! concurrency cap (#8424 item 4), plus the plan's quota window and token
//! allowance (#11286) — configured, not probed, because no provider in scope
//! offers a plan-safe usage probe.
//!
//! # Why a sibling file and not the account's own `.env`
//!
//! An account file is exactly one `KEY=value` assignment, and the registry's
//! "file holds more than one assignment; one account is one key" refusal is
//! load-bearing (it is what makes a key file unambiguous). So the cap lives
//! beside the key, in one per-provider
//! JSON file — the same shape as `.disabled` / `.allowlist` /
//! `.bad_accounts.json`, written by the same atomic
//! [`super::registry::write_secret`] under the same `.control.lock`:
//!
//! ```json
//! { "alpha": { "maxConcurrent": 2, "exhaustionWindowSecs": 604800, "planTokenLimit": 50000000 } }
//! ```
//!
//! One file per provider rather than one per account so selection pays a
//! single read for the whole candidate set.
//!
//! # Absent means unbounded; unreadable means unknown
//!
//! A missing file, or an account with no entry, is **unbounded** — every
//! account registered before #8424 keeps behaving exactly as it did. A file
//! that exists but cannot be read or parsed is an **error**, and the selection
//! ladder turns that into [`super::registry::Ineligible::Unverifiable`]:
//! "I could not read the caps" must never read as "there are none", for the
//! same reason an unreadable `.disabled` must never read as "nothing is
//! disabled".
//!
//! # A second source: `runtimes.apiKeyLimits` (#14932)
//!
//! A cap on a metered key is a spend governor, and a governor that exists only
//! as a hand step on each host can be neither reviewed nor reproduced, and is
//! gone when the host is rebuilt. So the workspace's resolved config declares
//! limits too, under `runtimes.apiKeyLimits.<provider>.<account>`:
//!
//! ```json
//! { "zai": { "alice": { "maxConcurrent": 1, "exhaustionWindowSecs": 604800 } } }
//! ```
//!
//! [`effective_limits`] merges the two sources per account and tags each
//! limit with its [`LimitSource`]. With both declaring a limit,
//! `maxConcurrent` takes the **stricter** (smaller) value — config can tighten
//! a host-local cap and never loosen it — and `exhaustionWindowSecs` takes
//! config's. `planTokenLimit` is not configurable.
//!
//! The config belongs to the **calling workspace**, not to the pool directory:
//! a shared machine-level pool account used from a repo is capped by that
//! repo's config. It is resolved **once** per command or spawn — resolving
//! reads every config tier — into a [`ConfiguredLimits`] that is threaded down
//! to the readers; nothing below it reads config per account.
//!
//! Unreadable means unknown on this side as well. A block that cannot be
//! understood (`0`, a wrong type, an unknown key, a name that is not a slug)
//! withholds the affected provider's accounts as
//! [`super::registry::Ineligible::Unverifiable`], exactly as an unreadable
//! `.limits.json` does: a typo must never read as "no cap".

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::tokens_pool::locking::MkdirLock;

use super::paths::{provider_dir, validate_account, validate_provider};
use super::registry::{restrict_dir, write_secret};

/// Per-provider file holding declared per-account limits.
pub const LIMITS_FILE: &str = ".limits.json";

/// Config key of the second limits source:
/// `runtimes.apiKeyLimits.<provider>.<account>` (#14932) — see the module docs.
pub const CONFIG_KEY: &str = "runtimes.apiKeyLimits";

/// The keys one account's block under [`CONFIG_KEY`] may carry.
const CONFIG_KEYS: [&str; 2] = ["maxConcurrent", "exhaustionWindowSecs"];

static NO_ACCOUNTS: BTreeMap<String, ConfiguredAccountLimits> = BTreeMap::new();

/// What an operator has declared about one account.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountLimits {
    /// Most spawns that may hold this account at once, provider-side ceiling
    /// (a Z.ai coding-plan key's concurrent-request limit). `None` is
    /// unbounded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrent: Option<u32>,
    /// The plan's quota window in seconds (#11286) — how long an exhausted
    /// seat stays out of selection when the provider's refusal names no reset
    /// instant of its own (see [`super::reset`]). `None` keeps the 6 h
    /// default. A weekly-capped Z.ai seat is `604800`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exhaustion_window_secs: Option<u64>,
    /// The plan's token allowance per [`Self::exhaustion_window_secs`]
    /// (#11286). Exported as the `loom.pool.plan_token_limit` gauge so the
    /// dashboard can divide `loom.llm.tokens.*` by it; nothing here enforces
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_token_limit: Option<u64>,
}

impl AccountLimits {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.max_concurrent.is_none()
            && self.exhaustion_window_secs.is_none()
            && self.plan_token_limit.is_none()
    }
}

/// Where the pool got one effective limit value (#14932).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitSource {
    /// A source could not be read, so the limit is unknown and the account is
    /// withheld — never "unbounded".
    Unknown,
    /// Neither source declares the limit.
    #[default]
    Absent,
    /// Only the host's `.limits.json` declares it.
    File,
    /// Only [`CONFIG_KEY`] declares it.
    Config,
    /// Both declare it and the config's value is in effect: the stricter cap,
    /// a cap equal to the file's, or — for the plan window, where config
    /// always wins — any value.
    BothConfig,
    /// Both declare a cap and the file's is in effect: it is the stricter one.
    BothFile,
}

impl LimitSource {
    /// The provenance as `api-keys list` prints it.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Absent => "absent",
            Self::File => "file",
            Self::Config => "config",
            Self::BothConfig => "both, config wins",
            Self::BothFile => "both, file wins",
        }
    }
}

/// One account's limits as the pool enforces them: `.limits.json` merged
/// with [`CONFIG_KEY`], each limit tagged with where it came from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EffectiveLimits {
    pub limits: AccountLimits,
    pub max_concurrent_source: LimitSource,
    pub exhaustion_window_secs_source: LimitSource,
    pub plan_token_limit_source: LimitSource,
}

impl EffectiveLimits {
    fn merged(file: AccountLimits, config: ConfiguredAccountLimits) -> Self {
        let (max_concurrent, max_concurrent_source) =
            merge_cap(file.max_concurrent, config.max_concurrent);
        let (exhaustion_window_secs, exhaustion_window_secs_source) =
            merge_window(file.exhaustion_window_secs, config.exhaustion_window_secs);
        let plan_token_limit_source = if file.plan_token_limit.is_some() {
            LimitSource::File
        } else {
            LimitSource::Absent
        };
        Self {
            limits: AccountLimits {
                max_concurrent,
                exhaustion_window_secs,
                plan_token_limit: file.plan_token_limit,
            },
            max_concurrent_source,
            exhaustion_window_secs_source,
            plan_token_limit_source,
        }
    }
}

/// The stricter (smaller) cap wins, so config can never loosen a hand-set
/// host-local cap. A tie is the config's: its value is the reproducible one.
fn merge_cap(file: Option<u32>, config: Option<u32>) -> (Option<u32>, LimitSource) {
    match (file, config) {
        (None, None) => (None, LimitSource::Absent),
        (Some(value), None) => (Some(value), LimitSource::File),
        (None, Some(value)) => (Some(value), LimitSource::Config),
        (Some(file), Some(config)) if config <= file => (Some(config), LimitSource::BothConfig),
        (Some(file), Some(_)) => (Some(file), LimitSource::BothFile),
    }
}

/// The plan window is config's whenever config declares one: it is the
/// fleet's declared plan, and the file's is a host's guess at it.
fn merge_window(file: Option<u64>, config: Option<u64>) -> (Option<u64>, LimitSource) {
    match (file, config) {
        (None, None) => (None, LimitSource::Absent),
        (Some(value), None) => (Some(value), LimitSource::File),
        (None, Some(value)) => (Some(value), LimitSource::Config),
        (Some(_), Some(value)) => (Some(value), LimitSource::BothConfig),
    }
}

/// What one account's block under [`CONFIG_KEY`] may declare — exactly the
/// two limits a config store has a say in. `planTokenLimit` is deliberately
/// not among them: it feeds a dashboard gauge, not selection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ConfiguredAccountLimits {
    max_concurrent: Option<u32>,
    exhaustion_window_secs: Option<u64>,
}

impl ConfiguredAccountLimits {
    fn is_empty(&self) -> bool {
        self.max_concurrent.is_none() && self.exhaustion_window_secs.is_none()
    }
}

/// One provider's entry under [`CONFIG_KEY`].
#[derive(Clone, Debug, PartialEq, Eq)]
enum Declared {
    /// Limits by account name. Empty when the entry is `null` or `{}` —
    /// `null` is how a higher config tier clears a lower one's block.
    Accounts(BTreeMap<String, ConfiguredAccountLimits>),
    /// The entry cannot be understood, so every account of the provider is
    /// withheld until the config is repaired.
    Malformed(String),
}

/// The [`CONFIG_KEY`] block of one workspace's resolved config, parsed **once**
/// per command or spawn and threaded to every limits read — see the module
/// docs for why it belongs to the caller rather than to the pool directory.
///
/// Parsing never fails: an entry that cannot be understood is carried as its
/// offence, so every reader it reaches withholds rather than guesses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfiguredLimits {
    /// Set when the block as a whole cannot be understood. It then withholds
    /// every pooled provider, because the offence — a wrong shape, a
    /// provider name that is not a slug — names no provider to scope it to.
    block_error: Option<String>,
    providers: BTreeMap<String, Declared>,
}

impl ConfiguredLimits {
    /// No config-declared limits: the file alone decides, exactly as before
    /// #14932.
    pub const EMPTY: Self = Self {
        block_error: None,
        providers: BTreeMap::new(),
    };

    /// Parse the block out of a workspace's already-resolved effective
    /// config. An absent or `null` block is [`Self::EMPTY`].
    #[must_use]
    pub fn from_config(config: &serde_json::Value) -> Self {
        match crate::config_resolver::get_path(config, CONFIG_KEY) {
            None | Some(serde_json::Value::Null) => Self::EMPTY,
            Some(block) => Self::parse_block(block),
        }
    }

    /// Resolve `workspace`'s effective config and parse its block. For a
    /// caller that has no config in hand; one that already resolved it
    /// passes the value to [`Self::from_config`] instead.
    #[must_use]
    pub fn from_workspace(workspace: &Path) -> Self {
        Self::from_config(&crate::config_resolver::resolve_effective_config(workspace))
    }

    /// `true` when nothing is declared and nothing is malformed, so every
    /// read is the file's alone.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.block_error.is_none()
            && self.providers.values().all(
                |declared| matches!(declared, Declared::Accounts(accounts) if accounts.is_empty()),
            )
    }

    /// Every offence in the block, for `loom-daemon validate` — the
    /// counterpart of `runtime_preference::ceiling::check_config`.
    #[must_use]
    pub fn check_config(config: &serde_json::Value) -> Vec<String> {
        let parsed = Self::from_config(config);
        parsed
            .block_error
            .clone()
            .into_iter()
            .chain(
                parsed
                    .providers
                    .into_values()
                    .filter_map(|declared| match declared {
                        Declared::Malformed(offence) => Some(offence),
                        Declared::Accounts(_) => None,
                    }),
            )
            .collect()
    }

    /// This provider's config-declared limits by account, or the reason they
    /// are unknowable, phrased for the account it withholds.
    fn accounts(
        &self,
        provider: &str,
    ) -> Result<&BTreeMap<String, ConfiguredAccountLimits>, String> {
        let unknown = |offence: &str| {
            format!(
                "{offence}; per-account limits for provider {provider:?} are unknown. Repair the \
                 config (`loom-daemon validate` names the key), or remove {CONFIG_KEY} to drop \
                 every config-declared cap"
            )
        };
        if let Some(offence) = &self.block_error {
            return Err(unknown(offence));
        }
        match self.providers.get(provider) {
            Some(Declared::Accounts(accounts)) => Ok(accounts),
            Some(Declared::Malformed(offence)) => Err(unknown(offence)),
            None => Ok(&NO_ACCOUNTS),
        }
    }

    fn parse_block(block: &serde_json::Value) -> Self {
        let serde_json::Value::Object(providers) = block else {
            return Self::block_offence(format!(
                "{CONFIG_KEY} must be an object of <provider> -> <account> -> {{{}}}, e.g. \
                 {CONFIG_KEY}.zai.alice.maxConcurrent, but it is {}",
                CONFIG_KEYS.join(", "),
                kind(block)
            ));
        };
        let mut parsed = BTreeMap::new();
        for (provider, accounts) in providers {
            // Scoped to no provider: a key that is not a slug names no pool
            // directory, so the cap it declares would bind nothing and read
            // as "no cap" — the one outcome this parser must never produce.
            if validate_provider(provider).is_err() {
                return Self::block_offence(format!(
                    "{CONFIG_KEY}.{provider:?}: provider name must be lowercase \
                     [a-z0-9][a-z0-9_-]* (it names a pool directory)"
                ));
            }
            let declared = match parse_provider(provider, accounts) {
                Ok(accounts) => Declared::Accounts(accounts),
                Err(offence) => Declared::Malformed(offence),
            };
            parsed.insert(provider.clone(), declared);
        }
        Self {
            block_error: None,
            providers: parsed,
        }
    }

    fn block_offence(offence: String) -> Self {
        Self {
            block_error: Some(offence),
            providers: BTreeMap::new(),
        }
    }
}

fn parse_provider(
    provider: &str,
    accounts: &serde_json::Value,
) -> Result<BTreeMap<String, ConfiguredAccountLimits>, String> {
    let key = format!("{CONFIG_KEY}.{provider}");
    if accounts.is_null() {
        return Ok(BTreeMap::new());
    }
    let serde_json::Value::Object(entries) = accounts else {
        return Err(format!(
            "{key} must be an object of <account> -> {{maxConcurrent, exhaustionWindowSecs}}, but \
             it is {}",
            kind(accounts)
        ));
    };
    let mut parsed = BTreeMap::new();
    for (account, limits) in entries {
        let key = format!("{key}.{account}");
        validate_account(account).map_err(|problem| format!("{key}: {problem}"))?;
        let limits = parse_account(&key, limits)?;
        // An entry that declares nothing (`null`, `{}`) is absent, not a
        // named account with no limits: a higher tier clears a lower one's
        // entry for one account exactly this way.
        if !limits.is_empty() {
            parsed.insert(account.clone(), limits);
        }
    }
    Ok(parsed)
}

fn parse_account(key: &str, limits: &serde_json::Value) -> Result<ConfiguredAccountLimits, String> {
    // `null` clears one account's entry the same way it clears a provider's.
    if limits.is_null() {
        return Ok(ConfiguredAccountLimits::default());
    }
    let serde_json::Value::Object(fields) = limits else {
        return Err(format!(
            "{key} must be an object with keys {} (or `null` to declare nothing), but it is {}",
            CONFIG_KEYS.join(", "),
            kind(limits)
        ));
    };
    let unknown: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|field| !CONFIG_KEYS.contains(field))
        .collect();
    if !unknown.is_empty() {
        return Err(format!(
            "{key} has unknown key(s): {} (known: {})",
            unknown.join(", "),
            CONFIG_KEYS.join(", ")
        ));
    }
    Ok(ConfiguredAccountLimits {
        max_concurrent: positive(
            fields.get("maxConcurrent"),
            key,
            "maxConcurrent",
            u32::MAX.into(),
        )?
        .and_then(|n| u32::try_from(n).ok()),
        exhaustion_window_secs: positive(
            fields.get("exhaustionWindowSecs"),
            key,
            "exhaustionWindowSecs",
            u64::MAX,
        )?,
    })
}

/// A declared limit: absent, `null`, or a positive integer no larger than
/// `max`. `0` is refused for the same reason [`set_max_concurrent`] refuses
/// it, and nothing about a rejected value is echoed — it is config text, and
/// config files are where a pasted secret is most likely to turn up.
fn positive(
    raw: Option<&serde_json::Value>,
    key: &str,
    field: &str,
    max: u64,
) -> Result<Option<u64>, String> {
    let Some(raw) = raw.filter(|raw| !raw.is_null()) else {
        return Ok(None);
    };
    if raw.as_u64() == Some(0) {
        return Err(format!(
            "{key}.{field} must be > 0 (use `api-keys disable` to take an account out of \
             selection entirely)"
        ));
    }
    raw.as_u64()
        .filter(|n| *n <= max)
        .map(Some)
        .ok_or_else(|| format!("{key}.{field} must be a positive integer, but it is {}", kind(raw)))
}

/// The JSON kind of a value, for a message that describes a mistake without
/// repeating the mistaken text.
fn kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(n) if n.is_u64() => "an integer too large for this field",
        serde_json::Value::Number(_) => "not a positive whole number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn limits_path(root: &Path, provider: &str) -> std::path::PathBuf {
    provider_dir(root, provider).join(LIMITS_FILE)
}

/// Every declared limit for `provider`, keyed by account name.
///
/// The host's `.limits.json` alone: what [`effective_limits`] merges with the
/// config block, and what the writers below read-modify-write.
///
/// # Errors
/// An absent file is an empty map. A file that exists but cannot be read or
/// parsed is an error — see the module docs.
pub fn read_limits(root: &Path, provider: &str) -> Result<BTreeMap<String, AccountLimits>, String> {
    let path = limits_path(root, provider);
    let body = match std::fs::read_to_string(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(format!("cannot read {} ({:?})", path.display(), e.kind())),
    };
    serde_json::from_str(&body).map_err(|e| {
        format!(
            "{} is not a valid limits file ({:?} error at line {}, column {}); per-account \
             concurrency caps for provider {provider:?} are unknown. Repair the JSON, or delete \
             the file to drop every declared cap for this provider",
            path.display(),
            e.classify(),
            e.line(),
            e.column()
        )
    })
}

/// Every limit the pool **enforces** for `provider`, keyed by account name:
/// `.limits.json` merged with `configured`'s [`CONFIG_KEY`] block (#14932), so
/// selection, health and the reset window all read one answer. An account
/// only the config names is listed too; one neither names is not, and reads
/// as unbounded.
///
/// # Errors
/// Either source unreadable: an unparseable `.limits.json`, or a config entry
/// for this provider that cannot be understood. Both, when both are broken.
pub fn effective_limits(
    root: &Path,
    provider: &str,
    configured: &ConfiguredLimits,
) -> Result<BTreeMap<String, EffectiveLimits>, String> {
    let (file, config) = match (read_limits(root, provider), configured.accounts(provider)) {
        (Ok(file), Ok(config)) => (file, config),
        (file, config) => {
            return Err([file.err(), config.err()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" "));
        }
    };
    let mut names: BTreeSet<String> = file.keys().cloned().collect();
    names.extend(config.keys().cloned());
    Ok(names
        .into_iter()
        .map(|name| {
            let file = file.get(&name).cloned().unwrap_or_default();
            let config = config.get(&name).cloned().unwrap_or_default();
            (name, EffectiveLimits::merged(file, config))
        })
        .collect())
}

/// `name`'s declared concurrency cap, or `None` for unbounded.
///
/// The file's alone — see [`effective_limits`] for the cap the pool enforces.
///
/// # Errors
/// As [`read_limits`].
pub fn max_concurrent(root: &Path, provider: &str, name: &str) -> Result<Option<u32>, String> {
    Ok(read_limits(root, provider)?
        .get(name)
        .and_then(|l| l.max_concurrent))
}

/// Declare (or clear, with `None`) `name`'s concurrency cap.
///
/// `Some(0)` is refused: a zero cap means "never selectable", which is what
/// `api-keys disable` is for — expressing it as a cap would hide a disabled
/// account behind a number nothing reports as disabled.
///
/// # Errors
/// An unusable provider directory, an unreadable existing limits file (never
/// rewritten from a failed read, which would silently drop every other
/// account's cap), or a lock that cannot be taken.
pub fn set_max_concurrent(
    root: &Path,
    provider: &str,
    name: &str,
    cap: Option<u32>,
) -> Result<(), String> {
    if cap == Some(0) {
        return Err("maxConcurrent must be > 0 (use `api-keys disable` to take an account out of \
             selection entirely)"
            .to_string());
    }
    update(root, provider, name, cap.is_some(), |limits| limits.max_concurrent = cap)
}

/// Declare (or clear, with `None`) `name`'s plan window — see
/// [`AccountLimits::exhaustion_window_secs`] (#11286).
///
/// # Errors
/// `Some(0)` (a window that ends the instant it starts), plus everything
/// [`set_max_concurrent`] can fail on.
pub fn set_exhaustion_window(
    root: &Path,
    provider: &str,
    name: &str,
    secs: Option<u64>,
) -> Result<(), String> {
    if secs == Some(0) {
        return Err("exhaustionWindowSecs must be > 0".to_string());
    }
    update(root, provider, name, secs.is_some(), |limits| {
        limits.exhaustion_window_secs = secs;
    })
}

/// Declare (or clear, with `None`) `name`'s plan token allowance — see
/// [`AccountLimits::plan_token_limit`] (#11286).
///
/// # Errors
/// `Some(0)`, plus everything [`set_max_concurrent`] can fail on.
pub fn set_plan_token_limit(
    root: &Path,
    provider: &str,
    name: &str,
    tokens: Option<u64>,
) -> Result<(), String> {
    if tokens == Some(0) {
        return Err("planTokenLimit must be > 0".to_string());
    }
    update(root, provider, name, tokens.is_some(), |limits| {
        limits.plan_token_limit = tokens;
    })
}

/// Read-modify-write one account's entry under the provider's control lock.
/// `declaring` is whether the edit sets something (as opposed to clearing):
/// clearing on a provider that has no directory yet is a no-op, not a
/// `mkdir`.
fn update(
    root: &Path,
    provider: &str,
    name: &str,
    declaring: bool,
    edit: impl FnOnce(&mut AccountLimits),
) -> Result<(), String> {
    validate_provider(provider)?;
    validate_account(name)?;
    let dir = provider_dir(root, provider);
    if !dir.exists() {
        if !declaring {
            return Ok(());
        }
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        restrict_dir(&dir);
    }
    let _lock = MkdirLock::acquire(&dir.join(".control.lock"))
        .map_err(|e| format!("cannot lock {}: {e}", limits_path(root, provider).display()))?;
    let mut limits = read_limits(root, provider)?;
    edit(limits.entry(name.to_string()).or_default());
    limits.retain(|_, l| !l.is_empty());
    write_limits(root, provider, &limits)
}

/// Drop `name`'s limits entirely — called by `registry::remove` so a name
/// re-registered later does not inherit a stale cap.
pub(super) fn forget(root: &Path, provider: &str, name: &str) -> Result<(), String> {
    if !limits_path(root, provider).exists() {
        return Ok(());
    }
    let dir = provider_dir(root, provider);
    let _lock = MkdirLock::acquire(&dir.join(".control.lock"))
        .map_err(|e| format!("cannot lock {}: {e}", limits_path(root, provider).display()))?;
    let mut limits = read_limits(root, provider)?;
    if limits.remove(name).is_none() {
        return Ok(());
    }
    write_limits(root, provider, &limits)
}

fn write_limits(
    root: &Path,
    provider: &str,
    limits: &BTreeMap<String, AccountLimits>,
) -> Result<(), String> {
    let path = limits_path(root, provider);
    if limits.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("cannot update {}: {e}", path.display()))?;
        }
        return Ok(());
    }
    let body = serde_json::to_string_pretty(limits).map_err(|e| e.to_string())?;
    write_secret(&path, &format!("{body}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(names: &[&str]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        for name in names {
            super::super::registry::add(tmp.path(), "zai", name, "ZAI_API_KEY", "fake-key", false)
                .unwrap();
        }
        tmp
    }

    #[test]
    fn an_account_with_no_declared_cap_is_unbounded() {
        let tmp = pool(&["alpha"]);
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        assert_eq!(read_limits(tmp.path(), "zai").unwrap(), BTreeMap::new());
    }

    #[test]
    fn a_declared_cap_round_trips_and_clears() {
        let tmp = pool(&["alpha", "beta"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(3)).unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), Some(3));
        assert_eq!(max_concurrent(tmp.path(), "zai", "beta").unwrap(), None);
        let body = std::fs::read_to_string(limits_path(tmp.path(), "zai")).unwrap();
        assert!(body.contains("\"maxConcurrent\": 3"), "{body}");

        set_max_concurrent(tmp.path(), "zai", "alpha", None).unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        // The file is removed once nothing is declared, rather than left as
        // an empty object.
        assert!(!limits_path(tmp.path(), "zai").exists());
    }

    #[test]
    fn a_zero_cap_is_refused_in_favour_of_disable() {
        let tmp = pool(&["alpha"]);
        let err = set_max_concurrent(tmp.path(), "zai", "alpha", Some(0)).unwrap_err();
        assert!(err.contains("disable"), "{err}");
    }

    #[test]
    fn an_unparsable_limits_file_is_an_error_and_is_never_clobbered() {
        let tmp = pool(&["alpha", "beta"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(2)).unwrap();
        let path = limits_path(tmp.path(), "zai");
        for torn in ["", "{\"alpha\":", "[]"] {
            std::fs::write(&path, torn).unwrap();
            let err = read_limits(tmp.path(), "zai").unwrap_err();
            assert!(err.contains(&path.display().to_string()), "{err}");
            assert!(max_concurrent(tmp.path(), "zai", "alpha").is_err());
            assert!(set_max_concurrent(tmp.path(), "zai", "beta", Some(1)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), torn, "file was clobbered");
        }
    }

    /// #11286: the plan window and token allowance round-trip beside the cap
    /// and clear independently of it.
    #[test]
    fn the_plan_window_and_token_limit_round_trip_independently() {
        let tmp = pool(&["alpha"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(2)).unwrap();
        set_exhaustion_window(tmp.path(), "zai", "alpha", Some(604_800)).unwrap();
        set_plan_token_limit(tmp.path(), "zai", "alpha", Some(50_000_000)).unwrap();
        let got = read_limits(tmp.path(), "zai").unwrap()["alpha"].clone();
        assert_eq!(
            got,
            AccountLimits {
                max_concurrent: Some(2),
                exhaustion_window_secs: Some(604_800),
                plan_token_limit: Some(50_000_000),
            }
        );
        let body = std::fs::read_to_string(limits_path(tmp.path(), "zai")).unwrap();
        assert!(body.contains("\"exhaustionWindowSecs\": 604800"), "{body}");
        assert!(body.contains("\"planTokenLimit\": 50000000"), "{body}");

        set_max_concurrent(tmp.path(), "zai", "alpha", None).unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        assert_eq!(
            super::super::reset::configured_window(
                tmp.path(),
                "zai",
                "alpha",
                &ConfiguredLimits::EMPTY
            ),
            Some(604_800)
        );
        set_exhaustion_window(tmp.path(), "zai", "alpha", None).unwrap();
        set_plan_token_limit(tmp.path(), "zai", "alpha", None).unwrap();
        assert!(!limits_path(tmp.path(), "zai").exists());
        assert!(set_exhaustion_window(tmp.path(), "zai", "alpha", Some(0)).is_err());
        assert!(set_plan_token_limit(tmp.path(), "zai", "alpha", Some(0)).is_err());
    }

    #[test]
    fn removing_an_account_forgets_its_cap() {
        let tmp = pool(&["alpha", "beta"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(2)).unwrap();
        set_max_concurrent(tmp.path(), "zai", "beta", Some(5)).unwrap();
        super::super::registry::remove(tmp.path(), "zai", "alpha").unwrap();
        assert_eq!(max_concurrent(tmp.path(), "zai", "alpha").unwrap(), None);
        assert_eq!(max_concurrent(tmp.path(), "zai", "beta").unwrap(), Some(5));
    }

    // ---- `runtimes.apiKeyLimits`: the second source (#14932) ----

    /// The block as a workspace's resolved config carries it.
    fn configured(block: serde_json::Value) -> ConfiguredLimits {
        ConfiguredLimits::from_config(&serde_json::json!({ "runtimes": { "apiKeyLimits": block } }))
    }

    fn effective(
        tmp: &tempfile::TempDir,
        configured: &ConfiguredLimits,
    ) -> BTreeMap<String, EffectiveLimits> {
        effective_limits(tmp.path(), "zai", configured).unwrap()
    }

    /// Absent block — or one cleared with `null`, the way a higher tier
    /// clears a lower one's — is today's behaviour, exactly: the file's
    /// values, reported as the file's, and nothing else listed.
    #[test]
    fn an_absent_block_leaves_the_file_as_the_only_source() {
        let tmp = pool(&["alpha"]);
        set_max_concurrent(tmp.path(), "zai", "alpha", Some(3)).unwrap();
        let expected = EffectiveLimits {
            limits: read_limits(tmp.path(), "zai").unwrap()["alpha"].clone(),
            max_concurrent_source: LimitSource::File,
            exhaustion_window_secs_source: LimitSource::Absent,
            plan_token_limit_source: LimitSource::Absent,
        };
        for block in [
            serde_json::json!({}),
            serde_json::json!({ "zai": null }),
            serde_json::json!({ "zai": {} }),
            serde_json::json!({ "zai": { "alpha": null } }),
            serde_json::json!({ "zai": { "alpha": {} } }),
        ] {
            let limits = configured(block.clone());
            assert!(limits.is_empty(), "{block}");
            let merged = effective(&tmp, &limits);
            assert_eq!(merged.len(), 1, "{block}");
            assert_eq!(merged["alpha"], expected, "{block}");
        }
        assert_eq!(ConfiguredLimits::from_config(&serde_json::json!({})), ConfiguredLimits::EMPTY);
        assert_eq!(effective(&tmp, &ConfiguredLimits::EMPTY)["alpha"], expected);
    }

    /// No `.limits.json` at all: config alone declares, and says so.
    #[test]
    fn a_config_only_cap_and_window_are_enforced_as_config_s() {
        let tmp = pool(&["alpha"]);
        let limits = effective(
            &tmp,
            &configured(serde_json::json!({
                "zai": { "alpha": { "maxConcurrent": 1, "exhaustionWindowSecs": 604_800 } }
            })),
        );
        assert_eq!(
            limits["alpha"].limits,
            AccountLimits {
                max_concurrent: Some(1),
                exhaustion_window_secs: Some(604_800),
                plan_token_limit: None,
            }
        );
        assert_eq!(limits["alpha"].max_concurrent_source, LimitSource::Config);
        assert_eq!(limits["alpha"].exhaustion_window_secs_source, LimitSource::Config);
        assert_eq!(limits["alpha"].plan_token_limit_source, LimitSource::Absent);
        // A declaration is merged at read time, never written back to the host.
        assert!(!limits_path(tmp.path(), "zai").exists());
    }

    /// The acceptance matrix for the cap: whichever side is stricter wins, so
    /// config can tighten a host's cap and never loosen it.
    #[test]
    fn the_stricter_cap_wins_whichever_side_declares_it() {
        let tmp = pool(&["alpha"]);
        for (file, config, enforced, source) in [
            (2u32, 1u32, 1u32, LimitSource::BothConfig),
            (1, 2, 1, LimitSource::BothFile),
            (2, 2, 2, LimitSource::BothConfig),
            (9, 1, 1, LimitSource::BothConfig),
            (1, 9, 1, LimitSource::BothFile),
        ] {
            set_max_concurrent(tmp.path(), "zai", "alpha", Some(file)).unwrap();
            let limits = effective(
                &tmp,
                &configured(serde_json::json!({ "zai": { "alpha": { "maxConcurrent": config } } })),
            );
            assert_eq!(
                limits["alpha"].limits.max_concurrent,
                Some(enforced),
                "file {file}, config {config}"
            );
            assert_eq!(
                limits["alpha"].max_concurrent_source, source,
                "file {file}, config {config}"
            );
        }
    }

    /// The plan window is the config's whenever config declares one, and the
    /// file's when only the file does.
    #[test]
    fn the_config_window_wins_and_the_file_s_is_used_when_config_has_none() {
        let tmp = pool(&["alpha"]);
        let limits = configured(
            serde_json::json!({ "zai": { "alpha": { "exhaustionWindowSecs": 604_800 } } }),
        );
        let config_only = effective(&tmp, &limits);
        assert_eq!(config_only["alpha"].limits.exhaustion_window_secs, Some(604_800));
        assert_eq!(config_only["alpha"].exhaustion_window_secs_source, LimitSource::Config);

        set_exhaustion_window(tmp.path(), "zai", "alpha", Some(3_600)).unwrap();
        let both = effective(&tmp, &limits);
        assert_eq!(both["alpha"].limits.exhaustion_window_secs, Some(604_800));
        assert_eq!(both["alpha"].exhaustion_window_secs_source, LimitSource::BothConfig);

        let file_only = effective(&tmp, &ConfiguredLimits::EMPTY);
        assert_eq!(file_only["alpha"].limits.exhaustion_window_secs, Some(3_600));
        assert_eq!(file_only["alpha"].exhaustion_window_secs_source, LimitSource::File);
    }

    /// An account only the config names is listed; one neither names is not.
    #[test]
    fn the_union_of_both_sources_is_listed() {
        let tmp = pool(&["alpha", "beta"]);
        set_max_concurrent(tmp.path(), "zai", "beta", Some(4)).unwrap();
        let limits = effective(
            &tmp,
            &configured(serde_json::json!({ "zai": { "alice": { "maxConcurrent": 1 } } })),
        );
        let names: Vec<&str> = limits.keys().map(String::as_str).collect();
        // `alpha` declares nothing in either source, so it is not listed at
        // all and reads as unbounded where it is looked up.
        assert_eq!(names, ["alice", "beta"]);
        assert_eq!(limits.get("alpha"), None);
        assert_eq!(limits["alice"].max_concurrent_source, LimitSource::Config);
        assert_eq!(limits["beta"].max_concurrent_source, LimitSource::File);
    }

    /// Everything a provider's block can get wrong withholds **that**
    /// provider, names the key, leaves every other provider alone, and is
    /// reported by `validate` in the same words.
    #[test]
    fn a_malformed_entry_makes_its_provider_s_limits_unknown_and_names_the_key() {
        let tmp = pool(&["alpha"]);
        for (block, key) in [
            (
                serde_json::json!({ "zai": { "alpha": { "maxConcurrent": 0 } } }),
                "runtimes.apiKeyLimits.zai.alpha.maxConcurrent",
            ),
            (
                serde_json::json!({ "zai": { "alpha": { "maxConcurrent": "1" } } }),
                "runtimes.apiKeyLimits.zai.alpha.maxConcurrent",
            ),
            (
                serde_json::json!({ "zai": { "alpha": { "maxConcurrent": -1 } } }),
                "runtimes.apiKeyLimits.zai.alpha.maxConcurrent",
            ),
            (
                serde_json::json!({ "zai": { "alpha": { "maxConcurrent": 1.5 } } }),
                "runtimes.apiKeyLimits.zai.alpha.maxConcurrent",
            ),
            (
                serde_json::json!({ "zai": { "alpha": { "maxConcurrent": 4_294_967_296_u64 } } }),
                "runtimes.apiKeyLimits.zai.alpha.maxConcurrent",
            ),
            (
                serde_json::json!({ "zai": { "alpha": { "exhaustionWindowSecs": 0 } } }),
                "runtimes.apiKeyLimits.zai.alpha.exhaustionWindowSecs",
            ),
            (
                serde_json::json!({ "zai": { "alpha": { "maxConcurrent": 1, "maxConccurent": 2 } } }),
                "runtimes.apiKeyLimits.zai.alpha",
            ),
            // `planTokenLimit` is not configurable: feeding a gauge is not a cap.
            (
                serde_json::json!({ "zai": { "alpha": { "planTokenLimit": 5 } } }),
                "runtimes.apiKeyLimits.zai.alpha",
            ),
            (
                serde_json::json!({ "zai": { "alpha": "1" } }),
                "runtimes.apiKeyLimits.zai.alpha",
            ),
            (serde_json::json!({ "zai": "alpha" }), "runtimes.apiKeyLimits.zai"),
            (
                serde_json::json!({ "zai": { "Alpha": { "maxConcurrent": 1 } } }),
                "runtimes.apiKeyLimits.zai.Alpha",
            ),
        ] {
            let config = serde_json::json!({ "runtimes": { "apiKeyLimits": block } });
            let limits = ConfiguredLimits::from_config(&config);
            assert!(!limits.is_empty(), "{block}");
            let err = effective_limits(tmp.path(), "zai", &limits).unwrap_err();
            assert!(err.contains(key), "{block}: {err}");
            assert!(err.contains("provider \"zai\""), "{block}: {err}");
            // Scoped to the provider it names: one it does not mention reads on.
            assert!(effective_limits(tmp.path(), "openai", &limits).is_ok(), "{block}");
            let findings = ConfiguredLimits::check_config(&config);
            assert_eq!(findings.len(), 1, "{block}: {findings:?}");
            assert!(findings[0].contains(key), "{block}: {:?}", findings[0]);
            assert!(err.starts_with(&findings[0]), "{block}: {err}");
        }
        // Each malformed provider is its own finding; a sound one adds none.
        let config = serde_json::json!({ "runtimes": { "apiKeyLimits": {
            "zai": { "alpha": { "maxConcurrent": 0 } },
            "openai": { "bob": { "maxConcurrent": 0 } },
            "groq": { "carol": { "maxConcurrent": 3 } }
        } } });
        let findings = ConfiguredLimits::check_config(&config);
        assert_eq!(findings.len(), 2, "{findings:?}");
    }

    /// A block that names no provider — a wrong shape, or a provider key that
    /// is not a slug — withholds every pooled provider until it is repaired.
    #[test]
    fn a_block_that_cannot_be_understood_withholds_every_provider() {
        let tmp = pool(&["alpha"]);
        for (block, key) in [
            (serde_json::json!(5), "runtimes.apiKeyLimits"),
            (serde_json::json!([{ "zai": {} }]), "runtimes.apiKeyLimits"),
            (
                serde_json::json!({ "Zai": { "alpha": { "maxConcurrent": 1 } } }),
                "runtimes.apiKeyLimits.\"Zai\"",
            ),
        ] {
            let config = serde_json::json!({ "runtimes": { "apiKeyLimits": block } });
            let limits = ConfiguredLimits::from_config(&config);
            for provider in ["zai", "openai"] {
                let err = effective_limits(tmp.path(), provider, &limits).unwrap_err();
                assert!(err.contains(key), "{err}");
                assert!(err.contains(&format!("provider {provider:?}")), "{err}");
            }
            assert_eq!(ConfiguredLimits::check_config(&config).len(), 1, "{block}");
        }
    }

    /// When both sources are broken both are named: repairing one must not
    /// look like it repaired the account.
    #[test]
    fn both_sources_broken_reports_both() {
        let tmp = pool(&["alpha"]);
        let path = limits_path(tmp.path(), "zai");
        std::fs::write(&path, "{").unwrap();
        let limits = configured(serde_json::json!({ "zai": { "alpha": { "maxConcurrent": 0 } } }));
        let err = effective_limits(tmp.path(), "zai", &limits).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "{err}");
        assert!(err.contains("runtimes.apiKeyLimits.zai.alpha.maxConcurrent"), "{err}");
    }

    /// A rejected value is described by its kind, never repeated: config
    /// files are where a pasted secret is most likely to turn up.
    #[test]
    fn a_rejected_value_is_never_echoed() {
        let tmp = pool(&["alpha"]);
        let limits = configured(serde_json::json!({
            "zai": { "alpha": { "maxConcurrent": "fake-key-must-not-be-echoed" } }
        }));
        let err = effective_limits(tmp.path(), "zai", &limits).unwrap_err();
        assert!(err.contains("a string"), "{err}");
        assert!(!err.contains("fake-key-must-not-be-echoed"), "{err}");
    }

    #[test]
    fn each_provenance_has_a_stable_wire_form_and_label() {
        for (source, wire, label) in [
            (LimitSource::Unknown, "\"unknown\"", "unknown"),
            (LimitSource::Absent, "\"absent\"", "absent"),
            (LimitSource::File, "\"file\"", "file"),
            (LimitSource::Config, "\"config\"", "config"),
            (LimitSource::BothConfig, "\"both-config\"", "both, config wins"),
            (LimitSource::BothFile, "\"both-file\"", "both, file wins"),
        ] {
            assert_eq!(serde_json::to_string(&source).unwrap(), wire);
            assert_eq!(source.label(), label);
        }
    }
}
