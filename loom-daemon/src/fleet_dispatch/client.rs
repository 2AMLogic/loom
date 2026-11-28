//! The worker's HTTP client for the fleet-dispatch server (#12450): one
//! bounded `POST` per call, and the `loom.fleet_dispatch.call` span.
//!
//! Every request has a short deadline ([`CALL_TIMEOUT`]), follows no
//! redirect and uses no proxy (the server is on the tailnet). The calls run
//! on the daemon's shared HTTP bridge ([`crate::http_bridge`]), the one the
//! forge mirror client uses for its synchronous callers too: it is only built
//! and spawned onto, never entered with `block_on`, so a call is safe from
//! any thread, inside a tokio runtime or not.
//!
//! A claim waits for its answer ([`post_blocking`]). `refuse`, `release` and
//! the heartbeat do not ([`post_detached`]): the step path never waits on
//! them.

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::http_bridge::{self, BridgeError, TransportClass};
use crate::step_role::StepRole;
use crate::telemetry::trace::{SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext};

/// One call's deadline: short, so a dead server costs a spawn little.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(2);

/// The deadline of a call that waits out a captain-side probe run (the
/// on-demand token refresh, #13943): the probe's own bound (the runner's
/// `tokens check` timeout, 120 s) plus answering headroom, so the answer
/// honestly reports the probe instead of racing it.
pub const PROBE_WAIT_TIMEOUT: Duration = Duration::from_secs(180);

/// Why a call produced no answer. Every one of these fails open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallError {
    /// No connection, or no answer within the deadline.
    Transport(String),
    /// A non-2xx status.
    Status(u16),
    /// A 2xx whose body is not the contract's shape.
    Malformed(String),
}

impl CallError {
    /// The HTTP status, when the server answered one.
    #[must_use]
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Status(s) => Some(*s),
            Self::Malformed(_) => Some(200),
            Self::Transport(_) => None,
        }
    }

    /// The closed `loom.fleet_dispatch.reason` token of a failed call.
    #[must_use]
    pub fn token(&self) -> &'static str {
        match self {
            Self::Transport(_) => "transport",
            Self::Status(s) if *s >= 500 => "status_5xx",
            Self::Status(_) => "status_4xx",
            Self::Malformed(_) => "malformed",
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(e) => write!(f, "transport: {e}"),
            Self::Status(s) => write!(f, "HTTP {s}"),
            Self::Malformed(e) => write!(f, "malformed answer: {e}"),
        }
    }
}

/// The client every call shares: no redirect, no proxy. It carries no timeout
/// of its own — [`post`] bounds each request — so one client serves both
/// [`CALL_TIMEOUT`] and [`PROBE_WAIT_TIMEOUT`].
fn build_http() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .connect_timeout(CALL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|e| format!("could not construct the fleet-dispatch HTTP client: {e}"))
}

fn http() -> Option<&'static reqwest::Client> {
    static HTTP: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    HTTP.get_or_init(|| {
        build_http()
            .map_err(|e| log::warn!("fleet_dispatch: {e}"))
            .ok()
    })
    .as_ref()
}

/// The error of a call the bridge could not take.
fn no_runtime() -> CallError {
    CallError::Transport(BridgeError::NoRuntime.to_string())
}

/// `POST <base><path>` with `body`, bounded by `timeout`, and parse the JSON
/// answer.
async fn post<Req: Serialize + ?Sized, Resp: DeserializeOwned>(
    http: &reqwest::Client,
    base: &str,
    path: &str,
    body: &Req,
    timeout: Duration,
) -> Result<Resp, CallError> {
    let url = format!("{}{path}", base.trim_end_matches('/'));
    let res = http
        .post(url)
        .timeout(timeout)
        .header(reqwest::header::ACCEPT, "application/json")
        .json(body)
        .send()
        .await
        .map_err(|e| CallError::Transport(classify(&e)))?;
    let status = res.status();
    if !status.is_success() {
        return Err(CallError::Status(status.as_u16()));
    }
    let bytes = res
        .bytes()
        .await
        .map_err(|e| CallError::Transport(classify(&e)))?;
    serde_json::from_slice(&bytes).map_err(|e| CallError::Malformed(e.to_string()))
}

/// A transport error's class, never its URL. This client reads the answer as
/// raw bytes, and reqwest reports a failed read of them as a decode error:
/// for this client both are the answer's body.
fn classify(e: &reqwest::Error) -> String {
    match http_bridge::classify_reqwest_error(e) {
        TransportClass::Decode | TransportClass::Body => "body".to_owned(),
        class => class.to_string(),
    }
}

/// [`post`] from synchronous code, bounded by `timeout` (plus a little)
/// whatever the network does.
fn post_blocking_bounded_by<Req, Resp>(
    base: &str,
    path: &str,
    body: &Req,
    timeout: Duration,
) -> Result<Resp, CallError>
where
    Req: Serialize + Clone + Send + Sync + 'static,
    Resp: DeserializeOwned + Send + 'static,
{
    let (http, base, path, body) = match http() {
        Some(http) => (http.clone(), base.to_string(), path.to_string(), body.clone()),
        None => return Err(no_runtime()),
    };
    http_bridge::run_blocking(timeout + Duration::from_millis(500), async move {
        post::<Req, Resp>(&http, &base, &path, &body, timeout).await
    })
    .unwrap_or_else(|e| Err(CallError::Transport(e.to_string())))
}

/// [`post`] from synchronous code, bounded by [`CALL_TIMEOUT`] (plus a
/// little) whatever the network does.
///
/// # Errors
///
/// [`CallError`]; a missed deadline is [`CallError::Transport`].
pub fn post_blocking<Req, Resp>(base: &str, path: &str, body: &Req) -> Result<Resp, CallError>
where
    Req: Serialize + Clone + Send + Sync + 'static,
    Resp: DeserializeOwned + Send + 'static,
{
    post_blocking_bounded_by(base, path, body, CALL_TIMEOUT)
}

/// [`post`] without waiting: `done` gets the answer and the call's latency
/// on the bridge runtime.
pub fn post_detached<Req, Resp>(
    base: &str,
    path: &str,
    body: Req,
    done: impl FnOnce(Result<Resp, CallError>, Duration) + Send + 'static,
) where
    Req: Serialize + Send + Sync + 'static,
    Resp: DeserializeOwned + Send + 'static,
{
    let (Some(rt), Some(http)) = (http_bridge::runtime(), http()) else {
        done(Err(no_runtime()), Duration::ZERO);
        return;
    };
    let (http, base, path) = (http.clone(), base.to_string(), path.to_string());
    rt.spawn(async move {
        let started = Instant::now();
        let answer = post::<Req, Resp>(&http, &base, &path, &body, CALL_TIMEOUT).await;
        done(answer, started.elapsed());
    });
}

/// [`post_blocking`] for a call whose answer legitimately arrives minutes
/// later — a captain running its token-pool probe before answering (#13943).
/// Bounded by [`PROBE_WAIT_TIMEOUT`] (plus a little) whatever the network
/// does; the same safe-from-any-thread contract as [`post_blocking`].
///
/// # Errors
///
/// [`CallError`]; a missed deadline is [`CallError::Transport`].
pub fn post_blocking_probe_wait<Req, Resp>(
    base: &str,
    path: &str,
    body: &Req,
) -> Result<Resp, CallError>
where
    Req: Serialize + Clone + Send + Sync + 'static,
    Resp: DeserializeOwned + Send + 'static,
{
    post_blocking_bounded_by(base, path, body, PROBE_WAIT_TIMEOUT)
}

// ---------------------------------------------------------------------------
// The call span
// ---------------------------------------------------------------------------

/// `loom.fleet_dispatch.result` of a granted claim.
pub const RESULT_GRANTED: &str = "granted";
/// `loom.fleet_dispatch.result` of a refused claim.
pub const RESULT_REFUSED: &str = "refused";
/// `loom.fleet_dispatch.result` of an answered heartbeat, refuse or release.
pub const RESULT_OK: &str = "ok";
/// `loom.fleet_dispatch.result` of a call with no usable answer: `error`,
/// the captain side's value (#12465), so one query reads both sides.
pub const RESULT_ERROR: &str = "error";

/// One call's facts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CallFacts {
    /// The path called (`/v1/claim`, ...), as the server names it.
    pub endpoint: &'static str,
    pub latency: Duration,
    /// [`RESULT_GRANTED`], [`RESULT_REFUSED`], [`RESULT_OK`] or
    /// [`RESULT_ERROR`].
    pub result: &'static str,
    /// The refusal reason, the refuse reason, the release outcome, or a
    /// failed call's [`CallError::token`].
    pub reason: Option<String>,
    pub server_epoch: Option<String>,
    /// The HTTP status, when the server answered.
    pub status: Option<u16>,
    pub repo: Option<String>,
    pub item: Option<u64>,
    /// The step's wire name (`judge`, `champion_merge`, ...).
    pub step: Option<String>,
    pub assignment_id: Option<String>,
    /// The host holding the item, on a refused claim that names one (#12707).
    pub held_by: Option<String>,
    /// Seconds before asking again is worthwhile, on a refused claim.
    pub retry_after_s: Option<u64>,
    /// Whether the refusal is a no-progress hold (`cooldown`): then the
    /// span carries `hold_until`.
    pub hold: bool,
}

/// The span for `facts`, ended at `at`. Pure.
///
/// The item and role use the codebase's join keys, as the captain's span of
/// the same name does (#12465): `loom.issue` or `loom.pr_number` (by whether
/// the step acts on a PR) and `loom.role` (the role name, as on
/// `loom.dispatch.step`), never `loom.step.*`, so one query covers both
/// sides of a call.
#[must_use]
pub fn span(facts: &CallFacts, at: DateTime<Utc>) -> SpanRecord {
    let name = SpanName::FleetDispatchCall;
    let instant = crate::telemetry::trace::instant(at);
    let item = facts.item.map(|n| n.to_string()).unwrap_or_default();
    let context = TraceContext::derived(
        name.as_str(),
        &[
            facts.endpoint,
            facts.assignment_id.as_deref().unwrap_or(""),
            facts.repo.as_deref().unwrap_or(""),
            &item,
            &instant,
        ],
    );
    let mut attributes = TraceAttributes::new();
    let mut put = |k: &str, v: String| {
        attributes.insert(k.to_string(), v);
    };
    put("loom.fleet_dispatch.side", "worker".to_string());
    put("loom.fleet_dispatch.host", crate::host_identity::host_identity());
    put("loom.fleet_dispatch.endpoint", facts.endpoint.to_string());
    put("loom.fleet_dispatch.latency_ms", facts.latency.as_millis().to_string());
    put("loom.fleet_dispatch.result", facts.result.to_string());
    put(
        "loom.fleet_dispatch.status",
        facts
            .status
            .map_or_else(|| "0".to_string(), |s| s.to_string()),
    );
    if let Some(reason) = &facts.reason {
        put("loom.fleet_dispatch.reason", reason.clone());
    }
    if let Some(epoch) = &facts.server_epoch {
        put("loom.fleet_dispatch.server_epoch", epoch.clone());
    }
    if let Some(repo) = &facts.repo {
        put("loom.repo", repo.clone());
    }
    let role = facts
        .step
        .as_deref()
        .and_then(|s| s.parse::<StepRole>().ok());
    if let Some(n) = facts.item {
        let key = if role.is_some_and(StepRole::acts_on_pr) {
            "loom.pr_number"
        } else {
            "loom.issue"
        };
        put(key, n.to_string());
    }
    if let Some(step) = &facts.step {
        put("loom.role", role.map_or_else(|| step.clone(), |r| r.role_name().to_string()));
    }
    if let Some(id) = &facts.assignment_id {
        put("loom.assignment.id", id.clone());
    }
    if let Some(step) = &facts.step {
        put("loom.fleet_dispatch.step", step.clone());
    }
    if let Some(held_by) = &facts.held_by {
        put("loom.fleet_dispatch.held_by", held_by.clone());
    }
    if let Some(retry) = facts.retry_after_s {
        put("loom.fleet_dispatch.retry_after_s", retry.to_string());
        if facts.hold {
            put("loom.fleet_dispatch.hold_until", super::telemetry::hold_until(at, retry));
        }
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    let started_at =
        at - chrono::Duration::from_std(facts.latency).unwrap_or_else(|_| chrono::Duration::zero());
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at,
        ended_at: at,
        status: if facts.result == RESULT_ERROR {
            SpanStatus::Error
        } else {
            SpanStatus::Ok
        },
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// Emit `facts`' span; a no-op without an exporter.
pub fn record(facts: &CallFacts) {
    if !crate::observability::ops::spans_exported() {
        return;
    }
    crate::observability::ops::emit_span(span(facts, Utc::now()));
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::super::proto::{ClaimRequest, ClaimResponse, CLAIM_PATH};
    use super::super::test_server::TestServer;
    use super::*;
    use serde_json::json;

    #[test]
    fn a_claim_round_trips_through_a_real_server() {
        let server = TestServer::start(|path, body| {
            assert_eq!(path, CLAIM_PATH);
            assert_eq!(body["step"], "judge");
            (
                200,
                json!({"server_epoch":"e1","granted":true,"assignment_id":"a-9","ttl_s":1080}),
            )
        });
        let req = ClaimRequest {
            host_id: "h".into(),
            repo: "o/r".into(),
            item: 9,
            step: "judge".into(),
        };
        let answer: ClaimResponse = post_blocking(&server.url(), CLAIM_PATH, &req).unwrap();
        assert_eq!(answer.assignment_id.as_deref(), Some("a-9"));
        assert_eq!(server.requests().len(), 1);
    }

    #[test]
    fn a_5xx_a_bad_body_and_a_dead_port_are_errors() {
        let req = json!({});
        let five = TestServer::start(|_, _| (503, json!({})));
        let e = post_blocking::<_, ClaimResponse>(&five.url(), CLAIM_PATH, &req).unwrap_err();
        assert_eq!(e, CallError::Status(503));
        assert_eq!(e.token(), "status_5xx");
        let bad = TestServer::start(|_, _| (200, json!({"nope": 1})));
        let e = post_blocking::<_, ClaimResponse>(&bad.url(), CLAIM_PATH, &req).unwrap_err();
        assert_eq!(e.token(), "malformed");
        let dead = TestServer::dead_url();
        let e = post_blocking::<_, ClaimResponse>(&dead, CLAIM_PATH, &req).unwrap_err();
        assert_eq!(e.token(), "transport");
    }

    #[test]
    fn a_server_that_never_answers_is_bounded_by_the_deadline() {
        let silent = TestServer::silent();
        let started = Instant::now();
        let e =
            post_blocking::<_, ClaimResponse>(&silent.url(), CLAIM_PATH, &json!({})).unwrap_err();
        assert_eq!(e.token(), "transport");
        assert!(
            started.elapsed() < CALL_TIMEOUT + Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
    }

    /// #13943: a token refresh waits out a captain-side probe run, so its
    /// answer legitimately arrives far past [`CALL_TIMEOUT`].
    #[test]
    fn a_probe_wait_call_outlasts_the_normal_deadline() {
        use super::super::proto::{TokensRefreshResponse, TOKENS_REFRESH_PATH};
        let slow = TestServer::start(|_, _| {
            std::thread::sleep(CALL_TIMEOUT + Duration::from_millis(100));
            (
                200,
                json!({"server_epoch":"e1","probed_at":"2026-11-19T23:30:00Z","coalesced":false}),
            )
        });
        let answer: TokensRefreshResponse =
            post_blocking_probe_wait(&slow.url(), TOKENS_REFRESH_PATH, &json!({"host_id":"h"}))
                .unwrap();
        assert!(answer.probed_at.is_some(), "{answer:?}");
        assert!(!answer.coalesced);
    }

    /// A detached call has no bridge bound of its own to lean on: the
    /// request's own [`CALL_TIMEOUT`] is what brings `done` its answer.
    #[test]
    fn a_detached_call_is_bounded_by_the_request_timeout() {
        let silent = TestServer::silent();
        let (tx, rx) = std::sync::mpsc::channel();
        post_detached::<serde_json::Value, ClaimResponse>(
            &silent.url(),
            CLAIM_PATH,
            json!({}),
            move |answer, _latency| tx.send(answer.map(|_| ())).unwrap(),
        );
        let e = rx
            .recv_timeout(CALL_TIMEOUT + Duration::from_secs(10))
            .expect("the detached call never answered")
            .unwrap_err();
        assert_eq!(e, CallError::Transport("timeout".into()));
    }

    /// Answer one request with `raw` bytes, then close the connection.
    fn answer_once(raw: &'static str) -> String {
        use std::io::{Read, Write};
        use std::net::Shutdown;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut b = [0u8; 1];
            while !head.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut b).unwrap();
                head.push(b[0]);
            }
            let len: usize = String::from_utf8_lossy(&head)
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .map_or(0, |v| v.parse().unwrap());
            let mut body = vec![0u8; len];
            stream.read_exact(&mut body).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            stream.shutdown(Shutdown::Write).unwrap();
        });
        format!("http://{addr}")
    }

    /// A `2xx` whose body stops short of its `Content-Length` is a failure
    /// of the answer's body (never a malformed answer, never a `decode`).
    #[test]
    fn a_truncated_body_is_a_body_transport_error() {
        let url = answer_once(
            "HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"a\":1}",
        );
        let e = post_blocking::<_, ClaimResponse>(&url, CLAIM_PATH, &json!({})).unwrap_err();
        assert_eq!(e, CallError::Transport("body".into()));
    }

    #[test]
    fn the_call_span_carries_its_facts_and_passes_the_allowlist() {
        let facts = CallFacts {
            endpoint: CLAIM_PATH,
            latency: Duration::from_millis(12),
            result: RESULT_REFUSED,
            reason: Some("held".into()),
            server_epoch: Some("e1".into()),
            status: Some(200),
            repo: Some("o/r".into()),
            item: Some(9),
            step: Some("judge".into()),
            assignment_id: None,
            held_by: Some("host-b".into()),
            retry_after_s: Some(90),
            hold: true,
        };
        let span = span(&facts, Utc::now());
        assert_eq!(span.name.as_str(), "loom.fleet_dispatch.call");
        let attr = |k: &str| span.attributes.get(k).cloned();
        assert_eq!(attr("loom.fleet_dispatch.endpoint").as_deref(), Some("/v1/claim"));
        assert_eq!(attr("loom.fleet_dispatch.latency_ms").as_deref(), Some("12"));
        assert_eq!(attr("loom.fleet_dispatch.result").as_deref(), Some("refused"));
        assert_eq!(attr("loom.fleet_dispatch.reason").as_deref(), Some("held"));
        assert_eq!(attr("loom.fleet_dispatch.server_epoch").as_deref(), Some("e1"));
        assert_eq!(attr("loom.fleet_dispatch.side").as_deref(), Some("worker"));
        assert_eq!(attr("loom.fleet_dispatch.status").as_deref(), Some("200"));
        assert_eq!(attr("loom.pr_number").as_deref(), Some("9"), "a Judge's item is a PR");
        assert_eq!(attr("loom.issue"), None);
        assert_eq!(attr("loom.role").as_deref(), Some("judge"));
        assert_eq!(attr("loom.fleet_dispatch.step").as_deref(), Some("judge"));
        assert_eq!(attr("loom.fleet_dispatch.held_by").as_deref(), Some("host-b"));
        assert_eq!(attr("loom.fleet_dispatch.retry_after_s").as_deref(), Some("90"));
        let until = attr("loom.fleet_dispatch.hold_until").unwrap();
        assert!(chrono::DateTime::parse_from_rfc3339(&until).is_ok(), "{until}");
        assert!(
            !span.attributes.keys().any(|k| k.starts_with("loom.step.")),
            "the captain's join keys, not loom.step.*: {:?}",
            span.attributes.keys().collect::<Vec<_>>()
        );
        // Every key survives the export filter (the lifecycle allowlist plus
        // `OPS_SPAN_ATTRIBUTE_KEYS`).
        let exported = span.clone().bounded().attributes;
        for key in span.attributes.keys() {
            assert!(exported.contains_key(key), "{key} is dropped at export");
        }
        let failed = super::span(
            &CallFacts {
                result: RESULT_ERROR,
                assignment_id: Some("a-1".into()),
                step: Some("champion_merge".into()),
                ..facts
            },
            Utc::now(),
        );
        assert_eq!(failed.status, SpanStatus::Error);
        assert_eq!(failed.attributes["loom.fleet_dispatch.result"], "error");
        assert_eq!(failed.attributes["loom.assignment.id"], "a-1");
        assert_eq!(failed.attributes["loom.role"], "champion");
        assert_eq!(failed.attributes["loom.pr_number"], "9");
        let builder = super::span(
            &CallFacts {
                step: Some("builder".into()),
                item: Some(5),
                ..CallFacts::default()
            },
            Utc::now(),
        );
        assert_eq!(builder.attributes["loom.issue"], "5", "a Builder's item is an issue");
        assert!(!builder.attributes.contains_key("loom.pr_number"));
        assert_eq!(builder.attributes["loom.role"], "builder");
    }
}
