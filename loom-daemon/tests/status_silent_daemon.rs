//! A live process whose socket answers neither status nor Ping should get the
//! unreachable diagnostic and its drained-restart advice (#15316).

use std::fs;
use std::io::BufReader;
use std::os::unix::net::UnixListener;
use std::process::Command;

use loom_daemon::daemon_install_state::EXIT_ALIVE_BUT_UNRESPONSIVE;
use loom_daemon::types::Request;

#[test]
fn silent_daemon_reports_both_failures_and_suggests_drained_restart() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("daemon.sock");
    // Leave both connections in the listener backlog until the CLI times out.
    let listener = UnixListener::bind(&socket).unwrap();
    let pid_file = dir.path().join("daemon.pid");
    fs::write(&pid_file, std::process::id().to_string()).unwrap();
    fs::write(
        dir.path().join("autonomy-desired"),
        format!("use_launchd=false\npid_file={}\n", pid_file.display()),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .args(["status", "--timeout-secs", "1"])
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", dir.path())
        .env("LOOM_SOCKET_PATH", &socket)
        .env("LOOM_DAEMON_STARTUP_GRACE_SECS", "0")
        .current_dir(dir.path())
        .output()
        .unwrap();

    let banner = String::from_utf8(output.stderr).unwrap();
    assert_eq!(output.status.code(), Some(EXIT_ALIVE_BUT_UNRESPONSIVE), "{banner}");
    assert!(banner.contains("not responding over IPC"), "{banner}");
    assert!(banner.contains("second request (Ping): failed too"), "{banner}");
    assert!(banner.contains("loom-daemon restart --drain"), "{banner}");

    // Check the actual wire requests even if a future banner prints both names unconditionally.
    listener.set_nonblocking(true).unwrap();
    let next_request = || {
        let (stream, _) = listener.accept().expect("missing IPC request");
        serde_json::from_reader::<_, Request>(BufReader::new(stream)).unwrap()
    };
    assert!(matches!(next_request(), Request::DaemonStatus));
    assert!(matches!(next_request(), Request::Ping));
}
