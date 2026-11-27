//! Bounded `git remote get-url origin` lookup (#9715).
//!
//! The one remote-URL read the daemon-resident sites share
//! (`credential_preflight`, `forge_etag_store`,
//! `serve`, `tokens_pool::private_workspace::dispatch`),
//! so a `git` that hangs costs at most [`FORGE_CMD_TIMEOUT`] instead of
//! stalling its caller indefinitely.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::FORGE_CMD_TIMEOUT;
use crate::cmd_out::{run_command, CmdOutcome, Unavailable};

/// Get the `origin` remote URL from `cwd`, or `None`.
///
/// Bounded by [`FORGE_CMD_TIMEOUT`] with stdin nulled: a `git` that hangs
/// (e.g. walking up from a directory on a blocked external volume) yields
/// `None` after the deadline, with one line on stderr naming the directory.
pub(crate) fn get_remote_url(cwd: &Path) -> Option<String> {
    remote_url_with("git", cwd, FORGE_CMD_TIMEOUT)
}

/// Parse a git remote URL into `(host, owner/repo)`.
///
/// The one `(host, owner/repo)` remote-URL parser the daemon shares (#15624).
/// Accepts scp-style (`git@host:owner/repo.git`), `ssh://[user@]host[:port]/
/// owner/repo`, `https://[user@]host/owner/repo` (also `http`, `git`,
/// `git+ssh`). The host is lowercased and any `:port` / userinfo dropped. The
/// first two path segments are the repository; a path with fewer or more
/// segments, a local path, or an unknown scheme is `None`.
pub(crate) fn parse_remote_url(url: &str) -> Option<(String, String)> {
    let url = url.trim();
    let (host, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git" | "git+ssh") {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        let host = authority.rsplit('@').next()?;
        let host = host.split(':').next()?;
        (host, path)
    } else {
        // scp-like: [user@]host:path — but never a local path.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') || path.starts_with("//") {
            return None;
        }
        (authority.rsplit('@').next()?, path)
    };
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let mut segs = path.split('/').filter(|s| !s.is_empty());
    let (owner, repo) = (segs.next()?, segs.next()?);
    if host.is_empty() || segs.next().is_some() {
        return None;
    }
    Some((host.to_ascii_lowercase(), format!("{owner}/{repo}")))
}

/// [`get_remote_url`] with the program and deadline injectable, so the
/// timeout path is testable with a stub that sleeps past a short budget.
fn remote_url_with(program: &str, cwd: &Path, timeout: Duration) -> Option<String> {
    let mut cmd = Command::new(program);
    cmd.args(["remote", "get-url", "origin"])
        .current_dir(cwd)
        .stdin(Stdio::null());
    let outcome = run_command(cmd, timeout);
    if let CmdOutcome::Unavailable(Unavailable::TimedOut { after, .. }) = &outcome {
        eprintln!(
            "loom-daemon: `git remote get-url origin` in {} timed out after {after:?}",
            cwd.display()
        );
    }
    let out = outcome.ok_output()?;
    let url = std::str::from_utf8(&out.stdout).ok()?.trim().to_string();
    if url.is_empty() {
        None
    } else {
        Some(url)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// #9715: a `git` that hangs (stub sleeping far past the budget) makes
    /// the remote-URL lookup return `None` within the deadline instead of
    /// blocking its caller. The stub is a file named `git` invoked by path,
    /// so no process-global `PATH` mutation is needed.
    #[cfg(unix)]
    #[test]
    fn remote_url_lookup_times_out_on_hung_git() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let stub = dir.path().join("git");
        std::fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let budget = std::time::Duration::from_millis(500);
        let start = std::time::Instant::now();
        let url = remote_url_with(stub.to_str().unwrap(), dir.path(), budget);
        let elapsed = start.elapsed();

        assert_eq!(url, None);
        assert!(elapsed < std::time::Duration::from_secs(10), "took {elapsed:?}");
    }

    #[test]
    fn parse_remote_url_accepts_every_common_shape() {
        let want = Some(("github.com".to_string(), "o/r".to_string()));
        for url in [
            "git@github.com:o/r.git",
            "git@github.com:o/r",
            "ssh://git@github.com/o/r.git",
            "ssh://git@github.com:22/o/r.git",
            "ssh://github.com/o/r",
            "git+ssh://git@github.com/o/r.git",
            "https://github.com/o/r",
            "https://github.com/o/r.git/",
            "https://token@github.com/o/r.git",
            "http://github.com/o/r.git",
            "git://github.com/o/r.git",
            "  https://GitHub.com/o/r.git\n",
        ] {
            assert_eq!(parse_remote_url(url), want, "{url}");
        }
    }

    #[test]
    fn parse_remote_url_rejects_non_repo_urls() {
        for url in [
            "",
            "not-a-remote",
            "https://github.com/",
            "https://github.com/only-owner",
            "https://github.com/o/r/tree/main",
            "/srv/git/widgets.git",
            "../widgets.git",
            "file:///srv/git/o/r.git",
            "ftp://github.com/o/r.git",
        ] {
            assert_eq!(parse_remote_url(url), None, "{url}");
        }
    }

    /// The injectable path answers normally when `git` does (#9715 AC3).
    #[cfg(unix)]
    #[test]
    fn remote_url_lookup_returns_trimmed_url() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempdir().unwrap();
        let stub = dir.path().join("git");
        std::fs::write(&stub, "#!/bin/sh\necho '  git@github.com:o/r.git  '\n").unwrap();
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();

        let url =
            remote_url_with(stub.to_str().unwrap(), dir.path(), std::time::Duration::from_secs(10));
        assert_eq!(url.as_deref(), Some("git@github.com:o/r.git"));
    }
}
