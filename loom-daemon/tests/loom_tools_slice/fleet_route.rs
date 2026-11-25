//! #14465: fleet-mode routing of the two curation workflow writes through
//! the scoped broker, end to end. A real local HTTP server plays the broker
//! (the same `/v1/<op>` + bearer-key contract 2am#8397 ships), the fake
//! published gateway plays the fleet reader, and the assertions a fleet run
//! must uphold are: the write went to the broker, no mutation ever reached
//! a `gh` credential, and a broker failure fails closed (curation) or is
//! reported per dependent (notification) — never a local fallback.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use super::publish_fake_gateway;
use super::workflow::{Forge, CURATE, NOTIFY, PARK};

/// One broker request the fake observed.
#[derive(Clone, Debug)]
struct Seen {
    auth: String,
    path: String,
    body: serde_json::Value,
}

type Answer = Arc<dyn Fn(&str, &serde_json::Value) -> (u16, String) + Send + Sync>;

/// The scripted broker: one thread, one request per connection, `Connection:
/// close` on every answer (reqwest re-opens as needed).
struct BrokerFake {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl BrokerFake {
    fn start(answer: Answer) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind broker");
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let logs = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { break };
                let mut reader = BufReader::new(stream);
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                let path = request_line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_owned();
                let mut content_length = 0usize;
                let mut auth = String::new();
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                    if let Some((name, value)) = header.split_once(':') {
                        if name.trim().eq_ignore_ascii_case("content-length") {
                            content_length = value.trim().parse().unwrap_or(0);
                        } else if name.trim().eq_ignore_ascii_case("authorization") {
                            auth = value.trim().to_owned();
                        }
                    }
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 {
                    reader.read_exact(&mut body).ok();
                }
                let json: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
                let (status, text) = answer(&path, &json);
                logs.lock().unwrap().push(Seen {
                    auth,
                    path,
                    body: json,
                });
                let reason = match status {
                    200 => "OK",
                    403 => "Forbidden",
                    _ => "Error",
                };
                let reply = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{text}",
                    text.len()
                );
                let mut stream = reader.into_inner();
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        Self { url, seen }
    }

    fn calls(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// The fleet environment the tools binary needs: mode, a published fake
/// gateway routing reads to the fixture `gh`, and the broker endpoint.
fn fleet_env<'a>(
    gateway: &std::path::Path,
    broker: &BrokerFake,
    extra: &'a [(&'a str, &'a str)],
) -> Vec<(&'a str, String)> {
    let mut env: Vec<(&str, String)> = [
        ("LOOM_MODE", "fleet".to_owned()),
        ("LOOM_GH_GATEWAY_DIR", gateway.display().to_string()),
        ("GITHUB_WRITE_BROKER_URL", broker.url.clone()),
        ("FLEET_ROLES_GITHUB_WRITE_KEY", "test-key-1".to_owned()),
        ("FLEET_ROLES_GITHUB_WRITE_KEY_FILE", "/absent/keyfile".to_owned()),
    ]
    .into_iter()
    .collect();
    env.extend(extra.iter().map(|(k, v)| (*k, (*v).to_owned())));
    env
}

fn call_fleet(
    f: &Forge,
    gateway: &std::path::Path,
    broker: &BrokerFake,
    args: &[&str],
) -> std::process::Output {
    let env = fleet_env(gateway, broker, &[]);
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    f.call(super::TOOLS, args, &pairs)
}

/// Publish the gateway naming the fixture `gh`, so the routed reads answer
/// from the same stateful fake the local-mode tests use.
fn publish(f: &Forge) -> std::path::PathBuf {
    let gateway = f.root().join("gateway/.loom/gh-config-gateway");
    publish_fake_gateway(&gateway, "acme", &f.root().join("gh"));
    gateway
}

fn no_local_writes(f: &Forge) {
    let calls = f.get("calls");
    for write in ["-X", "edit"] {
        assert!(!calls.contains(write), "a gh credential received a write: {calls}");
    }
}

fn ok_complete_curation() -> (u16, String) {
    (
        200,
        r#"{"number":12,"points":"5","previous_points":["points:2"],"curated":true,
            "changed":true,"replayed":false}"#
            .to_owned(),
    )
}

fn ok_notify(unblocked: bool, replayed: bool) -> (u16, String) {
    (
        200,
        format!(
            r#"{{"number":12,"closed":10,"unblocked":{unblocked},
                "comment":{{"id":700,"url":"https://example/c700"}},"replayed":{replayed}}}"#
        ),
    )
}

#[test]
fn fleet_curation_routes_the_label_projection_through_the_broker() {
    let f = Forge::new();
    let gateway = publish(&f);
    f.set("labels", r#"["points:2","important"]"#);
    f.set("body", "<!-- loom:points=5 -->");
    let broker = BrokerFake::start(Arc::new(|path, _| {
        if path == "/v1/complete_curation" {
            ok_complete_curation()
        } else {
            (404, r#"{"error":"unknown_op"}"#.to_owned())
        }
    }));
    let out = call_fleet(&f, &gateway, &broker, CURATE);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{out:?}\n{stderr}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains(r#""recorded":true"#) && stdout.contains(r#""points":5"#),
        "{stdout}"
    );

    let calls = broker.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].path, "/v1/complete_curation");
    assert_eq!(calls[0].auth, "Bearer test-key-1");
    assert_eq!(calls[0].body["repo"], "acme/role-probe");
    assert_eq!(calls[0].body["number"], 12);
    assert_eq!(calls[0].body["points"], "5");
    assert!(calls[0].body["idempotency_key"]
        .as_str()
        .is_some_and(|k| !k.is_empty()));

    // The label state is the fixture's, untouched: only the broker wrote.
    assert_eq!(f.get("labels"), r#"["points:2","important"]"#);
    no_local_writes(&f);
}

#[test]
fn fleet_curation_broker_failure_fails_closed_with_no_local_fallback() {
    let f = Forge::new();
    let gateway = publish(&f);
    f.set("labels", r#"["points:2","important"]"#);
    f.set("body", "<!-- loom:points=5 -->");
    let broker = BrokerFake::start(Arc::new(|_, _| {
        (502, r#"{"error":"github_error","status":502}"#.to_owned())
    }));
    let out = call_fleet(&f, &gateway, &broker, CURATE);
    assert!(!out.status.success(), "{out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("github_error") && stderr.contains("fleet broker"), "{stderr}");
    assert_eq!(f.get("labels"), r#"["points:2","important"]"#);
    no_local_writes(&f);
}

#[test]
fn fleet_notification_posts_one_marker_comment_per_closed_blocker() {
    let f = Forge::new();
    let gateway = publish(&f);
    f.set("body", PARK);
    f.set("labels", r#"["loom:blocked"]"#);
    let broker = BrokerFake::start(Arc::new(|path, body| {
        if path == "/v1/notify_cleared_blocker" {
            assert_eq!(body["closed"], 10, "one call per (dependent, closed)");
            ok_notify(true, false)
        } else {
            (404, r#"{"error":"unknown_op"}"#.to_owned())
        }
    }));
    let out = call_fleet(&f, &gateway, &broker, NOTIFY);
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("notified issue #12") && stdout.contains("unparked to loom:curated"),
        "{stdout}"
    );

    let calls = broker.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].path, "/v1/notify_cleared_blocker");
    assert_eq!(calls[0].body["repo"], "acme/role-probe");
    assert_eq!(calls[0].body["number"], 12);
    assert_eq!(calls[0].body["closed"], 10);

    // Neither the comment nor the unpark touched a gh credential.
    assert!(f.get("comments").is_empty());
    assert_eq!(f.get("labels"), r#"["loom:blocked"]"#);
    no_local_writes(&f);

    // A retried close event replays the same idempotency key and stays
    // exactly one comment.
    let out = call_fleet(&f, &gateway, &broker, NOTIFY);
    assert!(out.status.success(), "{out:?}");
    let calls = broker.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].body["idempotency_key"], calls[1].body["idempotency_key"]);
    assert!(f.get("comments").is_empty());
}

#[test]
fn fleet_notification_refusal_is_best_effort_and_names_the_reason() {
    let f = Forge::new();
    let gateway = publish(&f);
    f.set("body", PARK);
    f.set("labels", r#"["loom:blocked"]"#);
    let broker = BrokerFake::start(Arc::new(|_, _| {
        (
            403,
            r#"{"error":"blocker_not_cited","message":"does not reference #10"}"#.to_owned(),
        )
    }));
    let out = call_fleet(&f, &gateway, &broker, NOTIFY);
    assert!(out.status.success(), "best-effort: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("FAILED to notify"), "{stderr}");
    assert!(stderr.contains("blocker_not_cited"), "{stderr}");
    assert!(f.get("comments").is_empty());
    assert_eq!(f.get("labels"), r#"["loom:blocked"]"#);
    no_local_writes(&f);
}

#[test]
fn fleet_resweep_failure_reports_the_failed_unpark_with_the_reason() {
    let f = Forge::new();
    let gateway = publish(&f);
    f.set("body", PARK);
    f.set("labels", r#"["loom:blocked"]"#);
    let broker = BrokerFake::start(Arc::new(|_, _| {
        (500, r#"{"error":"github_error","status":500}"#.to_owned())
    }));
    let out = call_fleet(&f, &gateway, &broker, &["notify-cleared-blockers", "--resweep"]);
    assert!(out.status.success(), "best-effort: {out:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unpark failed; no comment posted"), "{stderr}");
    assert!(stderr.contains("github_error"), "{stderr}");
    assert!(f.get("comments").is_empty());
    assert_eq!(f.get("labels"), r#"["loom:blocked"]"#);
    no_local_writes(&f);
}

#[test]
fn fleet_write_without_a_caller_key_refuses_before_any_forge_call() {
    let f = Forge::new();
    let gateway = publish(&f);
    f.set("body", &format!("{PARK}\n<!-- loom:points=5 -->"));
    f.set("labels", r#"["loom:blocked"]"#);
    let broker = BrokerFake::start(Arc::new(|_, _| ok_complete_curation()));
    let env = fleet_env(&gateway, &broker, &[("FLEET_ROLES_GITHUB_WRITE_KEY", "")]);
    // An empty key env falls through to the key file, which is absent too.
    let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    for argv in [CURATE, NOTIFY] {
        let _ = std::fs::remove_file(f.root().join("calls"));
        let out = f.call(super::TOOLS, argv, &pairs);
        assert!(!out.status.success(), "{out:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("FLEET_ROLES_GITHUB_WRITE_KEY") && stderr.contains("fleetroleskey"),
            "{stderr}"
        );
        assert!(!f.root().join("calls").exists(), "no forge call may precede the key refusal");
        assert!(broker.calls().is_empty());
    }
}
