//! A loopback Gitea API stub for the native Gitea reads (#14939).
//!
//! Binds `127.0.0.1:0`, so an origin's `host:port` is a real address, serves
//! scripted replies and records every request with its `Authorization`
//! header. A test asserts from that log that a read reached the repository
//! the origin names, on that port, carrying the session's token.
//!
//! A private repo is modelled by the token: every request is recorded, and the
//! scripted routes are what the instance answers a token it accepts.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// One scripted answer.
#[derive(Clone)]
pub struct Reply {
    pub status: &'static str,
    pub total_count: Option<usize>,
    pub body: String,
}

impl Reply {
    pub fn json(status: &'static str, body: serde_json::Value) -> Self {
        Self {
            status,
            total_count: None,
            body: body.to_string(),
        }
    }

    pub fn text(status: &'static str, body: &str) -> Self {
        Self {
            status,
            total_count: None,
            body: body.to_string(),
        }
    }
}

/// A request as the stub received it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    /// The request line as sent: `GET /api/v1/repos/o/r/pulls/7`.
    pub line: String,
    /// The `Authorization` header, when the request carried one.
    pub authorization: Option<String>,
}

impl Request {
    /// The request's path and query, as sent.
    pub fn target(&self) -> &str {
        self.line.split_whitespace().nth(1).unwrap_or("")
    }
}

#[derive(Default)]
struct State {
    routes: Vec<(String, Reply)>,
    log: Vec<Request>,
}

/// The running stub: its base URL and its request log.
pub struct Stub {
    /// `http://127.0.0.1:<port>`, the port the origin names.
    pub base_url: String,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
}

impl Stub {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the loopback stub");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("stub address");
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (their_state, their_stop) = (state.clone(), stop.clone());
        std::thread::spawn(move || {
            while !their_stop.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => serve(stream, &their_state),
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            base_url: format!("http://{addr}"),
            state,
            stop,
        }
    }

    /// Answer any request whose target ends with `needle`, or whose target
    /// is `needle` followed by a query string (`/pulls/7`; `/pulls` also
    /// answers `/pulls?…`); the first matching route wins, and an unmatched
    /// request is a `404`.
    pub fn route(&self, needle: &str, reply: Reply) -> &Self {
        self.state
            .lock()
            .expect("stub state")
            .routes
            .push((needle.to_string(), reply));
        self
    }

    pub fn requests(&self) -> Vec<Request> {
        self.state.lock().expect("stub state").log.clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<State>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let Ok(reader) = stream.try_clone() else {
        return;
    };
    let mut request = Request {
        line: String::new(),
        authorization: None,
    };
    for line in BufReader::new(reader).lines() {
        let Ok(line) = line else { break };
        if line.is_empty() {
            break;
        }
        if request.line.is_empty() {
            request.line = line;
        } else if let Some((key, value)) = line.split_once(':') {
            if key.trim().eq_ignore_ascii_case("authorization") {
                request.authorization = Some(value.trim().to_string());
            }
        }
    }
    let reply = {
        let mut state = state.lock().expect("stub state");
        let target = request.target();
        let reply = state
            .routes
            .iter()
            .find(|(needle, _)| {
                target.ends_with(needle.as_str()) || target.contains(&format!("{needle}?"))
            })
            .map(|(_, reply)| reply.clone())
            .unwrap_or_else(|| Reply::text("404 Not Found", r#"{"message":"Not Found"}"#));
        state.log.push(request);
        reply
    };
    let count = reply
        .total_count
        .map(|n| format!("X-Total-Count: {n}\r\n"))
        .unwrap_or_default();
    let _ = stream.write_all(
        format!(
            "HTTP/1.1 {}\r\n{count}Content-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{}",
            reply.status,
            reply.body.len(),
            reply.body
        )
        .as_bytes(),
    );
}

/// A `gh` that records its argv and fails: a Gitea read that reaches it is a
/// bug, so a test points `LOOM_GH_BIN` and `PATH` here and asserts the log
/// was never written.
pub fn failing_gh(dir: &Path) -> (PathBuf, PathBuf) {
    let path = dir.join("gh");
    let log = dir.join("gh-calls");
    std::fs::write(&path, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> gh-calls\nexit 97\n")
        .expect("write the failing gh");
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .expect("chmod the failing gh");
    (path, log)
}

/// A checkout whose `origin` is `owner_repo` on `stub`.
pub fn checkout(dir: &Path, stub: &Stub, owner_repo: &str) {
    let origin = format!("{}/{owner_repo}.git", stub.base_url);
    let remotes = [
        vec!["init", "-q"],
        vec!["remote", "add", "origin", origin.as_str()],
    ];
    for args in remotes {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdin(std::process::Stdio::null())
            .output()
            .is_ok_and(|o| o.status.success());
        assert!(ok, "git in {}", dir.display());
    }
    std::fs::create_dir_all(dir.join(".loom")).expect("create .loom");
}

/// `loom-daemon …` in `dir` against the stub's Gitea with `token` as its
/// `GITEA_TOKEN`, and none of the host's forge or fleet configuration: the
/// fleet store's own `gh` read is not this path's to test.
pub fn daemon(dir: &Path, stub: &Stub, token: &str) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    cmd.current_dir(dir)
        .env("LOOM_FORGE_TYPE", "gitea")
        .env("GITEA_URL", &stub.base_url)
        .env("GITEA_TOKEN", token)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env_remove("LOOM_REPO")
        .env_remove("GH_REPO")
        .env_remove("FORGE_TOKEN")
        .env_remove("GITEA_USERNAME")
        .env_remove("LOOM_ALLOW_INSECURE_BASIC_AUTH")
        .env_remove("LOOM_FLEET_REPO");
    cmd
}

/// A pull request as Gitea's `GET /repos/{owner}/{repo}/pulls/{n}` answers it.
pub fn pull(number: u64, state: &str, labels: &[&str], head: &str) -> serde_json::Value {
    serde_json::json!({
        "id": 9000 + number,
        "number": number,
        "state": state,
        "title": format!("PR {number}"),
        "body": "A pull request",
        "html_url": format!("/qual/consumer/pulls/{number}"),
        "labels": labels
            .iter()
            .map(|name| serde_json::json!({"id": 1, "name": name, "color": "0075ca"}))
            .collect::<Vec<_>>(),
        "user": {"login": "alice", "type": "User"},
        "head": {"label": "feature", "ref": "feature/issue-1", "sha": head,
                 "repo": {"id": 5, "full_name": "qual/consumer"}},
        "base": {"label": "main", "ref": "main", "sha": "b".repeat(40),
                 "repo": {"id": 5, "full_name": "qual/consumer"}},
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": "2026-11-21T10:00:00Z",
        "closed_at": null,
        "merged": false,
        "merged_at": null,
        "mergeable": true,
        "draft": false
    })
}

/// A plain issue as Gitea's `GET /repos/{owner}/{repo}/issues/{n}` answers it:
/// no `pull_request` object.
pub fn issue(number: u64, state: &str, labels: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "id": 5000 + number,
        "number": number,
        "state": state,
        "title": format!("Issue {number}"),
        "body": "An issue",
        "html_url": format!("/qual/consumer/issues/{number}"),
        "labels": labels
            .iter()
            .map(|name| serde_json::json!({"id": 1, "name": name, "color": "0075ca"}))
            .collect::<Vec<_>>(),
        "user": {"login": "alice", "type": "User"},
        "created_at": "2026-11-20T10:00:00Z",
        "updated_at": "2026-11-21T10:00:00Z",
        "closed_at": null
    })
}
