//! #14935: private-seat preparation reads through the fleet Git read relay,
//! pinned against a fake relay (and a separate fake canonical remote) on
//! loopback — which endpoint received each operation, which credential
//! class it saw, and what a refusal, an unreachable relay and the push
//! destination do. Keys are synthetic; nothing here touches a real forge.

use super::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::test_git_helpers::{configure, run as git};

use super::git_relay::{RelayRead, RelayRoute, RELAY_HEADER_ENV, RELAY_URL_ENV};

const SYNTHETIC_KEY: &str = "synthetic-relay-key-14935";
const SYNTHETIC_HEADER: &str = "Authorization: Bearer synthetic-relay-key-14935";

/// One request the fake server received.
#[derive(Clone, Debug, PartialEq)]
struct Seen {
    method: String,
    /// Path with its query string.
    target: String,
    authorization: Option<String>,
}

impl Seen {
    fn is_probe(&self) -> bool {
        self.method == "GET"
            && self.target.ends_with("/info/refs?service=git-upload-pack")
            && self.target.contains(".git")
    }

    fn has(&self, needle: &str) -> bool {
        self.target.contains(needle)
    }
}

/// What a fake git HTTP endpoint serves: upload-pack reads from a backing
/// repository, receive-pack writes (`None` refuses them the way the relay
/// refuses every profile), or one fixed refusal for everything.
enum Surface {
    Git {
        backing: std::path::PathBuf,
        receive: bool,
    },
    Refuse(&'static str),
}

/// A fake git smart-HTTP endpoint on loopback, speaking protocol v0 with
/// Content-Length bodies (the shape `git http-backend` served before v2 and
/// every git still falls back to). One worker thread; every request is
/// recorded with the exact `Authorization` header it carried.
struct FakeEndpoint {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
}

impl FakeEndpoint {
    fn start(surface: Surface) -> Self {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind loopback");
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let addr = listener.local_addr().expect("local address");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen_t = seen.clone();
        let stop_t = stop.clone();
        std::thread::Builder::new()
            .name("fake-git-endpoint".into())
            .spawn(move || loop {
                if stop_t.load(Ordering::Relaxed) {
                    return;
                }
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = stream.set_nonblocking(false);
                        serve_connection(stream, &surface, &seen_t);
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            })
            .expect("spawn endpoint thread");
        Self { addr, seen, stop }
    }

    /// `http://<host>/<path>` for this endpoint — the loopback test seam
    /// the relay contract accepts (`git_relay::is_loopback`).
    fn url(&self, path: &str) -> String {
        format!("http://{}/{}", self.addr, path)
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().expect("log lock").clone()
    }

    fn receive_pack_count(&self) -> usize {
        self.requests()
            .iter()
            .filter(|r| r.has("git-receive-pack"))
            .count()
    }
}

impl Drop for FakeEndpoint {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// One parsed HTTP request: method, target with query, lowercased headers,
/// body.
struct ParsedRequest {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

/// Read one CRLF-terminated header block, then its Content-Length body.
fn read_request(stream: &mut TcpStream) -> Option<ParsedRequest> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while head.len() < 32_768 && !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return None,
            Ok(_) => head.push(byte[0]),
        }
    }
    let text = String::from_utf8_lossy(&head).into_owned();
    let mut lines = text.lines();
    let request = lines.next()?.to_owned();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 && stream.read_exact(&mut body).is_err() {
        return None;
    }
    let mut parts = request.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    Some(ParsedRequest {
        method,
        target,
        headers,
        body,
    })
}

/// Serve every request on one connection (HTTP/1.1 keep-alive).
fn serve_connection(mut stream: TcpStream, surface: &Surface, log: &Mutex<Vec<Seen>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    loop {
        let Some(request) = read_request(&mut stream) else {
            return;
        };
        let ParsedRequest {
            method,
            target,
            headers,
            body,
        } = request;
        let authorization = headers
            .iter()
            .find(|(n, _)| n == "authorization")
            .map(|(_, v)| v.clone());
        log.lock().expect("log lock").push(Seen {
            method: method.clone(),
            target: target.clone(),
            authorization,
        });
        if headers
            .iter()
            .any(|(n, v)| n == "expect" && v.eq_ignore_ascii_case("100-continue"))
        {
            let _ = stream.write_all(b"HTTP/1.1 100 Continue\r\n\r\n");
        }
        let (status, reason, content_type, out) = respond(surface, &method, &target, &body);
        let head = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            out.len()
        );
        if stream.write_all(head.as_bytes()).is_err() || stream.write_all(&out).is_err() {
            return;
        }
    }
}

/// One smart-HTTP answer: a refusal, a v0 advertisement, or a stateless-rpc
/// round against the backing repository.
fn respond(
    surface: &Surface,
    method: &str,
    target: &str,
    body: &[u8],
) -> (u16, &'static str, String, Vec<u8>) {
    let json = |status: u16, message: &str| {
        (
            status,
            if status == 403 {
                "Forbidden"
            } else {
                "Not Found"
            },
            "application/json".to_owned(),
            format!("{{\"message\":\"{message}\"}}").into_bytes(),
        )
    };
    let Surface::Git { backing, receive } = surface else {
        let Surface::Refuse(message) = surface else {
            unreachable!()
        };
        return json(403, message);
    };
    let path = target.split('?').next().unwrap_or(target);
    let service = target
        .split_once('?')
        .and_then(|(_, query)| query.split('&').find_map(|p| p.strip_prefix("service=")))
        .map(str::to_owned);
    if method == "GET" && path.ends_with("/info/refs") {
        match service.as_deref() {
            Some("git-upload-pack") => {
                let adv = advertise("upload-pack", backing);
                (200, "OK", "application/x-git-upload-pack-advertisement".into(), adv)
            }
            Some("git-receive-pack") if *receive => {
                let adv = advertise("receive-pack", backing);
                (200, "OK", "application/x-git-receive-pack-advertisement".into(), adv)
            }
            _ => json(403, "write_not_permitted"),
        }
    } else if method == "POST" && path.ends_with("/git-upload-pack") {
        let out = stateless_rpc(&["git", "upload-pack", "--stateless-rpc"], backing, body);
        (200, "OK", "application/x-git-upload-pack-result".into(), out)
    } else if method == "POST" && path.ends_with("/git-receive-pack") && *receive {
        let out = stateless_rpc(&["git", "receive-pack", "--stateless-rpc"], backing, body);
        (200, "OK", "application/x-git-receive-pack-result".into(), out)
    } else {
        json(404, "unsupported_surface")
    }
}

/// The v0 `# service=…` flush-pkt followed by the subprocess advertisement.
fn advertise(service: &str, backing: &std::path::Path) -> Vec<u8> {
    let mut out = format!("# service=git-{service}\n").into_bytes();
    let prefix = format!("{:04x}", out.len() + 4);
    let mut framed = prefix.into_bytes();
    framed.append(&mut out);
    framed.extend_from_slice(b"0000");
    framed.extend_from_slice(&stateless_rpc(
        &["git", service, "--advertise-refs", "--stateless-rpc"],
        backing,
        b"",
    ));
    framed
}

fn stateless_rpc(argv: &[&str], backing: &std::path::Path, body: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new(argv[0])
        .args(&argv[1..])
        .arg(backing)
        .env_remove("GIT_DIR")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn git");
    child
        .stdin
        .as_mut()
        .expect("git stdin")
        .write_all(body)
        .expect("feed git");
    let out = child.wait_with_output().expect("git answers");
    assert!(
        out.status.success(),
        "git {} failed: {}",
        argv.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// A volume prepared through a fake relay, plus the separate canonical
/// remote its clone keeps as `origin`.
struct RelayFixture {
    _temp: tempfile::TempDir,
    /// The working clone the fixture commits new revisions in.
    relay_backing_work: std::path::PathBuf,
    /// The bare repository the relay serves reads from.
    relay_backing_bare: std::path::PathBuf,
    volume: std::path::PathBuf,
    relay: FakeEndpoint,
    canonical: FakeEndpoint,
    canonical_url: String,
    relay_url: String,
}

impl RelayFixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let backing_work = temp.path().join("relay-backing");
        std::fs::create_dir(&backing_work).expect("backing dir");
        git(&backing_work, &["init", "-q", "-b", "main"]);
        configure(&backing_work);
        std::fs::write(backing_work.join("fixture"), "base").expect("write");
        git(&backing_work, &["add", "."]);
        git(&backing_work, &["commit", "-q", "-m", "base"]);
        let backing = temp.path().join("relay-backing.git");
        git(
            &backing_work,
            &[
                "clone",
                "-q",
                "--bare",
                &backing_work.display().to_string(),
                &backing.display().to_string(),
            ],
        );
        // The canonical remote: a separate endpoint whose repository starts
        // empty, because reads never reach it — only a push does.
        let canonical_backing = temp.path().join("canonical.git");
        git(
            temp.path(),
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                &canonical_backing.display().to_string(),
            ],
        );
        let relay = FakeEndpoint::start(Surface::Git {
            backing: backing.clone(),
            receive: false,
        });
        let canonical = FakeEndpoint::start(Surface::Git {
            backing: canonical_backing,
            receive: true,
        });
        let canonical_url = canonical.url("acme/widget");
        let relay_url = relay.url("acme/widget.git");
        let volume = temp.path().join("volume");
        std::fs::create_dir(&volume).expect("volume dir");
        Self {
            _temp: temp,
            relay_backing_work: backing_work,
            relay_backing_bare: backing.clone(),
            volume,
            relay,
            canonical,
            canonical_url,
            relay_url,
        }
    }

    fn args(&self) -> repository::PrepareArgs {
        repository::PrepareArgs {
            account: "fixture".into(),
            repository: self.canonical_url.clone(),
            base: "main".into(),
            container_id: "test-container".into(),
            branch: None,
            slug: None,
        }
    }

    /// The relay route the daemon would hand the exec, for this fixture.
    fn relay(&self) -> RelayRead {
        let vars: HashMap<&str, &str> = [
            (RELAY_URL_ENV, self.relay_url.as_str()),
            (RELAY_HEADER_ENV, SYNTHETIC_HEADER),
        ]
        .into_iter()
        .collect();
        RelayRead::from_env_for(&self.canonical_url, &|name| {
            vars.get(name).map(|v| std::ffi::OsString::from(v))
        })
        .expect("relay contract holds")
        .expect("relay route present")
    }

    fn prepare(
        &self,
        relay: Option<&RelayRead>,
    ) -> std::result::Result<repository::PrepareOutcome, repository::PrepareFailure> {
        repository::prepare_with_relay(&self.volume, &self.args(), &|_, _, _, _| false, relay)
    }

    /// Add one commit to the repository the relay serves.
    fn advance_relay_backing(&self, name: &str) -> String {
        let revision =
            crate::test_git_helpers::commit_file(&self.relay_backing_work, name, name, name);
        git(
            &self.relay_backing_work,
            &[
                "push",
                "-q",
                &self.relay_backing_bare.display().to_string(),
                "main:main",
            ],
        );
        revision
    }
}

// ---------------------------------------------------------------------------
// The relay path.
// ---------------------------------------------------------------------------

#[test]
fn initial_clone_and_later_fetch_read_through_the_relay_with_only_its_key() {
    let f = RelayFixture::new();
    let relay = f.relay();

    let first = f.prepare(Some(&relay)).expect("relay clone prepares");
    let clone = f.volume.join("repo");
    assert_eq!(
        git(&clone, &["remote", "get-url", "origin"]),
        f.canonical_url,
        "the clone keeps its canonical origin"
    );

    // Every read hit the relay endpoint, each carrying the relay's Bearer
    // key — and only that credential class.
    let seen = f.relay.requests();
    assert!(seen.iter().any(|r| r.is_probe()), "the read endpoint was probed: {seen:?}");
    assert!(
        seen.iter()
            .any(|r| r.method == "POST" && r.has("git-upload-pack")),
        "the clone negotiated upload-pack with the relay: {seen:?}"
    );
    for request in &seen {
        assert_eq!(
            request.authorization.as_deref(),
            Some("Bearer synthetic-relay-key-14935"),
            "relay saw only the relay key: {request:?}"
        );
    }
    assert!(
        f.canonical.requests().is_empty(),
        "no read reached the canonical remote directly: {:?}",
        f.canonical.requests()
    );
    assert_eq!(f.relay.receive_pack_count(), 0);

    // The relay route never persisted into repository configuration.
    let config = std::fs::read_to_string(clone.join(".git/config")).expect("repo config");
    assert!(!config.contains("extraHeader"), "relay key persisted: {config}");
    assert!(!config.contains("insteadOf"), "relay rewrite persisted: {config}");

    // A second preparation fetches the newly added commit through the relay.
    let new_revision = f.advance_relay_backing("second");
    let second = f.prepare(Some(&relay)).expect("relay fetch prepares");
    assert_eq!(second.revision, new_revision);
    assert_ne!(second.revision, first.revision);
    assert!(
        f.relay.requests().iter().filter(|r| r.is_probe()).count() >= 2,
        "the fetch was routed through the relay again"
    );
    assert_eq!(
        git(&clone, &["remote", "get-url", "origin"]),
        f.canonical_url,
        "the canonical origin survives the fetch"
    );
    assert!(f.canonical.requests().is_empty());
}

#[test]
fn a_refused_repository_is_an_actionable_refusal_with_no_direct_read() {
    let temp = tempfile::tempdir().expect("tempdir");
    let refuse = FakeEndpoint::start(Surface::Refuse("repo_not_permitted"));
    let volume = temp.path().join("volume");
    std::fs::create_dir(&volume).expect("volume dir");
    let canonical_url = refuse.url("acme/widget");
    let relay_url = refuse.url("acme/widget.git");
    let vars: HashMap<&str, &str> = [
        (RELAY_URL_ENV, relay_url.as_str()),
        (RELAY_HEADER_ENV, SYNTHETIC_HEADER),
    ]
    .into_iter()
    .collect();
    let relay = RelayRead::from_env_for(&canonical_url, &|name| {
        vars.get(name).map(|v| std::ffi::OsString::from(v))
    })
    .expect("relay contract holds")
    .expect("relay route present");

    let args = repository::PrepareArgs {
        account: "fixture".into(),
        repository: canonical_url,
        base: "main".into(),
        container_id: "test-container".into(),
        branch: None,
        slug: None,
    };
    let failure = repository::prepare_with_relay(&volume, &args, &|_, _, _, _| false, Some(&relay))
        .expect_err("a refused repository must refuse preparation");
    let message = failure.to_string();
    assert!(
        message.contains("refused the Git read") && message.contains("repo_not_permitted"),
        "not an actionable relay diagnostic: {message}"
    );
    assert!(
        message.contains(refuse.addr.to_string().as_str()),
        "the diagnostic names the relay: {message}"
    );
    // No clone was left behind and nothing switched to a direct read.
    assert!(!volume.join("repo").exists());
    let seen = refuse.requests();
    assert!(
        seen.iter().all(|r| r.is_probe()),
        "only the probe ran, no Git negotiation: {seen:?}"
    );
}

#[test]
fn an_unreachable_relay_is_an_actionable_error() {
    // A port that answers nothing: bound, then released.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let addr = listener.local_addr().expect("addr");
    drop(listener);
    let temp = tempfile::tempdir().expect("tempdir");
    let volume = temp.path().join("volume");
    std::fs::create_dir(&volume).expect("volume dir");
    let canonical_url = format!("https://{addr}/acme/widget");
    let relay_url = format!("https://{addr}/acme/widget.git");
    let vars: HashMap<&str, &str> = [
        (RELAY_URL_ENV, relay_url.as_str()),
        (RELAY_HEADER_ENV, SYNTHETIC_HEADER),
    ]
    .into_iter()
    .collect();
    let relay = RelayRead::from_env_for(&canonical_url, &|name| {
        vars.get(name).map(|v| std::ffi::OsString::from(v))
    })
    .expect("relay contract holds")
    .expect("relay route present");

    let args = repository::PrepareArgs {
        account: "fixture".into(),
        repository: canonical_url,
        base: "main".into(),
        container_id: "test-container".into(),
        branch: None,
        slug: None,
    };
    let failure = repository::prepare_with_relay(&volume, &args, &|_, _, _, _| false, Some(&relay))
        .expect_err("an unreachable relay must refuse preparation");
    let message = failure.to_string();
    assert!(
        message.contains("unreachable"),
        "not an actionable transport diagnostic: {message}"
    );
    assert!(
        message.contains(addr.to_string().as_str()),
        "the diagnostic names the relay: {message}"
    );
}

#[test]
fn the_push_destination_stays_canonical_and_the_relay_never_receives_writes() {
    let f = RelayFixture::new();
    let relay = f.relay();
    f.prepare(Some(&relay)).expect("relay clone prepares");
    let clone = f.volume.join("repo");

    // Local unpublished work on a named branch, as a step would leave it.
    git(&clone, &["switch", "-c", "feature/issue-1"]);
    std::fs::write(clone.join("step"), "work").expect("write");
    git(&clone, &["add", "."]);
    git(&clone, &["commit", "-q", "-m", "step"]);

    // `recover --publish` pushes through the clone's origin: the canonical
    // remote, never the relay.
    let outcome = repository::recover(
        &f.volume,
        &repository::RecoverArgs {
            account: "fixture".into(),
            repository: f.canonical_url.clone(),
            base: "main".into(),
            publish: true,
            discard: false,
            issue: None,
            branch: vec!["feature/issue-1".into()],
            worktree: Vec::new(),
            validate_worktrees: false,
            slug: None,
        },
    )
    .expect("recover publishes");
    assert_eq!(outcome.published, ["feature/issue-1"]);

    assert!(
        f.canonical.receive_pack_count() > 0,
        "the push reached the canonical remote: {:?}",
        f.canonical.requests()
    );
    assert_eq!(f.relay.receive_pack_count(), 0, "the relay must never receive a write");
    assert_eq!(git(&clone, &["remote", "get-url", "origin"]), f.canonical_url);
}

#[test]
fn the_unset_route_keeps_today_s_direct_path() {
    // No relay in the exec: the legacy local-remote fixture prepares as
    // before (the existing suite covers this shape; this pins that a route
    // of `None` is the same code path, not a third one).
    let temp = tempfile::tempdir().expect("tempdir");
    let remote = temp.path().join("remote");
    let volume = temp.path().join("volume");
    std::fs::create_dir(&remote).expect("remote dir");
    std::fs::create_dir(&volume).expect("volume dir");
    git(&remote, &["init", "-q", "-b", "main"]);
    configure(&remote);
    std::fs::write(remote.join("fixture"), "base").expect("write");
    git(&remote, &["add", "."]);
    git(&remote, &["commit", "-q", "-m", "base"]);
    let args = repository::PrepareArgs {
        account: "fixture".into(),
        repository: remote.display().to_string(),
        base: "main".into(),
        container_id: "test-container".into(),
        branch: None,
        slug: None,
    };
    let outcome = repository::prepare_with_relay(&volume, &args, &|_, _, _, _| false, None)
        .expect("direct preparation still works");
    assert!(!outcome.revision.is_empty());
}

// ---------------------------------------------------------------------------
// The route contract.
// ---------------------------------------------------------------------------

fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
    move |name| {
        vars.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| std::ffi::OsString::from(v))
    }
}

/// The error text of a route lookup that must have refused.
fn err_of<T>(outcome: std::result::Result<T, String>, what: &str) -> String {
    match outcome {
        Err(error) => error,
        Ok(_) => panic!("expected a refusal: {what}"),
    }
}

#[test]
fn the_exec_route_contract_is_enforced() {
    let canonical = "https://github.com/acme/widget";
    let url = "https://github-proxy.example.com/acme/widget.git";

    let none = RelayRead::from_env_for(canonical, &env_of(&[])).expect("no vars");
    assert!(none.is_none(), "no relay variables means no route");

    for broken in [
        vec![(RELAY_URL_ENV, url)],
        vec![(RELAY_HEADER_ENV, SYNTHETIC_HEADER)],
    ] {
        let error = err_of(
            RelayRead::from_env_for(canonical, &env_of(&broken)),
            "one variable alone is refused",
        );
        assert!(error.contains("need both"), "{error}");
    }
    for (vars, needle) in [
        (
            vec![
                (RELAY_URL_ENV, "https://github-proxy.example.com/other/thing.git"),
                (RELAY_HEADER_ENV, SYNTHETIC_HEADER),
            ],
            "not the prepared repository",
        ),
        (
            vec![
                (RELAY_URL_ENV, "https://github-proxy.example.com/acme/widget"),
                (RELAY_HEADER_ENV, SYNTHETIC_HEADER),
            ],
            ".git",
        ),
        (
            vec![
                (RELAY_URL_ENV, "http://github-proxy.example.com/acme/widget.git"),
                (RELAY_HEADER_ENV, SYNTHETIC_HEADER),
            ],
            "HTTPS",
        ),
        (
            vec![
                (RELAY_URL_ENV, url),
                (RELAY_HEADER_ENV, "Basic dXNlcjprZXk="),
            ],
            "Bearer",
        ),
        (
            vec![
                (RELAY_URL_ENV, url),
                (RELAY_HEADER_ENV, "Authorization: Bearer "),
            ],
            "non-empty",
        ),
    ] {
        let error = err_of(
            RelayRead::from_env_for(canonical, &env_of(&vars)),
            "a broken route contract is refused",
        );
        assert!(error.contains(needle), "expected {needle:?} in: {error}");
        assert!(!error.contains(SYNTHETIC_KEY), "the diagnostic must not echo the key: {error}");
    }

    let route = RelayRead::from_env_for(
        canonical,
        &env_of(&[(RELAY_URL_ENV, url), (RELAY_HEADER_ENV, SYNTHETIC_HEADER)]),
    )
    .expect("valid contract")
    .expect("route present");
    let env = route.git_env(canonical);
    let value_of = |config_key: &str| -> String {
        for (name, value) in &env {
            if value == config_key {
                let Some(index) = name.strip_prefix("GIT_CONFIG_KEY_") else {
                    continue;
                };
                if let Some((_, val)) = env
                    .iter()
                    .find(|(k, _)| k == &format!("GIT_CONFIG_VALUE_{index}"))
                {
                    return val.clone();
                }
            }
        }
        String::new()
    };
    let count = env
        .iter()
        .find(|(k, _)| k == "GIT_CONFIG_COUNT")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    assert_eq!(count, "3");
    assert_eq!(
        value_of(&format!("url.{url}.insteadOf")),
        canonical,
        "the rewrite is whole-URL: nothing else is rewritten"
    );
    assert_eq!(value_of("http.extraHeader"), SYNTHETIC_HEADER);
    assert_eq!(value_of("http.followRedirects"), "false");
}

#[test]
fn the_host_route_resolves_from_the_git_key_refs_and_names_a_missing_key() {
    let key_dir = tempfile::tempdir().expect("tempdir");
    let key_file = key_dir.path().join("acme-git.key");
    std::fs::write(&key_file, "gp_14935testkey0000000000000000000").expect("key");
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&key_file, std::fs::Permissions::from_mode(0o600))
            .expect("owner-only key file");
    }
    let config = serde_json::json!({
        "forge": { "gateway": {
            "origin": "https://github-proxy.example.com",
            "keyRefs": {"acme": "file:/nowhere/at/all"},
            "git": {"keyRefs": {"Acme": format!("file:{}", key_file.display())}}
        } }
    });

    let route = RelayRoute::from_config(&config, "https://github.com/ACME/Widget.git")
        .expect("configured")
        .expect("the owner has a git key ref");
    let env = route.env();
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == RELAY_URL_ENV)
            .map(|(_, v)| v.as_str()),
        Some("https://github-proxy.example.com/acme/widget.git"),
        "the owner is lowercased into the served path shape"
    );
    assert_eq!(route.host(), "github-proxy.example.com");
    assert_eq!(
        env.iter()
            .find(|(k, _)| k == RELAY_HEADER_ENV)
            .map(|(_, v)| v.as_str()),
        Some("Authorization: Bearer gp_14935testkey0000000000000000000")
    );

    // An owner without a git entry, a non-github.com repository and no
    // gateway at all keep today's direct path.
    assert!(
        RelayRoute::from_config(&config, "https://github.com/other/tool.git")
            .expect("resolves")
            .is_none(),
        "no git key ref for the owner"
    );
    assert!(
        RelayRoute::from_config(&config, "https://forge.example.com/acme/widget.git")
            .expect("resolves")
            .is_none(),
        "the relay serves github.com only"
    );
    assert!(
        RelayRoute::from_config(&serde_json::json!({}), "https://github.com/acme/widget.git")
            .expect("resolves")
            .is_none(),
        "no gateway configured"
    );

    // A configured git key that cannot be read refuses preparation with the
    // ref and the reason — never a silent switch to the direct fetch.
    let broken = serde_json::json!({
        "forge": { "gateway": {
            "origin": "https://github-proxy.example.com",
            "keyRefs": {"acme": "file:/nowhere/at/all"},
            "git": {"keyRefs": {"acme": "file:/also/nowhere"}}
        } }
    });
    let message = match RelayRoute::from_config(&broken, "https://github.com/acme/widget.git") {
        Err(error) => error.to_string(),
        Ok(_) => panic!("an unreadable key must refuse preparation"),
    };
    assert!(message.contains("forge.gateway.git.keyRefs.acme"), "names the ref: {message}");
    assert!(
        message.contains("relay Git key for acme cannot be read"),
        "actionable: {message}"
    );
}
