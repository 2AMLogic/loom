//! Attribution for the `loom-daemon` children the daemon spawns of itself
//! (#14100).
//!
//! A `loom-daemon` process in the daemon's own cgroup once reached 28-35 GB and
//! was OOM-killed twice on one worker; the kernel line named only a pid and
//! nothing in `daemon.log` mapped that pid to a subcommand. [`track`] closes
//! that gap for the self-spawn sites: it logs the child's `pid`, `site` and
//! argv at start, and its status, wall time and peak RSS at exit, so a kernel
//! `Killed process <pid> (loom-daemon)` line matches one `self_spawn start`
//! line. A child killed by a signal (an OOM kill included) is recorded with the
//! signal in `status`.
//!
//! [`track`] wraps an already-spawned [`std::process::Child`] rather than running it,
//! so callers can keep their own stdio and timeout handling. `output_tracked`
//! handles self-spawns that capture both output streams. Only argv is
//! recorded, never the environment, so credentials set on the command (the
//! per-owner `gh` config) cannot leak. Peak RSS comes from the same `/proc`
//! `VmHWM` sampler the fleet-sync git children use
//! ([`crate::fleet_sync::git_spawn_log`]); it is `None` off Linux.

use std::fmt::Display;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::fleet_sync::git_spawn_log::{argv_of, Sampler};
use crate::proc_exec::{run_bounded_observed, Completion, ExecError};

/// How a tracked child ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitLine {
    /// The child's pid.
    pub pid: u32,
    /// The spawning site.
    pub site: &'static str,
    /// Exit status text (`exit status: 0`, `signal: 9 (SIGKILL)`, `timed out`).
    pub status: String,
    /// Wall time since [`track`].
    pub wall: Duration,
    /// Peak RSS in KiB, when measurable.
    pub peak_rss_kib: Option<u64>,
}

impl ExitLine {
    /// The `daemon.log` line.
    #[must_use]
    pub fn render(&self) -> String {
        format!(
            "self_spawn exit pid={} site={} status={} wall_ms={} peak_rss_kib={}",
            self.pid,
            self.site,
            self.status,
            self.wall.as_millis(),
            self.peak_rss_kib
                .map_or_else(|| "unknown".to_string(), |k| k.to_string()),
        )
    }
}

/// The `daemon.log` line for a spawn.
#[must_use]
pub fn start_line(site: &str, pid: u32, argv: &[String]) -> String {
    format!("self_spawn start pid={pid} site={site} argv={argv:?}")
}

/// A running tracked child. Call [`Tracked::finish`] once it has ended; if it
/// is dropped first (a caller's early return) the exit line says `abandoned`.
pub struct Tracked {
    site: &'static str,
    pid: u32,
    started: Instant,
    sampler: Option<Sampler>,
}

/// Start tracking `child_pid`, just spawned from `cmd` by `site`. Logs the
/// start line (argv only, no environment) and begins sampling peak RSS.
#[must_use]
pub fn track(site: &'static str, cmd: &Command, child_pid: u32) -> Tracked {
    track_argv(site, &argv_of(cmd), child_pid)
}

/// [`track`] from an argv captured earlier, for a caller that has already
/// handed the `Command` away (see [`run_bounded_tracked`]).
fn track_argv(site: &'static str, argv: &[String], child_pid: u32) -> Tracked {
    log::info!("{}", start_line(site, child_pid, argv));
    let sampler = Sampler::for_self_spawn();
    sampler.start(child_pid);
    Tracked {
        site,
        pid: child_pid,
        started: Instant::now(),
        sampler: Some(sampler),
    }
}

/// Run a self-spawn with null stdin and capture stdout and stderr separately.
/// Log its start and exit, returning the output even for a nonzero exit status.
pub(crate) fn output_tracked(site: &'static str, cmd: &mut Command) -> std::io::Result<Output> {
    let child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let tracked = track(site, cmd, child.id());
    let out = child.wait_with_output()?;
    tracked.finish(out.status);
    Ok(out)
}

/// Run a self-spawn under `timeout` through [`crate::proc_exec`]: its pipes are
/// drained concurrently and a deadline terminates the child's whole process
/// group, so a `gh` or `npm` it forked does not outlive it. Logs the start and
/// exit lines [`track`] does (`timed out` for a deadline); a command that could
/// not be spawned logs nothing, as nothing ran.
///
/// # Errors
///
/// As [`crate::proc_exec::run_bounded_observed`].
pub(crate) fn run_bounded_tracked(
    site: &'static str,
    cmd: Command,
    timeout: Duration,
) -> Result<Completion, ExecError> {
    let argv = argv_of(&cmd);
    let mut tracked = None;
    let result = run_bounded_observed(cmd, timeout, |pid| {
        tracked = Some(track_argv(site, &argv, pid));
    });
    if let Some(tracked) = tracked {
        match &result {
            Ok(Completion::Exited(out)) => drop(tracked.finish(out.status)),
            Ok(Completion::TimedOut { .. }) => drop(tracked.finish("timed out")),
            Err(e) => drop(tracked.finish(e)),
        }
    }
    result
}

impl Tracked {
    /// Log and return the exit line. `status` is the child's `ExitStatus`
    /// (whose text names a fatal signal) or a caller note such as `timed out`.
    pub fn finish(mut self, status: impl Display) -> ExitLine {
        self.end(status.to_string())
    }

    fn end(&mut self, status: String) -> ExitLine {
        let peak_rss_kib = self.sampler.take().and_then(|s| s.stop());
        let line = ExitLine {
            pid: self.pid,
            site: self.site,
            status,
            wall: self.started.elapsed(),
            peak_rss_kib,
        };
        log::info!("{}", line.render());
        line
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        if self.sampler.is_some() {
            self.end("abandoned".to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        cmd
    }

    #[test]
    fn output_tracked_captures_output_and_logs_start_and_exit() {
        let mut cmd = sh("cat; printf stdout; printf stderr >&2; exit 3");
        // Any inherited input would be copied into stdout by `cat`.
        cmd.stdin(std::fs::File::open(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml")).unwrap());
        let records = crate::test_log_capture::capture_logs(|| {
            let out = output_tracked("test_capture", &mut cmd).unwrap();
            assert_eq!(out.stdout, b"stdout");
            assert_eq!(out.stderr, b"stderr");
            assert_eq!(out.status.code(), Some(3));
        });
        assert!(
            records
                .iter()
                .any(|(_, line)| line.starts_with("self_spawn start pid=")
                    && line.contains(" site=test_capture ")),
            "{records:?}"
        );
        assert!(
            records
                .iter()
                .any(|(_, line)| line.starts_with("self_spawn exit pid=")
                    && line.contains(" site=test_capture status=exit status: 3 ")),
            "{records:?}"
        );
    }

    fn lines_for(records: &[(log::Level, String)], site: &str) -> Vec<String> {
        records
            .iter()
            .map(|(_, line)| line.clone())
            .filter(|line| line.contains(&format!(" site={site} ")))
            .collect()
    }

    #[test]
    fn run_bounded_tracked_logs_start_and_exit_of_a_completed_child() {
        let records = crate::test_log_capture::capture_logs(|| {
            let out = run_bounded_tracked(
                "test_bounded",
                sh("printf out; printf err >&2; exit 3"),
                Duration::from_secs(5),
            )
            .unwrap()
            .output()
            .unwrap();
            assert_eq!(out.stdout, b"out");
            assert_eq!(out.stderr, b"err");
            assert_eq!(out.status.code(), Some(3));
        });
        let lines = lines_for(&records, "test_bounded");
        assert!(lines.iter().any(|l| l.starts_with("self_spawn start pid=")), "{lines:?}");
        assert!(
            lines
                .iter()
                .any(|l| l.starts_with("self_spawn exit pid=")
                    && l.contains("status=exit status: 3 ")),
            "{lines:?}"
        );
    }

    #[test]
    fn run_bounded_tracked_logs_a_deadline_as_timed_out_and_ends_the_group() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let records = crate::test_log_capture::capture_logs(|| {
            let done = run_bounded_tracked(
                "test_bounded",
                sh(&crate::exec_test_support::hung_with_grandchild(&pidfile)),
                Duration::from_millis(600),
            )
            .unwrap();
            assert!(matches!(done, Completion::TimedOut { .. }), "{done:?}");
        });
        let lines = lines_for(&records, "test_bounded");
        assert!(lines.iter().any(|l| l.contains("status=timed out ")), "{lines:?}");
        assert!(!lines.iter().any(|l| l.contains("abandoned")), "{lines:?}");
        assert!(
            !crate::exec_test_support::grandchild_outlived(&pidfile),
            "the grandchild outlived the deadline"
        );
    }

    #[test]
    fn run_bounded_tracked_logs_nothing_for_a_spawn_failure() {
        let records = crate::test_log_capture::capture_logs(|| {
            let err = run_bounded_tracked(
                "test_bounded",
                Command::new("/nonexistent/loom-daemon"),
                Duration::from_secs(1),
            )
            .unwrap_err();
            assert!(matches!(err, ExecError::Spawn(_)), "{err:?}");
        });
        assert!(lines_for(&records, "test_bounded").is_empty(), "{records:?}");
    }

    #[test]
    fn output_tracked_returns_spawn_error() {
        let mut cmd = Command::new("/nonexistent/loom-daemon");
        let err = output_tracked("test_capture", &mut cmd).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[test]
    fn start_line_names_pid_site_and_argv() {
        let argv = vec!["loom-daemon".to_string(), "tokens".to_string()];
        let line = start_line("token_ranking_refresh", 4242, &argv);
        assert!(line.contains("pid=4242"), "{line}");
        assert!(line.contains("site=token_ranking_refresh"), "{line}");
        assert!(line.contains(r#"argv=["loom-daemon", "tokens"]"#), "{line}");
    }

    #[test]
    fn exit_line_records_status_wall_and_peak_rss() {
        let mut cmd = sh("sleep 0.3");
        let mut child = cmd.spawn().unwrap();
        let tracked = track("test", &cmd, child.id());
        let status = child.wait().unwrap();
        let exit = tracked.finish(status);
        assert_eq!(exit.pid, child.id());
        assert!(exit.status.contains('0'), "{exit:?}");
        assert!(exit.wall >= Duration::from_millis(250), "{exit:?}");
        #[cfg(target_os = "linux")]
        assert!(exit.peak_rss_kib.is_some_and(|k| k > 0), "{exit:?}");
        let line = exit.render();
        assert!(line.starts_with("self_spawn exit pid="), "{line}");
        assert!(line.contains("peak_rss_kib="), "{line}");
    }

    #[test]
    fn signal_killed_child_still_produces_an_exit_line() {
        let mut cmd = sh("kill -KILL $$");
        let mut child = cmd.spawn().unwrap();
        let tracked = track("test", &cmd, child.id());
        let status = child.wait().unwrap();
        assert_eq!(status.signal(), Some(9));
        let exit = tracked.finish(status);
        assert!(exit.status.contains("SIGKILL"), "{exit:?}");
    }

    #[test]
    fn child_gone_before_first_sample_still_emits_a_line() {
        let mut cmd = sh("exit 0");
        let mut child = cmd.spawn().unwrap();
        let status = child.wait().unwrap();
        let exit = track("test", &cmd, child.id()).finish(status);
        assert!(exit.render().contains("peak_rss_kib="), "{exit:?}");
    }

    #[test]
    fn environment_is_never_logged() {
        let mut cmd = sh("sleep 0.1");
        cmd.env("SECRET_TOKEN", "hunter2");
        let mut child = cmd.spawn().unwrap();
        let start = start_line("test", child.id(), &argv_of(&cmd));
        let tracked = track("test", &cmd, child.id());
        let exit = tracked.finish(child.wait().unwrap());
        assert!(!start.contains("hunter2") && !start.contains("SECRET_TOKEN"));
        assert!(!exit.render().contains("hunter2"));
    }

    #[test]
    fn dropped_tracker_logs_abandoned() {
        let mut cmd = sh("exit 0");
        let mut child = cmd.spawn().unwrap();
        let tracked = track("test", &cmd, child.id());
        child.wait().unwrap();
        drop(tracked); // must not panic
    }
}
