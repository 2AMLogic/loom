//! Resolve the `docker` CLI executable every daemon launch site uses
//! (issue #14885).
//!
//! # The leak this closes
//!
//! A daemon started by launchd (or a minimal systemd unit) inherits
//! `PATH=/usr/bin:/bin:/usr/sbin:/sbin`, which names none of the directories
//! Docker Desktop installs its CLI into (`/usr/local/bin`, `~/.docker/bin`,
//! `/Applications/Docker.app/Contents/Resources/bin`). Every bare
//! `Command::new("docker")` — and every `run_bounded("docker", …)` helper
//! call — then failed at spawn with not-found, so an operator's
//! `loom-daemon accounts check --live` reported every session-managed account
//! as `unknown`/`session_probe_error` for as long as the caller's environment
//! stayed minimal.
//!
//! # One lookup, every call site
//!
//! The daemon selects Docker through this module alone (the reaper's
//! `LOOM_DOCKER_BIN` override is the same name, now honoured everywhere):
//!
//! 1. `LOOM_DOCKER_BIN` when set. An override that does not resolve to an
//!    executable file is an **error**, never a silent fall-through: a broken
//!    explicit override must fail even when another working `docker` exists.
//! 2. Each `$PATH` entry, in order.
//! 3. Absolute fallbacks, in order: `/usr/local/bin`, `/opt/homebrew/bin`,
//!    `~/.docker/bin` (expanded from the process's home),
//!    `/Applications/Docker.app/Contents/Resources/bin`, `/usr/bin`.
//!
//! A candidate qualifies only if [`crate::fs_util::is_executable`] accepts
//! it — an existing regular file (symlinks followed, which is how Docker
//! Desktop links its CLI) with an execute bit. Missing candidates,
//! directories and non-executable files are skipped. The caller's `PATH` is
//! never modified: the child gets the resolved absolute path.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::PathBuf;

use crate::fs_util;

/// Env var naming the docker CLI binary explicitly. Historically the reaper's
/// own override (`container_stop`); the shared lookup honours it for every
/// call site (#14885).
pub const DOCKER_BIN_ENV: &str = "LOOM_DOCKER_BIN";

/// The program name every default-selection call site used to hard-code, and
/// the literal a CLI `--docker` argument's `default_value` still carries: at
/// the use site it means "resolve through the shared lookup", never a bare
/// PATH-dependent spawn (#14885).
pub const DEFAULT_PROGRAM: &str = "docker";

/// Absolute fallback directories searched (in order) after `$PATH`, with
/// `~/.docker/bin` inserted at [`HOME_FALLBACK_AT`] when the process home is
/// known (#14885: `/usr/local/bin`, `/opt/homebrew/bin`, `~/.docker/bin`,
/// `/Applications/Docker.app/Contents/Resources/bin`, `/usr/bin`).
const FALLBACK_DIRS: &[&str] = &[
    "/usr/local/bin",
    "/opt/homebrew/bin",
    "/Applications/Docker.app/Contents/Resources/bin",
    "/usr/bin",
];

/// Where in [`FALLBACK_DIRS`]'s order the home-expanded `~/.docker/bin`
/// belongs.
const HOME_FALLBACK_AT: usize = 2;

/// The directory suffix appended to the process home for that fallback.
const HOME_FALLBACK_SUFFIX: &str = ".docker/bin";

/// Why [`resolve`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// `LOOM_DOCKER_BIN` names a value that is not an executable file.
    /// Carries the offending value: the error must fail the call — never
    /// fall through to a working `docker` elsewhere (#14885).
    InvalidOverride(String, io::ErrorKind),
    /// No candidate resolved: the override is unset, nothing on `$PATH`, and
    /// nothing in the fallback directories.
    NotFound,
}

impl fmt::Display for ResolveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOverride(value, _) => {
                write!(f, "{DOCKER_BIN_ENV}={value:?} does not name an executable file")
            }
            Self::NotFound => write!(
                f,
                "no docker executable on $PATH or in the fallback directories \
                 (/usr/local/bin, /opt/homebrew/bin, ~/.docker/bin, \
                 /Applications/Docker.app/Contents/Resources/bin, /usr/bin)"
            ),
        }
    }
}

impl std::error::Error for ResolveError {}

impl From<ResolveError> for io::Error {
    fn from(error: ResolveError) -> Self {
        let kind = match error {
            ResolveError::InvalidOverride(_, kind) => kind,
            ResolveError::NotFound => io::ErrorKind::NotFound,
        };
        Self::new(kind, error)
    }
}

/// The environment [`resolve_in`] searches — injected so tests drive synthetic
/// override/PATH/fallback values without mutating (and racing on) the real
/// process environment.
pub(crate) struct Search {
    /// A non-empty, trimmed `LOOM_DOCKER_BIN` value.
    pub override_value: Option<String>,
    /// The raw `$PATH`.
    pub path: Option<OsString>,
    /// The absolute fallback directories, in order.
    pub fallbacks: Vec<PathBuf>,
}

impl Search {
    /// The process environment: override and `$PATH` as-is, fallbacks built
    /// from the process home.
    fn from_env() -> Self {
        Self {
            override_value: std::env::var(DOCKER_BIN_ENV)
                .ok()
                .as_deref()
                .and_then(override_from),
            path: std::env::var_os("PATH"),
            fallbacks: fallback_dirs(
                std::env::var_os("HOME")
                    .as_ref()
                    .map(PathBuf::from)
                    .as_deref(),
            ),
        }
    }
}

/// A raw `LOOM_DOCKER_BIN` value as the lookup treats it: trimmed, with
/// empty (and whitespace-only) values ignored — the override is unset then.
pub(crate) fn override_from(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Describe the runtime selection for the bounded session startup diagnostic.
pub(crate) fn search_description() -> String {
    let search = Search::from_env();
    if let Some(value) = search.override_value {
        return format!("{DOCKER_BIN_ENV}={value:?}");
    }
    let dirs: Vec<_> = search.path.iter().flat_map(std::env::split_paths).collect();
    format!("PATH directories {dirs:?}, fallback directories {:?}", search.fallbacks)
}

/// Resolve the docker CLI executable — see the module docs for the order.
///
/// # Errors
/// [`ResolveError::InvalidOverride`] when `LOOM_DOCKER_BIN` is set but does
/// not name an executable file; [`ResolveError::NotFound`] when no candidate
/// resolved.
pub fn resolve() -> Result<PathBuf, ResolveError> {
    resolve_in(&Search::from_env())
}

/// Testable core of [`resolve`].
pub(crate) fn resolve_in(search: &Search) -> Result<PathBuf, ResolveError> {
    if let Some(value) = &search.override_value {
        let path = PathBuf::from(value);
        fs_util::check_executable(&path)
            .map_err(|err| ResolveError::InvalidOverride(value.clone(), err.kind()))?;
        return Ok(path);
    }
    if let Some(path) = &search.path {
        for dir in std::env::split_paths(path) {
            let candidate = dir.join(DEFAULT_PROGRAM);
            if fs_util::is_executable(&candidate) {
                return Ok(candidate);
            }
        }
    }
    for dir in &search.fallbacks {
        let candidate = dir.join(DEFAULT_PROGRAM);
        if fs_util::is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(ResolveError::NotFound)
}

/// [`FALLBACK_DIRS`] with `~/.docker/bin` expanded from `home` at its
/// position in the order; without a home, the fixed directories alone.
fn fallback_dirs(home: Option<&std::path::Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = FALLBACK_DIRS.iter().map(PathBuf::from).collect();
    if let Some(home) = home {
        dirs.insert(HOME_FALLBACK_AT, home.join(HOME_FALLBACK_SUFFIX));
    }
    dirs
}

/// [`resolve`] as a program string, for the bounded-runner helpers that take
/// one (`docker_cli::run_bounded(&str, …)`).
///
/// # Errors
/// As [`resolve`].
pub fn resolve_str() -> Result<String, ResolveError> {
    resolve().map(|path| path.to_string_lossy().into_owned())
}

/// The program for a call site that honours a scoped override (a CLI
/// `--docker` argument, `LOOM_CODEX_SESSION_DOCKER`): the explicit value
/// verbatim when given, else the shared resolution.
///
/// # Errors
/// As [`resolve`] — only when no explicit value was given.
pub fn scoped_or_resolved(explicit: Option<&str>) -> Result<String, ResolveError> {
    match explicit.filter(|value| !value.is_empty()) {
        Some(value) => Ok(value.to_string()),
        None => resolve_str(),
    }
}

/// [`scoped_or_resolved`] for fail-open call sites that learn "docker cannot
/// be asked" from the spawn failing (the session-state snapshot and its
/// readers): a resolution failure yields a guaranteed-unlaunchable absolute
/// path carrying the reason, so the caller's existing failure path fires —
/// and never a fall-through to a working `docker` an invalid override
/// forbids (#14885).
#[must_use]
pub fn scoped_or_fail_open(explicit: Option<&str>) -> String {
    match scoped_or_resolved(explicit) {
        Ok(program) => program,
        Err(error) => format!("/nonexistent/docker ({error})"),
    }
}

/// A CLI `--docker` argument's value: the `default_value` literal means
/// "resolve through the shared lookup" (#14885), any other value stays
/// verbatim (an explicit program argument, a test's injected fake). Failure
/// resolution is [`scoped_or_fail_open`]'s: unlaunchable, never a fall-through.
#[must_use]
pub fn cli_arg_or_fail_open(value: &str) -> String {
    if value == DEFAULT_PROGRAM {
        scoped_or_fail_open(None)
    } else {
        value.to_string()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    fn executable_at(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn plain_file_at(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"not executable").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
    }

    fn search(
        override_value: Option<&str>,
        path: Option<&[PathBuf]>,
        fallbacks: &[PathBuf],
    ) -> Search {
        Search {
            override_value: override_value.map(str::to_string),
            path: path.map(|dirs| std::env::join_paths(dirs.iter()).unwrap()),
            fallbacks: fallbacks.to_vec(),
        }
    }

    #[test]
    fn a_usable_override_wins_over_path_and_fallbacks() {
        let root = tempfile::tempdir().unwrap();
        let (r#override, on_path, in_fallback) = (
            root.path().join("override/docker"),
            root.path().join("path/docker"),
            root.path().join("fallback/docker"),
        );
        for path in [&r#override, &on_path, &in_fallback] {
            executable_at(path);
        }
        let resolved = resolve_in(&search(
            Some(r#override.to_str().unwrap()),
            Some(&[on_path.parent().unwrap().to_path_buf()]),
            &[in_fallback.parent().unwrap().to_path_buf()],
        ))
        .unwrap();
        assert_eq!(resolved, r#override);
    }

    #[test]
    fn an_invalid_override_fails_even_with_a_working_alternative() {
        let root = tempfile::tempdir().unwrap();
        let on_path = root.path().join("path/docker");
        executable_at(&on_path);
        let error = resolve_in(&search(
            Some("/nonexistent/docker"),
            Some(&[on_path.parent().unwrap().to_path_buf()]),
            &[],
        ))
        .unwrap_err();
        assert_eq!(
            error,
            ResolveError::InvalidOverride("/nonexistent/docker".into(), io::ErrorKind::NotFound)
        );
    }

    #[test]
    fn path_is_searched_in_order_before_the_fallbacks() {
        let root = tempfile::tempdir().unwrap();
        let (first, second, in_fallback) = (
            root.path().join("first/docker"),
            root.path().join("second/docker"),
            root.path().join("fallback/docker"),
        );
        for path in [&first, &second, &in_fallback] {
            executable_at(path);
        }
        let resolved = resolve_in(&search(
            None,
            Some(&[
                second.parent().unwrap().to_path_buf(),
                first.parent().unwrap().to_path_buf(),
            ]),
            &[in_fallback.parent().unwrap().to_path_buf()],
        ))
        .unwrap();
        assert_eq!(resolved, second, "the first PATH entry wins");

        let empty = std::env::join_paths(std::iter::empty::<PathBuf>()).unwrap();
        let resolved = resolve_in(&Search {
            override_value: None,
            path: Some(empty),
            fallbacks: vec![in_fallback.parent().unwrap().to_path_buf()],
        })
        .unwrap();
        assert_eq!(resolved, in_fallback, "with nothing on PATH, the fallback wins");
    }

    #[test]
    fn fallbacks_are_searched_in_order() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("a/docker"), root.path().join("b/docker"));
        for path in [&a, &b] {
            executable_at(path);
        }
        let resolved = resolve_in(&search(
            None,
            None,
            &[
                b.parent().unwrap().to_path_buf(),
                a.parent().unwrap().to_path_buf(),
            ],
        ))
        .unwrap();
        assert_eq!(resolved, b, "fallback order is honoured, not sorted");
    }

    #[test]
    fn the_home_fallback_sits_between_homebrew_and_docker_app() {
        let home = PathBuf::from("/tmp/loom-test-home-14885");
        let dirs = fallback_dirs(Some(&home));
        assert_eq!(
            dirs,
            [
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/opt/homebrew/bin"),
                home.join(".docker/bin"),
                PathBuf::from("/Applications/Docker.app/Contents/Resources/bin"),
                PathBuf::from("/usr/bin"),
            ]
        );
        assert_eq!(
            fallback_dirs(None),
            [
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/opt/homebrew/bin"),
                PathBuf::from("/Applications/Docker.app/Contents/Resources/bin"),
                PathBuf::from("/usr/bin"),
            ],
            "without a home, the fixed directories alone are searched"
        );
    }

    #[test]
    fn missing_directories_and_non_executable_candidates_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let hostile = root.path().join("hostile");
        fs::create_dir_all(hostile.join("docker")).unwrap(); // a DIRECTORY named docker
        let plain = root.path().join("plain");
        plain_file_at(&plain.join("docker"));
        let good = root.path().join("good");
        executable_at(&good.join("docker"));

        let resolved = resolve_in(&Search {
            override_value: None,
            path: Some(std::env::join_paths([hostile, plain]).unwrap()),
            fallbacks: vec![root.path().join("missing"), good.clone()],
        })
        .unwrap();
        assert_eq!(resolved, good.join("docker"));
    }

    #[test]
    fn an_executable_symlink_qualifies_like_docker_desktop_links() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target/docker");
        executable_at(&target);
        let link_dir = root.path().join("links");
        fs::create_dir_all(&link_dir).unwrap();
        std::os::unix::fs::symlink(&target, link_dir.join("docker")).unwrap();
        let resolved = resolve_in(&Search {
            override_value: None,
            path: Some(std::env::join_paths([&link_dir]).unwrap()),
            fallbacks: Vec::new(),
        })
        .unwrap();
        assert_eq!(resolved, link_dir.join("docker"));
    }

    #[test]
    fn nothing_anywhere_is_not_found() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            resolve_in(&search(
                None,
                Some(&[root.path().to_path_buf()]),
                &[root.path().to_path_buf()]
            )),
            Err(ResolveError::NotFound)
        );
        assert_eq!(resolve_in(&search(None, None, &[])), Err(ResolveError::NotFound));
    }

    #[test]
    fn a_whitespace_override_value_is_trimmed_and_an_empty_one_ignored() {
        let root = tempfile::tempdir().unwrap();
        let docker = root.path().join("bin/docker");
        executable_at(&docker);
        let resolved = resolve_in(&Search {
            override_value: override_from(&format!("  {}  ", docker.display())),
            path: None,
            fallbacks: Vec::new(),
        })
        .unwrap();
        assert_eq!(resolved, docker);

        assert_eq!(override_from("   "), None);
        assert_eq!(override_from(""), None);
        assert_eq!(
            resolve_in(&Search {
                override_value: override_from("   "),
                path: None,
                fallbacks: Vec::new(),
            }),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn an_explicit_scoped_value_stays_verbatim() {
        assert_eq!(scoped_or_resolved(Some("/tmp/a-fake/docker")).unwrap(), "/tmp/a-fake/docker");
        assert_eq!(cli_arg_or_fail_open("/tmp/a-fake/docker"), "/tmp/a-fake/docker");
    }
}
