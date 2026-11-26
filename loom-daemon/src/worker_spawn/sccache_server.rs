//! A temp root that outlives one run for the host's shared `sccache` server
//! (#15139).
//!
//! `sccache` is a per-user server that the first `cargo` client starts lazily,
//! with that client's environment. A guarded worker runs under a per-run
//! `TMPDIR` (`<pinned_tmp_base>/<16 hex>`) that
//! [`crate::native_state_reclaim::sweep_pinned_tmp`] removes once the run's
//! session directory is gone. A server started by a worker's own `cargo`
//! therefore keeps creating its per-compile temp dirs under a root that no
//! longer exists, and every later `rustc` call through it — for every sweep on
//! the host — dies with `Failed to create temp dir`.
//!
//! Before the worker execs, this seam makes sure the server is up *with a
//! stable temp root*: it runs `sccache --start-server` with `TMPDIR` set to
//! [`crate::native_state_reclaim::shared_sccache_tmp`] (a non-hex leaf the
//! reaper never removes). Measured against `sccache 0.17.0`:
//! `--start-server` starts a server that inherits that `TMPDIR` (exit 0), and
//! against a live server it is **not** idempotent — it exits non-zero with
//! `Address in use`, which this seam reads as "already running". (`--show-stats`
//! is no substitute: it exits 0 whether or not a server is up and starts none.)
//! The worker's own `TMPDIR` is not touched.
//!
//! Limits, stated plainly:
//!
//! * a server that is *already* running keeps the environment it was started
//!   with — this seam cannot move it, only prevent the next one from being
//!   started badly;
//! * only a `RUSTC_WRAPPER` whose file name is `sccache` is acted on; an
//!   absent or different wrapper is a no-op;
//! * a failed or hung probe is logged once and never fails the launch, so the
//!   worker is not blocked by a cache.

use std::ffi::OsStr;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The wrapper variable this seam keys on.
pub const WRAPPER_ENV: &str = "RUSTC_WRAPPER";
/// Longest the start-and-probe call may take before it is killed and reported.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest stderr excerpt carried into the log line.
const STDERR_EXCERPT_BYTES: usize = 300;

/// What one pass did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// `RUSTC_WRAPPER` is not `sccache`: nothing was done.
    Skipped,
    /// No server was up; this call started one under the stable root.
    Started,
    /// A server was already up. Its environment is whatever it was started
    /// with — this seam did not and cannot change it.
    AlreadyRunning,
    /// The root could not be prepared or the probe failed; carries the reason.
    Unhealthy(String),
}

/// `value` as an sccache wrapper path, or `None` when it is unset, empty, or
/// names some other tool.
#[must_use]
pub fn sccache_wrapper(value: Option<&OsStr>) -> Option<PathBuf> {
    let path = Path::new(value.filter(|v| !v.is_empty())?);
    let name = path.file_name()?.to_str()?;
    matches!(name, "sccache" | "sccache.exe").then(|| path.to_path_buf())
}

/// Create `dir` (and parents) owner-only if absent, and require it to be a
/// real directory we own — a symlink or another user's directory under the
/// shared `/tmp` is refused rather than followed. An existing directory of
/// ours with looser bits is tightened to `0700` (the shared
/// [`private_directory_with`] sequence, #15187).
fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        crate::native_tools::provision::private_directory_with(
            dir,
            crate::native_tools::provision::OnLoose::Tighten,
        )
        .map_err(std::io::Error::other)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)
    }
}

/// Run `<wrapper> --start-server` with `TMPDIR=<stable>`, bounded by `timeout`.
/// Zero exit is [`Outcome::Started`]; a non-zero exit whose stderr says
/// `Address in use` is [`Outcome::AlreadyRunning`]; anything else is the reason.
fn probe(wrapper: &Path, stable: &Path, timeout: Duration) -> Result<Outcome, String> {
    let mut stderr = tempfile::tempfile().map_err(|e| format!("no scratch file: {e}"))?;
    let mut command = Command::new(wrapper);
    command
        .arg("--start-server")
        .env("TMPDIR", stable)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(
            stderr
                .try_clone()
                .map_err(|e| format!("no scratch file: {e}"))?,
        ));
    // Own process group: a server this call starts must not share the
    // worker's group, or cancelling that one sweep (`kill(-pgid)`) would take
    // the host-shared server, and every other sweep's compiles, down with it.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", wrapper.display()))?;
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("`--start-server` did not return in {}s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => return Err(format!("cannot wait for `--start-server`: {e}")),
        }
    };
    if status.success() {
        return Ok(Outcome::Started);
    }
    let mut text = String::new();
    let _ = stderr.seek(SeekFrom::Start(0));
    let _ = stderr.take(4096).read_to_string(&mut text);
    let excerpt: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(STDERR_EXCERPT_BYTES)
        .collect();
    if excerpt.contains("Address in use") {
        return Ok(Outcome::AlreadyRunning);
    }
    Err(format!("`--start-server` exited {status}: {excerpt}"))
}

/// The seam with every input explicit, so a test needs no process env.
pub fn ensure_in(
    wrapper_env: Option<&OsStr>,
    stable: &Path,
    timeout: Duration,
    log: &mut dyn Write,
) -> Outcome {
    let Some(wrapper) = sccache_wrapper(wrapper_env) else {
        return Outcome::Skipped;
    };
    let result = ensure_private_dir(stable)
        .map_err(|e| format!("temp root {} unusable: {e}", stable.display()))
        .and_then(|()| probe(&wrapper, stable, timeout));
    match result {
        Ok(Outcome::Started) => {
            let _ =
                writeln!(log, "# LOOM_SCCACHE_SERVER started tmpdir={} (#15139)", stable.display());
            Outcome::Started
        }
        Ok(outcome) => {
            let _ = writeln!(
                log,
                "# LOOM_SCCACHE_SERVER already running (environment not changed by this launch) (#15139)"
            );
            outcome
        }
        Err(reason) => {
            let _ = writeln!(
                log,
                "# LOOM_SCCACHE_SERVER unhealthy: {reason} (#15139) — compiles through \
                 sccache may fail; not retried per crate"
            );
            Outcome::Unhealthy(reason)
        }
    }
}

/// The worker-spawn entry: acts on the ambient `RUSTC_WRAPPER`, with the
/// host's stable root.
pub fn ensure(log: &mut dyn Write) -> Outcome {
    ensure_in(
        std::env::var_os(WRAPPER_ENV).as_deref(),
        &crate::native_state_reclaim::shared_sccache_tmp(),
        PROBE_TIMEOUT,
        log,
    )
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in `sccache` that records the `TMPDIR` and argv it saw, then
    /// runs `body`.
    fn fake(dir: &Path, body: &str) -> PathBuf {
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join("sccache");
        let script = format!(
            "#!/bin/sh\nprintf '%s %s\\n' \"$TMPDIR\" \"$*\" > '{}'\n{body}\n",
            dir.join("seen").display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn run(wrapper: &Path, stable: &Path, log: &mut Vec<u8>) -> Outcome {
        ensure_in(Some(wrapper.as_os_str()), stable, Duration::from_secs(5), log)
    }

    #[test]
    fn wrapper_detection_matches_only_sccache() {
        assert!(sccache_wrapper(Some(OsStr::new("sccache"))).is_some());
        assert!(sccache_wrapper(Some(OsStr::new("/opt/homebrew/bin/sccache"))).is_some());
        assert!(sccache_wrapper(Some(OsStr::new("/usr/bin/ccache"))).is_none());
        assert!(sccache_wrapper(Some(OsStr::new("/x/sccache/rustc-wrap"))).is_none());
        assert!(sccache_wrapper(Some(OsStr::new(""))).is_none());
        assert!(sccache_wrapper(None).is_none());
    }

    #[test]
    fn a_non_sccache_wrapper_is_a_noop() {
        let temp = tempfile::tempdir().unwrap();
        let stable = temp.path().join("sccache-root");
        let mut log = Vec::new();
        let outcome = ensure_in(
            Some(OsStr::new("/usr/bin/ccache")),
            &stable,
            Duration::from_secs(1),
            &mut log,
        );
        assert_eq!(outcome, Outcome::Skipped);
        assert!(log.is_empty());
        assert!(!stable.exists(), "nothing is created when the wrapper is not sccache");
        assert_eq!(ensure_in(None, &stable, Duration::from_secs(1), &mut log), Outcome::Skipped);
    }

    #[test]
    fn the_server_is_started_with_the_stable_root_not_the_run_tmpdir() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = fake(temp.path(), "exit 0");
        let stable = temp.path().join("loom-nt/sccache");
        let mut log = Vec::new();

        let outcome = run(&wrapper, &stable, &mut log);

        assert_eq!(outcome, Outcome::Started);
        let seen = std::fs::read_to_string(temp.path().join("seen")).unwrap();
        assert_eq!(seen.trim(), format!("{} --start-server", stable.display()));
        assert_eq!(std::fs::metadata(&stable).unwrap().permissions().mode() & 0o777, 0o700);
        let line = String::from_utf8(log).unwrap();
        assert!(line.contains("# LOOM_SCCACHE_SERVER started"), "{line}");
    }

    #[test]
    fn address_in_use_means_a_server_is_already_running() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = fake(
            temp.path(),
            "echo 'sccache: error: Server startup failed: Address in use' >&2; exit 2",
        );
        let stable = temp.path().join("sccache-root");
        let mut log = Vec::new();

        assert_eq!(run(&wrapper, &stable, &mut log), Outcome::AlreadyRunning);
        let text = String::from_utf8(log).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("already running"), "{text}");
        assert!(!text.contains("unhealthy"), "{text}");
    }

    #[test]
    fn a_failing_probe_logs_one_clear_line_and_is_not_retried() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = fake(temp.path(), "echo 'sccache: boom' >&2; exit 2");
        let stable = temp.path().join("sccache-root");
        let mut log = Vec::new();

        let outcome = run(&wrapper, &stable, &mut log);

        let Outcome::Unhealthy(reason) = outcome else {
            panic!("expected Unhealthy");
        };
        assert!(reason.contains("sccache: boom"), "{reason}");
        let text = String::from_utf8(log).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("unhealthy"), "{text}");
    }

    #[test]
    fn a_hung_probe_is_killed_at_the_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = fake(temp.path(), "exec sleep 30");
        let stable = temp.path().join("sccache-root");
        let mut log = Vec::new();

        let started = Instant::now();
        let outcome =
            ensure_in(Some(wrapper.as_os_str()), &stable, Duration::from_millis(300), &mut log);

        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(matches!(outcome, Outcome::Unhealthy(r) if r.contains("did not return")));
    }

    #[test]
    fn a_symlinked_root_is_refused_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = fake(temp.path(), "exit 0");
        let elsewhere = temp.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let stable = temp.path().join("sccache-root");
        std::os::unix::fs::symlink(&elsewhere, &stable).unwrap();
        let mut log = Vec::new();

        let outcome = run(&wrapper, &stable, &mut log);

        assert!(matches!(outcome, Outcome::Unhealthy(r) if r.contains("unusable")));
        assert!(!temp.path().join("seen").exists(), "the probe never ran");
    }

    #[test]
    fn a_loose_existing_root_is_tightened() {
        let temp = tempfile::tempdir().unwrap();
        let wrapper = fake(temp.path(), "exit 0");
        let stable = temp.path().join("sccache-root");
        std::fs::create_dir_all(&stable).unwrap();
        std::fs::set_permissions(&stable, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut log = Vec::new();

        assert_eq!(run(&wrapper, &stable, &mut log), Outcome::Started);
        assert_eq!(std::fs::metadata(&stable).unwrap().permissions().mode() & 0o777, 0o700);
    }
}
