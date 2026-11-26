//! Session-mode gateway publication (#14884): the daemon-free half of the
//! gateway read profiles, for a host that runs attended sessions with no
//! `loom-daemon` (epic #12708).
//!
//! A daemon host gets its profiles from the refresh loop
//! ([`super::spawn_refresh`]), which publishes into its workspace's
//! `.loom/gh-config-gateway/` and rewrites them every cadence. A session-only
//! host has no loop, so its fleet reads fell back to whatever smaller direct
//! credential was left and hit the quota wall (#14884). `loom-tools gateway
//! refresh` is the explicit replacement: one publication pass, run by the
//! operator at session start and after a key rotation, into a session-owned
//! destination. Setting a variable inside a short-lived `loom-tools` process
//! cannot update the shell that later starts a fleet reader, so a separately
//! launched reader is pointed at the publication with
//! [`super::DIR_ENV`] (or the downstream default) — see
//! `defaults/docs/session-mode.md`.
//!
//! # Destination and trust
//!
//! The profiles live at `<session root>/.loom/gh-config-gateway/<owner>/`
//! (default session root `~/.local/share/loom/session`, override
//! [`SESSION_ROOT_ENV`]): the same `.loom/gh-config-gateway/<owner>/hosts.yml`
//! suffix and `gh-ok` marker the readers already validate
//! ([`super::publish::safe_base`]), in a directory no checkout owns
//! ([`checked_base`] refuses a destination inside a git checkout). The
//! `forge.gateway` block is read from the host's private defaults tier only
//! ([`host_tier_config`]): a repository tier or an inherited
//! [`super::DIR_ENV`] must not redirect a host key to another origin.
//!
//! Nothing here grants dispatch or a write path: the profile is the same
//! read-only `hosts.yml` the daemon publishes
//! ([`super::publish::gateway_hosts_yaml`], no `user`, no `git_protocol`),
//! and nothing runs as a service.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde_json::Value;

use super::publish::{self, Deps};
use super::{parse, GatewayConfig, PassOutcome, DIR_NAME, MIN_GH_VERSION};

#[cfg(test)]
#[path = "session_tests.rs"]
mod session_tests;

/// Env override for the session root directory (the parent of the
/// publication's `.loom/`): non-empty names the directory, explicitly empty
/// disables the session publication entirely.
pub const SESSION_ROOT_ENV: &str = "LOOM_SESSION_GATEWAY_ROOT";

/// The default session root under the home directory.
#[cfg_attr(test, allow(dead_code))]
const DEFAULT_SESSION_ROOT_REL: &str = ".local/share/loom/session";

/// How long a published profile counts as current: the same republish
/// cadence the daemon's loop rewrites profiles on, so a rotated key is due
/// for pickup after this long without a `gateway refresh`.
pub const REFRESH_CADENCE: Duration = crate::credential_preflight::GITHUB_APP_REFRESH_INTERVAL;

/// The session root ([`SESSION_ROOT_ENV`], then `~/.local/share/loom/session`),
/// or `None` when disabled (an explicitly empty override) or no home
/// directory resolves.
///
/// The home fallback is refused under `cfg(test)` for the same reason
/// [`crate::config_resolver::private_defaults_path`] refuses its own
/// (#8584): the env var is process-global and every `#[test]` in this crate
/// links into one multi-threaded binary, so a default fallback would let the
/// operator's live session root decide what a fixture sees.
#[must_use]
pub fn session_root() -> Option<PathBuf> {
    if let Ok(v) = std::env::var(SESSION_ROOT_ENV) {
        return if v.trim().is_empty() {
            None
        } else {
            Some(PathBuf::from(v))
        };
    }
    #[cfg(test)]
    {
        None
    }
    #[cfg(not(test))]
    {
        dirs::home_dir().map(|h| h.join(DEFAULT_SESSION_ROOT_REL))
    }
}

/// `<root>/.loom/gh-config-gateway`: the base readers are pointed at.
#[must_use]
pub fn session_base_at(root: &Path) -> PathBuf {
    root.join(".loom").join(DIR_NAME)
}

/// Whether `path` sits inside a git checkout (a `.git` — a directory or a
/// worktree's file — at or above it). The session publication must be
/// session-owned: a checkout's ignore rules, settings and cleanup must never
/// own it, and no repository setting can redirect it.
fn inside_a_checkout(path: &Path) -> bool {
    path.ancestors().any(|a| a.join(".git").exists())
}

/// The session publication directory, after the safety checks: refuses a
/// destination inside a git checkout, and a disabled root.
///
/// # Errors
///
/// A reason naming the refused location and the fix.
pub fn checked_base() -> Result<PathBuf, String> {
    let Some(root) = session_root() else {
        return Err(format!(
            "no session root resolves: {SESSION_ROOT_ENV} is explicitly empty or no home \
             directory is set"
        ));
    };
    checked_base_at(&root, std::env::current_dir)
}

/// [`checked_base`] for an explicit root and cwd lookup: the root is
/// absolutized before [`inside_a_checkout`] runs, because a relative
/// [`SESSION_ROOT_ENV`] leaves `ancestors()` walking a relative path that
/// never reaches the checkout root and slips the guard from below it
/// (#14964). The cwd lookup is injectable since the process cwd is global
/// state every concurrent `#[test]` shares.
///
/// # Errors
///
/// A reason naming the refused location and the fix, or naming
/// [`SESSION_ROOT_ENV`] when the root cannot be resolved — fail closed, the
/// un-absolutized relative path is never checked.
fn checked_base_at(
    root: &Path,
    cwd: impl FnOnce() -> std::io::Result<PathBuf>,
) -> Result<PathBuf, String> {
    let joined = if root.is_absolute() {
        root.to_path_buf()
    } else {
        let cwd = cwd().map_err(|e| {
            format!(
                "cannot resolve the current directory to anchor the relative {SESSION_ROOT_ENV} \
                 session root {}: {e}",
                root.display()
            )
        })?;
        cwd.join(root)
    };
    // `absolute` cleans redundant components without touching the filesystem
    // (the root need not exist yet), so `ancestors()` walks to `/`; `..` is
    // left in place, and the OS resolves it during the `.git` lookup.
    let rooted = std::path::absolute(joined.as_path()).map_err(|e| {
        format!(
            "cannot absolutize the {SESSION_ROOT_ENV} session root {}: {e}",
            joined.display()
        )
    })?;
    let base = session_base_at(&rooted);
    if inside_a_checkout(&base) {
        return Err(format!(
            "the session publication {} would sit inside a git checkout; point {SESSION_ROOT_ENV} \
             at a directory outside every checkout",
            base.display()
        ));
    }
    Ok(base)
}

/// The trusted host tier for the session publication: the machine-level
/// defaults file only ([`crate::config_resolver::private_defaults_path`]).
/// Repository tiers never contribute — a repository setting must not
/// redirect a host key (#14884) — and neither does an inherited
/// [`super::DIR_ENV`]: the destination is [`checked_base`]'s, always.
#[must_use]
pub fn host_tier_config_from(path: Option<&Path>) -> Value {
    path.map(crate::config_resolver::soft_read_json_object)
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()))
}

/// [`host_tier_config_from`] against the live private defaults path.
#[must_use]
pub fn host_tier_config() -> Value {
    host_tier_config_from(crate::config_resolver::private_defaults_path().as_deref())
}

/// Remove exactly the session publication at `base`: that directory only,
/// never a workspace's `.loom/gh-config-gateway` (a daemon host's
/// publication). Whether anything was removed; idempotent.
pub fn clean(base: &Path) -> bool {
    match std::fs::symlink_metadata(base) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(base).is_ok(),
        Ok(_) => std::fs::remove_file(base).is_ok(),
        Err(_) => false,
    }
}

/// One session publication pass into `base` from the host tier `host`.
/// Always due (an explicit command, not a loop): every configured owner's
/// profile is rewritten — so a second invocation picks up a rotated key —
/// dropped owners are withdrawn ([`publish`]'s stray sweep), a profile whose
/// key no longer resolves is withdrawn, and a `gh` below
/// [`MIN_GH_VERSION`] publishes nothing and removes what was there (its
/// `api_host` would be ignored and the key sent to `api.github.com`).
/// Returns the outcome plus warnings whose text never contains a key.
///
/// Never touches the process-global daemon state ([`super::active`]): a
/// one-shot tool owns no route.
pub fn refresh(base: &Path, host: &Value, deps: &dyn Deps) -> (PassOutcome, Vec<String>) {
    let mut warnings = Vec::new();
    let (cfg, parse_warnings) = parse(host);
    warnings.extend(parse_warnings);
    let Some(cfg) = cfg else {
        clean(base);
        return (PassOutcome::Off, warnings);
    };
    if let Err(e) = publish::ensure_private_dir(base) {
        clean(base);
        warnings.push(format!("cannot create {}: {e}", base.display()));
        return (PassOutcome::Off, warnings);
    }
    let gh = deps.gh(base);
    let seen = gh.as_ref().map(|(_, v)| *v);
    let Some((gh_path, _)) = gh.filter(|(_, v)| *v >= MIN_GH_VERSION) else {
        clean(base);
        return (PassOutcome::GhTooOld(seen), warnings);
    };
    let wrote = publish::write_all(base, &cfg, &gh_path, deps);
    for (owner, why) in &wrote.failed {
        warnings.push(format!("{owner}: {why}"));
    }
    if !wrote.marker_ok {
        warnings.push(format!(
            "could not write the {} marker in {}",
            publish::GH_MARKER,
            base.display()
        ));
    }
    (
        PassOutcome::Published {
            owners: wrote.owners,
            failed: wrote.failed.into_iter().map(|(o, _)| o).collect(),
        },
        warnings,
    )
}

/// [`refresh`] against the real outside world: the resolver's `gh` and the
/// configured key sources.
pub fn refresh_real(base: &Path, host: &Value) -> (PassOutcome, Vec<String>) {
    refresh(base, host, &publish::RealDeps)
}

/// One owner's row in the status report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnerState {
    /// Published, and rewritten within [`REFRESH_CADENCE`].
    Current { age_secs: u64 },
    /// Published, but older than [`REFRESH_CADENCE`]: a rotated key is not
    /// picked up until `gateway refresh` runs again.
    Due { age_secs: u64 },
    /// Configured in the host tier but not published.
    Missing,
    /// Published but no longer configured, or not readable as a trusted
    /// profile: `gateway refresh` withdraws or rewrites it.
    Stray { age_secs: Option<u64> },
}

/// The session publication as `gateway status` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub base: PathBuf,
    /// Whether the publication directory exists at all.
    pub exists: bool,
    /// Whether the `gh-ok` marker is present (readers refuse the base
    /// without it).
    pub marker: bool,
    pub owners: Vec<(String, OwnerState)>,
}

/// Classify the publication at `base` against the host tier `host` (whose
/// configured owner set decides `Missing` and `Stray`), read-only: nothing
/// is written, no key is read, and no credential appears in the result.
#[must_use]
pub fn status(base: &Path, host: &Value, now: SystemTime) -> Status {
    let (cfg, _) = parse(host);
    let configured: Option<&GatewayConfig> = cfg.as_ref();
    let mut owners = Vec::new();
    let mut published: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(base) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !e.path().is_dir() || !crate::forge_bucket_book::valid_owner(&name) {
                continue;
            }
            let owner = name.to_ascii_lowercase();
            let age = mtime_age(&e.path().join("hosts.yml"), now);
            let trusted = super::publish::safe_profile(&e.path());
            let state = match (trusted, age, configured) {
                (false, ..) => OwnerState::Stray { age_secs: None },
                (true, Some(age), c) if c.is_some_and(|c| c.key_refs.contains_key(&owner)) => {
                    if age <= REFRESH_CADENCE.as_secs() {
                        OwnerState::Current { age_secs: age }
                    } else {
                        OwnerState::Due { age_secs: age }
                    }
                }
                (true, age, _) => OwnerState::Stray { age_secs: age },
            };
            published.push(owner.clone());
            owners.push((owner, state));
        }
    }
    if let Some(cfg) = configured {
        for owner in cfg.key_refs.keys() {
            if !published.contains(owner) {
                owners.push((owner.clone(), OwnerState::Missing));
            }
        }
    }
    owners.sort_by(|a, b| a.0.cmp(&b.0));
    Status {
        base: base.to_path_buf(),
        exists: base.is_dir(),
        marker: base.join(publish::GH_MARKER).is_file(),
        owners,
    }
}

/// Seconds since `path`'s mtime, or `None` when it does not read.
fn mtime_age(path: &Path, now: SystemTime) -> Option<u64> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    now.duration_since(mtime).ok().map(|d| d.as_secs())
}

fn age_text(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

/// The `gateway status` lines: owners, paths, ages and the marker — never a
/// credential (the key lives only inside each 0600 `hosts.yml`).
#[must_use]
pub fn render_status(s: &Status) -> Vec<String> {
    let mut lines = Vec::new();
    if !s.exists && s.owners.is_empty() {
        lines.push(format!(
            "session gateway profiles: none at {} — run 'loom-tools gateway refresh'",
            s.base.display()
        ));
        return lines;
    }
    lines.push(format!("session gateway profiles: {}", s.base.display()));
    let cadence = age_text(REFRESH_CADENCE.as_secs());
    for (owner, state) in &s.owners {
        let row = match state {
            OwnerState::Current { age_secs } => {
                format!("  {owner}: current (rewritten {} ago)", age_text(*age_secs))
            }
            OwnerState::Due { age_secs } => format!(
                "  {owner}: due for refresh (rewritten {} ago, older than {cadence}; run \
                 'loom-tools gateway refresh')",
                age_text(*age_secs)
            ),
            OwnerState::Missing => {
                format!(
                    "  {owner}: missing (configured but not published; run 'loom-tools \
                         gateway refresh')"
                )
            }
            OwnerState::Stray { .. } => format!(
                "  {owner}: stray (not in the host config or not a trusted profile; \
                 'loom-tools gateway refresh' withdraws it)"
            ),
        };
        lines.push(row);
    }
    lines.push(if s.marker {
        format!("  {} marker: present", publish::GH_MARKER)
    } else {
        format!(
            "  {} marker: missing (readers refuse the publication without it; run \
             'loom-tools gateway refresh')",
            publish::GH_MARKER
        )
    });
    lines
}
