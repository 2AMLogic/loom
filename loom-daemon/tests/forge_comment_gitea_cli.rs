//! CLI contract for `post-comment.sh` / `forge comment` on a Gitea forge
//! (#14940): the comment chokepoint must post through
//! `POST /api/v1/repos/{owner}/{repo}/issues/{N}/comments` with the
//! connection the write-scope probe authorized, and never invoke `gh`.
//!
//! Every case runs the **real binary through the real shell wrapper**
//! (`defaults/scripts/post-comment.sh`, a pure exec of `forge comment`) from
//! a temporary Gitea checkout whose `origin` carries an explicit port — the
//! exact shape the single-stage qualification observed failing — against a
//! loopback Gitea server and a recording `gh` stub
//! (`LOOM_GH_BIN` / `LOOM_DAEMON_SELF_BIN`, the same seams
//! `forge_check_open_pr_cli.rs` uses). The request log lets each case assert
//! the permission request, the exact POST path, the decoded body, and the
//! request count, so a regression to the `gh` transport (or a second,
//! differently-resolved repository) cannot pass silently.
//!
//! The transport's unit mechanics (footer idempotence through the wire,
//! credential redaction, backend selection) are pinned in
//! `src/forge_comment/tests.rs`; this file pins the CLI surface around them.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use loom_daemon::forge_comment::append_dashboard_footer;

const NWO: &str = "loomtest/loom-consumer";
const NUMBER: u64 = 7;
/// The URL the stub server supplies for the created comment — any URL the
/// server prints is the CLI's stdout; it need not point at the loopback.
const COMMENT_URL: &str = "http://gitea.test/loomtest/loom-consumer/issues/7#issuecomment-11";

/// What the loopback server answers for each route it serves.
struct Routes {
    /// `GET /api/v1/repos/{NWO}` status + body (the write-scope probe).
    probe: (String, String),
    /// `POST /api/v1/repos/{NWO}/issues/{N}/comments` status + body.
    post: (String, String),
}

/// One loopback Gitea server: serves until [`Loopback::finish`] is called
/// (30 s backstop), routing by request method and recording every raw
/// request so a case can assert paths, headers, bodies and counts.
struct Loopback {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

impl Loopback {
    fn start(routes: Routes) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let handle = {
            let requests = Arc::clone(&requests);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let deadline = Instant::now() + Duration::from_secs(30);
                while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            let Some(request) = read_request(&mut stream) else {
                                continue;
                            };
                            let method = request
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .split(' ')
                                .next()
                                .unwrap_or_default()
                                .to_string();
                            let (status, body) = if method == "GET" {
                                routes.probe.clone()
                            } else {
                                routes.post.clone()
                            };
                            requests.lock().unwrap().push(request);
                            let response = format!(
                                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
                                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            );
                            let _ = stream.write_all(response.as_bytes());
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(15));
                        }
                        Err(_) => return,
                    }
                }
            })
        };
        Self {
            base_url,
            requests,
            stop,
            handle,
        }
    }

    /// Stop serving and return every recorded request.
    fn finish(self) -> Vec<String> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap();
        self.requests.lock().unwrap().clone()
    }
}

/// Read one HTTP request (headers + Content-Length body) from `stream`.
fn read_request(stream: &mut std::net::TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    let mut request = String::new();
    let mut buf = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(10);
    while request.len() < 64 * 1024 && Instant::now() < deadline {
        let header_end = request.find("\r\n\r\n").map(|i| i + 4);
        let complete = header_end.is_some_and(|start| {
            let cl = request
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().to_string())
                })
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            request.len() >= start + cl
        });
        if complete {
            return Some(request);
        }
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => return (!request.is_empty()).then_some(request),
            Ok(n) => request.push_str(&String::from_utf8_lossy(&buf[..n])),
        }
    }
    (!request.is_empty()).then_some(request)
}

/// A temporary Gitea checkout: a git repo whose `origin` carries an explicit
/// port (the qualification's shape) and whose `.loom/config.json` selects
/// the Gitea forge, with `token` unless the case is the no-credential one.
fn gitea_checkout(base_url: &str, token: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", &format!("{base_url}/{NWO}.git")],
    ] {
        let ok = Command::new("git")
            .args(&args)
            .current_dir(dir.path())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {:?}", args);
    }
    let loom = dir.path().join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    let gitea = match token {
        Some(t) => serde_json::json!({ "url": base_url, "token": t }),
        None => serde_json::json!({ "url": base_url }),
    };
    std::fs::write(
        loom.join("config.json"),
        serde_json::json!({ "forge": { "type": "gitea", "gitea": gitea } }).to_string(),
    )
    .unwrap();
    dir
}

/// A `gh` stand-in that records every invocation and exits non-zero: on the
/// Gitea path the log must stay empty, and had `gh` run it would fail loudly.
fn recording_gh_stub(dir: &Path) -> PathBuf {
    let path = dir.join("gh");
    std::fs::write(
        &path,
        format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", dir.join("gh.log").display()),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    path
}

/// Write the comment body every case posts: multiline markdown with a role
/// marker, backticks and quotes — the shapes `-f`-style quoting corrupts.
fn write_body(checkout: &Path) -> String {
    let body = "builder note\n\n- with `markdown` and \"quotes\"\n<!-- loom:builder-note -->\n";
    std::fs::write(checkout.join("body.md"), body).unwrap();
    body.to_string()
}

/// Run `post-comment.sh <args…>` (the pure-exec shell wrapper over `forge
/// comment`) from `checkout`, pinning every ambient influence it reads.
fn run_wrapper(checkout: &Path, gh_stub: &Path, args: &[&str]) -> Output {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/post-comment.sh");
    let mut cmd = Command::new("bash");
    cmd.arg(&script)
        .args(args)
        .current_dir(checkout)
        .env("LOOM_DAEMON_SELF_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
        .env("LOOM_GH_BIN", gh_stub)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_WORKSPACES_PATH", checkout.join("workspaces.json"));
    for k in [
        "GITEA_TOKEN",
        "GITEA_URL",
        "GITEA_USERNAME",
        "FORGE_TOKEN",
        "LOOM_FORGE_TYPE",
        "LOOM_DASHBOARD_URL",
        "LOOM_REPO",
        "GH_REPO",
        "GH_TOKEN",
        "GITHUB_TOKEN",
    ] {
        cmd.env_remove(k);
    }
    cmd.output().unwrap()
}

fn probe_write_ok() -> (String, String) {
    (
        "200 OK".into(),
        r#"{"full_name":"loomtest/loom-consumer","permissions":{"admin":false,"push":true,"pull":true}}"#.into(),
    )
}

fn probe_read_only() -> (String, String) {
    (
        "200 OK".into(),
        r#"{"permissions":{"admin":false,"push":false,"pull":true}}"#.into(),
    )
}

fn comment_created() -> (String, String) {
    ("201 Created".into(), format!(r#"{{"id":11,"html_url":"{COMMENT_URL}"}}"#))
}

fn post_forbidden() -> (String, String) {
    ("403 Forbidden".into(), r#"{"message":"permission denied"}"#.into())
}

/// Happy path, no `--repo`: the write check resolves the checkout's origin,
/// the probe authorizes it, and the one POST lands on the Gitea comments
/// endpoint with the footered body — `gh` never runs, and the CLI prints the
/// URL the server supplied.
#[test]
fn gitea_comment_posts_from_a_gitea_checkout_without_repo() {
    let server = Loopback::start(Routes {
        probe: probe_write_ok(),
        post: comment_created(),
    });
    let checkout = gitea_checkout(&server.base_url, Some("stub-token"));
    let stub = tempfile::tempdir().unwrap();
    let gh = recording_gh_stub(stub.path());
    let body = write_body(checkout.path());

    let out = run_wrapper(checkout.path(), &gh, &[&NUMBER.to_string(), "--body-file", "body.md"]);
    let requests = server.finish();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(stdout.trim(), COMMENT_URL, "stdout: {stdout}");

    assert_eq!(requests.len(), 2, "one probe + one POST: {requests:#?}");
    assert!(
        requests[0].starts_with(&format!("GET /api/v1/repos/{NWO} HTTP/1.1")),
        "the permission request names the origin's repository: {}",
        requests[0]
    );
    assert!(
        requests[0].contains("Authorization: token stub-token"),
        "the probe authenticates with the configured credential: {}",
        requests[0]
    );
    let post = &requests[1];
    assert!(
        post.starts_with(&format!("POST /api/v1/repos/{NWO}/issues/{NUMBER}/comments HTTP/1.1")),
        "the exact Gitea comments endpoint: {post}"
    );
    assert!(
        post.contains("Authorization: token stub-token"),
        "the write carries the credential the check approved: {post}"
    );
    let posted: serde_json::Value =
        serde_json::from_str(post.split_once("\r\n\r\n").map_or("", |(_, b)| b))
            .expect("the posted body is JSON");
    assert_eq!(
        posted["body"].as_str().unwrap(),
        append_dashboard_footer(NWO, NUMBER, false, &body),
        "multiline markdown and the role marker survive byte-for-byte, footer applied once"
    );
    assert!(!stub.path().join("gh.log").exists(), "the Gitea CLI path never invokes gh");
}

/// An explicit managed `--repo` and `--pr`: same endpoint, and the footer's
/// link says `/pull/N`.
#[test]
fn gitea_comment_with_explicit_repo_and_pr() {
    let server = Loopback::start(Routes {
        probe: probe_write_ok(),
        post: comment_created(),
    });
    let checkout = gitea_checkout(&server.base_url, Some("stub-token"));
    let stub = tempfile::tempdir().unwrap();
    let gh = recording_gh_stub(stub.path());
    let body = write_body(checkout.path());

    let out = run_wrapper(
        checkout.path(),
        &gh,
        &[
            &NUMBER.to_string(),
            "--body-file",
            "body.md",
            "--repo",
            NWO,
            "--pr",
        ],
    );
    let requests = server.finish();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(requests.len(), 2, "one probe + one POST: {requests:#?}");
    let post = &requests[1];
    assert!(
        post.starts_with(&format!("POST /api/v1/repos/{NWO}/issues/{NUMBER}/comments HTTP/1.1")),
        "a PR is an issue for comments: {post}"
    );
    let posted: serde_json::Value =
        serde_json::from_str(post.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap();
    assert_eq!(
        posted["body"].as_str().unwrap(),
        append_dashboard_footer(NWO, NUMBER, true, &body),
        "--pr picks /pull/N in the dashboard link"
    );
    assert!(posted["body"].as_str().unwrap().contains("/pull/7"));
    assert!(!stub.path().join("gh.log").exists());
}

/// A body that already carries the footer posts unchanged — idempotence
/// through the whole CLI path.
#[test]
fn gitea_comment_does_not_double_an_existing_footer() {
    let server = Loopback::start(Routes {
        probe: probe_write_ok(),
        post: comment_created(),
    });
    let checkout = gitea_checkout(&server.base_url, Some("stub-token"));
    let stub = tempfile::tempdir().unwrap();
    let gh = recording_gh_stub(stub.path());
    let once = append_dashboard_footer(NWO, NUMBER, false, "already footered");
    std::fs::write(checkout.path().join("body.md"), &once).unwrap();

    let out = run_wrapper(checkout.path(), &gh, &[&NUMBER.to_string(), "--body-file", "body.md"]);
    let requests = server.finish();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert_eq!(requests.len(), 2, "one probe + one POST: {requests:#?}");
    let post = &requests[1];
    let posted: serde_json::Value =
        serde_json::from_str(post.split_once("\r\n\r\n").map_or("", |(_, b)| b)).unwrap();
    assert_eq!(posted["body"].as_str().unwrap(), once);
}

/// No credential configured: exit nonzero before any request (the connection
/// cannot resolve), `gh` never invoked, and the credential is not echoed.
#[test]
fn gitea_missing_credential_refuses_before_any_request() {
    let server = Loopback::start(Routes {
        probe: probe_write_ok(),
        post: comment_created(),
    });
    let checkout = gitea_checkout(&server.base_url, None);
    let stub = tempfile::tempdir().unwrap();
    let gh = recording_gh_stub(stub.path());
    write_body(checkout.path());

    let out = run_wrapper(checkout.path(), &gh, &[&NUMBER.to_string(), "--body-file", "body.md"]);
    let requests = server.finish();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "must refuse: {stderr}");
    assert!(stderr.contains("token is required"), "names the missing credential: {stderr}");
    assert!(
        requests.is_empty(),
        "no probe, no POST — the refusal precedes any request: {requests:#?}"
    );
    assert!(!stub.path().join("gh.log").exists(), "no gh either");
}

/// Read-only permission: the probe's one GET answers `pull` only, the write
/// is refused, and the comment POST never happens.
#[test]
fn gitea_read_only_permission_refuses_before_the_post() {
    let server = Loopback::start(Routes {
        probe: probe_read_only(),
        post: comment_created(),
    });
    let checkout = gitea_checkout(&server.base_url, Some("stub-token"));
    let stub = tempfile::tempdir().unwrap();
    let gh = recording_gh_stub(stub.path());
    write_body(checkout.path());

    let out = run_wrapper(checkout.path(), &gh, &[&NUMBER.to_string(), "--body-file", "body.md"]);
    let requests = server.finish();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "must refuse: {stderr}");
    assert!(stderr.contains("cannot write"), "names the denied write permission: {stderr}");
    assert_eq!(requests.len(), 1, "the permission request only, never the POST: {requests:#?}");
    assert!(requests[0].starts_with("GET "), "the one request is the probe");
    assert!(!stub.path().join("gh.log").exists());
}

/// A rejected POST reports failure: the probe authorizes, the API answers
/// 403, the CLI exits nonzero.
#[test]
fn gitea_rejected_post_reports_failure() {
    let server = Loopback::start(Routes {
        probe: probe_write_ok(),
        post: post_forbidden(),
    });
    let checkout = gitea_checkout(&server.base_url, Some("stub-token"));
    let stub = tempfile::tempdir().unwrap();
    let gh = recording_gh_stub(stub.path());
    write_body(checkout.path());

    let out = run_wrapper(checkout.path(), &gh, &[&NUMBER.to_string(), "--body-file", "body.md"]);
    let requests = server.finish();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_ne!(out.status.code(), Some(0), "a 403 is a failure: {stderr}");
    assert!(stderr.contains("403"), "names the status: {stderr}");
    assert_eq!(requests.len(), 2, "the POST was attempted: {requests:#?}");
    assert!(!stub.path().join("gh.log").exists());
}
