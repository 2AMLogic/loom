//! Small filesystem/formatting helpers shared across the crate: the reclaim
//! and GC modules (`target_dir_gc`, `tmpfs_reclaim`, `scratch_reclaim`,
//! `git_tmp_reclaim`, `docker_image_clean`, `target_orphan_reclaim`) and, since
//! issue #15372, the one `~`-expansion convention every pool and forge path
//! resolver delegates to ([`expand_tilde_with_home`]).

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};

/// Durability/permission options for [`write_atomic`].
#[derive(Debug, Clone, Copy)]
pub struct AtomicOpts {
    /// Owner-only (`0600`) file on unix; otherwise `0644`.
    pub private: bool,
    /// `fsync` the parent directory after the rename so the rename itself
    /// survives a crash (#10831). Best-effort where a directory cannot be
    /// opened for sync.
    pub sync_dir: bool,
}

impl Default for AtomicOpts {
    /// Crash-safe state: not private, parent directory synced.
    fn default() -> Self {
        Self {
            private: false,
            sync_dir: true,
        }
    }
}

/// Leading part of the name [`write_atomic`] stages its temp file under.
pub const STAGING_PREFIX: &str = ".tmp";

/// Length of the random suffix after [`STAGING_PREFIX`].
pub const STAGING_RAND_LEN: usize = 6;

/// Whether `name` has the shape of a [`write_atomic`] staging file
/// (`STAGING_PREFIX` + `STAGING_RAND_LEN` ASCII alphanumerics). Readers that
/// allowlist a directory's entries use this rather than a second literal.
#[must_use]
pub fn is_staging_name(name: &str) -> bool {
    name.strip_prefix(STAGING_PREFIX).is_some_and(|r| {
        r.len() == STAGING_RAND_LEN && r.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// Write `bytes` to `path` atomically: create the parent directory, write a
/// uniquely named temp file beside `path`, `fsync` it, then `rename` it into
/// place. A reader sees the old file or the new one, never a partial write,
/// and no temp file is left behind on success.
///
/// # Errors
/// Any I/O failure creating, writing, syncing or renaming the file.
pub fn write_atomic(path: &Path, bytes: &[u8], opts: AtomicOpts) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    write_atomic_in_existing_dir(path, bytes, opts)
}

/// As [`write_atomic`], but a parent removed during an operation stays removed.
pub(crate) fn write_atomic_in_existing_dir(
    path: &Path,
    bytes: &[u8],
    opts: AtomicOpts,
) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut tmp = tempfile::Builder::new()
        .prefix(STAGING_PREFIX)
        .rand_bytes(STAGING_RAND_LEN)
        .tempfile_in(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = if opts.private { 0o600 } else { 0o644 };
        tmp.as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    tmp.write_all(bytes)?;
    tmp.as_file().sync_all()?;
    tmp.persist(path).map_err(|e| e.error)?;
    if opts.sync_dir {
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}

/// Format a byte count as `B` / `K` / `M` / `G` with one decimal place.
pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1}G", b / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1}M", b / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1}K", b / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

/// Recursively find the newest mtime and cumulative size under `path`,
/// without following symlinks. Best-effort: an unreadable child is skipped
/// rather than failing the whole walk (a permissions hiccup on one nested
/// file must not abort GC of everything else).
pub(crate) fn newest_mtime_and_size(path: &Path) -> io::Result<(DateTime<Utc>, u64)> {
    let meta = fs::symlink_metadata(path)?;
    if meta.is_dir() {
        let mut newest = mtime_of(&meta);
        let mut total = 0u64;
        let entries = match fs::read_dir(path) {
            Ok(e) => e,
            Err(_) => return Ok((newest, total)),
        };
        for entry in entries.flatten() {
            if let Ok((child_newest, child_size)) = newest_mtime_and_size(&entry.path()) {
                if child_newest > newest {
                    newest = child_newest;
                }
                total = total.saturating_add(child_size);
            }
        }
        Ok((newest, total))
    } else {
        Ok((mtime_of(&meta), meta.len()))
    }
}

fn mtime_of(meta: &fs::Metadata) -> DateTime<Utc> {
    meta.modified()
        .ok()
        .map_or_else(Utc::now, DateTime::<Utc>::from)
}

/// Is `path` an existing regular file (symlinks followed) with any execute
/// bit set? On non-unix targets, any regular file qualifies.
#[must_use]
pub fn is_executable(path: &Path) -> bool {
    check_executable(path).is_ok()
}

/// Like `is_executable`, retaining the failure kind for startup diagnostics.
pub(crate) fn check_executable(path: &Path) -> io::Result<()> {
    let meta = fs::metadata(path)?;
    if !meta.is_file() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if meta.permissions().mode() & 0o111 == 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(())
}

/// Find `name` as an executable file in `search_path` (a `$PATH`-shaped
/// `OsStr`), returning the first matching candidate. Mirrors a shell's
/// `command -v` lookup without spawning one: the first directory holding a
/// regular file (symlinks followed) with any execute bit set wins, so a
/// non-executable stub earlier in the path cannot shadow a real executable
/// further on. [`find_executable_on_path`] is the process-`$PATH` form.
#[must_use]
pub fn find_executable_in(name: &str, search_path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(search_path)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// [`find_executable_in`] against the process `$PATH`: `None` when `$PATH` is
/// unset or nothing resolves to an executable file.
#[must_use]
pub fn find_executable_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| find_executable_in(name, &paths))
}

/// Whether `name` resolves to an executable file on the process `$PATH` —
/// `command -v <name>` without the shell.
#[must_use]
pub fn is_on_path(name: &str) -> bool {
    find_executable_on_path(name).is_some()
}

/// Keep only the last `max` bytes of captured output, snapped forward to a
/// char boundary and trimmed of surrounding whitespace (the failure detail is
/// usually last).
#[must_use]
pub fn truncate_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.trim().to_string();
    }
    let start = s.len() - max;
    let start = (start..s.len())
        .find(|&i| s.is_char_boundary(i))
        .unwrap_or(s.len());
    s[start..].trim().to_string()
}

/// Expand a leading tilde in `raw` against an explicit, optional home.
///
/// `~` resolves to the home itself and `~/rest` to `home/rest`; any other
/// input — `~other/key`, a plain relative, an absolute path — and every input
/// when no home is available is returned verbatim, so the caller's own
/// validation still sees exactly what was configured (issue #15372: this is
/// the one shared implementation the credential pools, fleet roster and
/// forge identity used to carry as per-module copies).
///
/// The home *source* stays the caller's decision: `dirs::home_dir()`
/// ([`expand_tilde`]), an env var read, or a value injected by a test — sites
/// that must not fall back to OS account discovery pass their own lookup's
/// result and get `None`-preserving behavior for free.
#[must_use]
pub(crate) fn expand_tilde_with_home(raw: &str, home: Option<&Path>) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = home {
            return home.join(rest);
        }
    } else if raw == "~" {
        if let Some(home) = home {
            return home.to_path_buf();
        }
    }
    PathBuf::from(raw)
}

/// [`expand_tilde_with_home`] with the home from `dirs::home_dir()`: the
/// form the credential pools use, where a discoverable account home is an
/// acceptable substitute for `$HOME`.
#[must_use]
pub(crate) fn expand_tilde(raw: &str) -> PathBuf {
    expand_tilde_with_home(raw, dirs::home_dir().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_tail_trims_short_text_and_cuts_on_char_boundary() {
        assert_eq!(truncate_tail("  hi \n", 10), "hi");
        let raw = "é".repeat(100);
        let tail = truncate_tail(&raw, 11);
        assert!(tail.len() <= 11);
        assert!(tail.chars().all(|c| c == 'é'));
        assert_eq!(truncate_tail("abcdef", 3), "def");
    }

    #[test]
    fn is_executable_requires_regular_file_with_exec_bit() {
        let td = tempfile::tempdir().unwrap();
        assert!(!is_executable(td.path()), "directory");
        assert!(!is_executable(&td.path().join("missing")));
        let f = td.path().join("f");
        fs::write(&f, b"x").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert!(!is_executable(&f), "no exec bit");
            fs::set_permissions(&f, fs::Permissions::from_mode(0o755)).unwrap();
            assert!(is_executable(&f));
        }
    }

    #[cfg(unix)]
    #[test]
    fn find_executable_in_skips_non_executable_shadows_and_directories() {
        use std::os::unix::fs::PermissionsExt as _;

        let td = tempfile::tempdir().unwrap();
        let early = td.path().join("early");
        let late = td.path().join("late");
        let dir_collision = td.path().join("collision");
        for d in [&early, &late, &dir_collision.join("tool")] {
            fs::create_dir_all(d).unwrap();
        }

        // A non-executable regular file earlier on the path must not shadow
        // the real executable further on.
        fs::write(early.join("tool"), b"stub").unwrap();
        let real = late.join("tool");
        fs::write(&real, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();

        let joined = std::env::join_paths([&early, &late]).expect("join_paths of two temp dirs");
        assert_eq!(find_executable_in("tool", &joined), Some(real.clone()));

        // Without the executable further on, nothing resolves at all.
        let only_early = std::env::join_paths([&early]).unwrap();
        assert_eq!(find_executable_in("tool", &only_early), None);
        // A directory named like the tool is not a candidate either.
        let only_collision = std::env::join_paths([&dir_collision]).unwrap();
        assert_eq!(find_executable_in("tool", &only_collision), None);
        // An empty search path finds nothing.
        let empty = std::env::join_paths(std::iter::empty::<&Path>()).unwrap();
        assert_eq!(find_executable_in("tool", &empty), None);
    }

    #[cfg(unix)]
    #[test]
    fn is_on_path_matches_the_shared_lookup() {
        // `sh` exists on every supported host; the bool form must agree with
        // the resolving form on the same process `$PATH`.
        assert_eq!(is_on_path("sh"), find_executable_on_path("sh").is_some());
        assert!(is_on_path("sh"), "sh must resolve on a supported host's PATH");
        assert!(!is_on_path("definitely-not-a-real-tool-loom-fs-util"));
    }

    #[test]
    fn staging_name_matcher_accepts_what_the_writer_stages() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .rand_bytes(STAGING_RAND_LEN)
            .tempfile_in(tmp.path())
            .unwrap();
        let name = staged
            .path()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(is_staging_name(&name), "{name}");
        for bad in [
            ".tmp",
            ".tmpabc",
            ".tmpabcdefg",
            ".tmp-abcde",
            "tmpabcdef",
            "a.token",
        ] {
            assert!(!is_staging_name(bad), "{bad}");
        }
    }

    #[test]
    fn write_atomic_creates_parent_replaces_and_leaves_no_temp() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join("a/b/state.json");
        write_atomic(&path, b"one", AtomicOpts::default()).unwrap();
        write_atomic(&path, b"two", AtomicOpts::default()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "leftover temp file: {names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn write_atomic_private_is_0600_and_default_is_0644() {
        use std::os::unix::fs::PermissionsExt as _;
        let td = tempfile::tempdir().unwrap();
        let p = td.path().join("secret");
        let opts = AtomicOpts {
            private: true,
            sync_dir: false,
        };
        write_atomic(&p, b"x", opts).unwrap();
        assert_eq!(fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        let q = td.path().join("public");
        write_atomic(&q, b"x", AtomicOpts::default()).unwrap();
        assert_eq!(fs::metadata(&q).unwrap().permissions().mode() & 0o777, 0o644);
    }

    #[test]
    fn expand_tilde_with_home_covers_the_tilde_forms() {
        // The full-form callers (pools, monitor dir, roster root): a bare `~`
        // is the home, `~/rest` joins under it, anything else passes through.
        let home = Path::new("/home/tester");
        let cases: &[(&str, &str)] = &[
            ("~", "/home/tester"),
            ("~/key", "/home/tester/key"),
            ("~other/key", "~other/key"),
            ("plain", "plain"),
            ("./rel", "./rel"),
            ("/abs/key", "/abs/key"),
        ];
        for (raw, want) in cases {
            assert_eq!(
                expand_tilde_with_home(raw, Some(home)),
                PathBuf::from(*want),
                "raw = {raw}"
            );
        }
    }

    #[test]
    fn expand_tilde_without_a_home_returns_the_input_verbatim() {
        for raw in ["~", "~/key", "~other/key", "plain", "/abs"] {
            assert_eq!(expand_tilde_with_home(raw, None), PathBuf::from(raw));
        }
    }

    #[test]
    fn expand_tilde_delegates_to_dirs_home_dir() {
        // Only the pass-through arm is host-independent enough to pin here;
        // the tilde arms are [`expand_tilde_with_home`]'s table above.
        assert_eq!(expand_tilde("plain"), PathBuf::from("plain"));
        assert_eq!(
            expand_tilde("/abs/key"),
            PathBuf::from("/abs/key"),
            "an absolute path must never be rewritten"
        );
    }
}
