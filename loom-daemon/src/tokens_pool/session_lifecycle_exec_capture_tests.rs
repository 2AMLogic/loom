use super::*;
use crate::env_test_support::EnvGuard;
use std::time::Instant;

#[cfg(unix)]
struct FakeDocker {
    _dir: tempfile::TempDir,
    _path: EnvGuard,
}

#[cfg(unix)]
impl FakeDocker {
    fn new(script: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        crate::exec_test_support::write_executable(
            &bin.join("docker"),
            format!("#!/bin/sh\n{script}\n"),
        );
        let rest: Vec<_> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        let path = std::env::join_paths(std::iter::once(bin).chain(rest)).unwrap();
        Self {
            _path: EnvGuard::set("PATH", path).and_set(crate::docker_bin::DOCKER_BIN_ENV, None),
            _dir: dir,
        }
    }
}

fn fixture_is_busy(error: &anyhow::Error) -> bool {
    matches!(error.downcast_ref::<ExecError>(), Some(ExecError::Spawn(io))
        if io.kind() == std::io::ErrorKind::ExecutableFileBusy)
}

fn capture(argv: &[&str], timeout: Duration) -> Result<ExecOutput> {
    let runner = ProcessContainerRunner;
    let mut attempts = 0..1000;
    loop {
        match runner.exec_capture("loom-codex-session-test", argv, timeout) {
            Err(error) if fixture_is_busy(&error) && attempts.next().is_some() => {
                std::thread::sleep(Duration::from_millis(2));
            }
            output => return output,
        }
    }
}

#[test]
#[serial]
fn a_missing_docker_cli_is_unavailable_not_a_failed_exec() {
    let empty = tempfile::tempdir().unwrap();
    let missing = empty.path().join("docker");
    let _docker = EnvGuard::set(crate::docker_bin::DOCKER_BIN_ENV, &missing);
    let output = capture(&["codex", "login", "status"], PROBE_TIMEOUT).unwrap();
    assert_eq!(
        output,
        ExecOutput {
            success: false,
            unavailable: true,
            timed_out: false,
            exit_code: None,
            output: format!(
                "docker CLI could not start: LOOM_DOCKER_BIN={missing:?} does not name an executable file"
            ),
        }
    );
}

#[test]
#[serial]
#[cfg(unix)]
fn a_docker_exec_that_never_answers_times_out() {
    let _docker = FakeDocker::new("printf 'docker: partial\\n'; sleep 30 & wait");
    let started = Instant::now();
    let output = capture(&["codex", "login", "status"], Duration::from_millis(300)).unwrap();
    assert_eq!(
        output,
        ExecOutput {
            success: false,
            unavailable: false,
            timed_out: true,
            exit_code: None,
            output: String::new(),
        }
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a timed-out exec must return at its deadline, took {:?}",
        started.elapsed()
    );
}

#[test]
#[serial]
#[cfg(unix)]
fn exec_output_is_stdout_or_stderr_when_stdout_is_empty_cut_at_max_probe_bytes() {
    const FLOOD: &str = "head -c 8192 /dev/zero | tr '\\0' 'x'";
    let stdout_cap = format!("Logged in\n{}", "x".repeat(MAX_PROBE_BYTES - "Logged in\n".len()));
    let stderr_cap =
        format!("Not logged in\n{}", "x".repeat(MAX_PROBE_BYTES - "Not logged in\n".len()));
    let stdout_over_cap = format!("printf 'Logged in\\n'; {FLOOD}");
    let stderr_over_cap = format!("printf 'Not logged in\\n' >&2; {FLOOD} >&2");
    let cases = [
        ("printf 'Logged in\\n'", "Logged in\n".to_string(), 0),
        ("printf 'Not logged in\\n' >&2", "Not logged in\n".to_string(), 0),
        ("printf 'Logged in\\n'; printf 'noise\\n' >&2", "Logged in\n".to_string(), 0),
        ("printf 'Not logged in\\n' >&2; printf 'noise\\n'", "noise\n".to_string(), 0),
        (FLOOD, "x".repeat(MAX_PROBE_BYTES), 0),
        (stdout_over_cap.as_str(), stdout_cap, 0),
        (stderr_over_cap.as_str(), stderr_cap, 0),
        ("printf 'failed\\n'; exit 3", "failed\n".to_string(), 3),
    ];
    for (script, expected, code) in cases {
        let _docker = FakeDocker::new(script);
        let output = capture(&["codex", "login", "status"], PROBE_TIMEOUT).unwrap();
        assert!(!output.unavailable && !output.timed_out, "script: {script:.40}");
        assert_eq!(output.exit_code, Some(code), "script: {script:.40}");
        assert_eq!(output.success, code == 0, "script: {script:.40}");
        assert_eq!(output.output, expected, "script: {script:.40}");
    }
}

#[test]
#[serial]
#[cfg(unix)]
fn a_docker_exec_that_outfills_the_pipe_buffers_still_completes() {
    const FLOOD: &str = "head -c 262144 /dev/zero | tr '\\0' 'x'";
    let cases = [
        format!("printf 'Logged in\\n'; {FLOOD}"),
        format!("{FLOOD} >&2; printf 'Logged in\\n'"),
    ];
    for script in cases {
        let _docker = FakeDocker::new(&script);
        let output = capture(&["codex", "login", "status"], PROBE_TIMEOUT).unwrap();
        assert!(!output.timed_out, "a 256 KiB write read as a hang");
        assert!(output.success, "script: {script:.40}");
        assert!(output.output.starts_with("Logged in\n"), "script: {script:.40}");
    }
}
