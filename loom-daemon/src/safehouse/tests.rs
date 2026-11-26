//! Tests for the merged-PR report (#13946): config, the connection-state cell,
//! the wire builder, the client, the delivery rule (post / lost reply /
//! read-back / retry) and the host and captain producers.
//!
//! Everything runs offline: safehoused is a fake `AF_UNIX` room that can lose,
//! refuse or hang a reply, and `gh` is a fake script behind
//! `$LOOM_SAFEHOUSE_GH_BIN`.

use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::json;
use serial_test::serial;
use tokio::net::UnixListener;

use super::forge_gate;
use super::merge_report::{self, Outcome, PrReport, ReportTarget};
use super::merge_source::MergedPr;
use super::reporter::Reporter;
use super::*;
use crate::observability::ops::feed::Trigger;
use crate::rate_limit_breaker::evidence::ResetEvidence;
use crate::rate_limit_breaker::report::{BreakerHandle, BudgetProbe, FailureContext, ProbeMode};
use crate::rate_limit_breaker::{BudgetSnapshot, RateLimitBreakerConfig, SharedRateLimitBreaker};

// ---- fake safehoused room ---------------------------------------------------

/// The fake room's state and fault switches. Counters are consumed one fault at
/// a time, so a test can lose exactly one reply and then let traffic through.
#[derive(Default)]
struct Room {
    /// Bodies of messages that landed, oldest first.
    messages: Mutex<Vec<(i64, String)>>,
    /// Number of `send` requests received (landed or not).
    sends: AtomicUsize,
    /// Next N sends: the connection closes before the message lands.
    close_before_store: AtomicUsize,
    /// Next N sends: the message lands, then the connection closes unanswered.
    close_after_store: AtomicUsize,
    /// Next N sends: the message lands, then the reply is never sent.
    hang_after_store: AtomicUsize,
    /// Every `send` is refused at the protocol layer.
    refuse_sends: AtomicBool,
    /// Optional raw read reply for partial or malformed responses.
    read_reply: Mutex<Option<Value>>,
    reads: AtomicUsize,
    /// Every `read` is refused.
    fail_reads: AtomicBool,
}

impl Room {
    fn lines(&self) -> Vec<String> {
        self.messages
            .lock()
            .unwrap()
            .iter()
            .map(|(_, body)| body.clone())
            .collect()
    }

    fn count_containing(&self, needle: &str) -> usize {
        self.lines().iter().filter(|l| l.contains(needle)).count()
    }
}

/// Take one fault token from `counter`, if any.
fn take(counter: &AtomicUsize) -> bool {
    counter
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
        .is_ok()
}

async fn serve(stream: tokio::net::UnixStream, room: Arc<Room>) {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let req: Value = serde_json::from_str(&line).unwrap();
        let id = req["id"].clone();
        let reply = match req["op"].as_str().unwrap() {
            "hello" => json!({"ok": true, "id": id}),
            "send" => {
                room.sends.fetch_add(1, Ordering::SeqCst);
                if room.refuse_sends.load(Ordering::SeqCst) {
                    json!({"ok": false, "error": "'room' required: 2 rooms joined", "id": id})
                } else if take(&room.close_before_store) {
                    return;
                } else {
                    room.messages.lock().unwrap().push((
                        Utc::now().timestamp_millis(),
                        req["body"].as_str().unwrap().to_owned(),
                    ));
                    if take(&room.close_after_store) {
                        return;
                    }
                    if take(&room.hang_after_store) {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                    json!({"ok": true, "event_id": "$evt", "id": id})
                }
            }
            "read" => {
                room.reads.fetch_add(1, Ordering::SeqCst);
                if let Some(mut reply) = room.read_reply.lock().unwrap().clone() {
                    reply["id"] = id;
                    reply
                } else if room.fail_reads.load(Ordering::SeqCst) {
                    json!({"ok": false, "error": "read refused", "id": id})
                } else {
                    let limit = req["limit"].as_u64().unwrap_or(20) as usize;
                    let all = room.messages.lock().unwrap();
                    let start = all.len().saturating_sub(limit);
                    let rows: Vec<Value> = all[start..]
                        .iter()
                        .map(|(ts, body)| {
                            json!({
                                "event_id": "$x", "sender": "@someone:example.org",
                                "own": false, "ts": ts, "envelope": {"body": body},
                            })
                        })
                        .collect();
                    json!({"ok": true, "messages": rows, "undecryptable": 0, "id": id})
                }
            }
            other => json!({"ok": false, "error": format!("unknown op {other}"), "id": id}),
        };
        let mut out = reply.to_string();
        out.push('\n');
        if write_half.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Start the fake room on a socket inside `dir`; every connection is served.
fn start_room(dir: &Path) -> (PathBuf, Arc<Room>) {
    let socket = dir.join("safehoused.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let room = Arc::new(Room::default());
    let served = Arc::clone(&room);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(stream, Arc::clone(&served)));
        }
    });
    (socket, room)
}

fn target(socket: &Path) -> ReportTarget {
    ReportTarget {
        socket: socket.to_owned(),
        persona: "loom_daemon".to_owned(),
        room: Some("!fleet:example.org".to_owned()),
        rpc_timeout: Duration::from_secs(5),
        state: new_shared_state(),
    }
}

fn merged(number: u32, slug: &str, age: chrono::Duration) -> MergedPr {
    MergedPr {
        number,
        url: format!("https://github.com/{slug}/pull/{number}"),
        title: Some(format!("change {number}")),
        merged_at: Utc::now() - age,
    }
}

fn report_for(pr: &MergedPr) -> PrReport {
    PrReport::from_merged(pr).unwrap()
}

fn hours(n: i64) -> chrono::Duration {
    chrono::Duration::hours(n)
}

fn minutes(n: i64) -> chrono::Duration {
    chrono::Duration::minutes(n)
}

// ---- config -----------------------------------------------------------------

#[test]
fn config_defaults_to_disabled_with_the_default_persona() {
    let cfg = SafehouseConfig::default();
    assert!(!cfg.enabled);
    assert_eq!(cfg.persona, "loom_daemon");
    assert_eq!(cfg.signal_room(), None);
    assert_eq!(config_from_value(None), cfg);
    assert_eq!(config_from_value(Some(&json!("not an object"))), cfg);
}

#[test]
fn config_reads_the_block_and_prefers_the_signal_room() {
    let block = json!({
        "enabled": true,
        "socket": "/run/safehoused.sock",
        "room": "!legacy:example.org",
        "persona": "loom_bot",
        "rooms": {"signal": " !fleet:example.org "},
    });
    let cfg = config_from_value(Some(&block));
    assert!(cfg.enabled);
    assert_eq!(cfg.socket, Some(PathBuf::from("/run/safehoused.sock")));
    assert_eq!(cfg.persona, "loom_bot");
    assert_eq!(cfg.signal_room(), Some("!fleet:example.org"));

    let legacy = config_from_value(Some(&json!({"room": "!legacy:example.org"})));
    assert_eq!(legacy.signal_room(), Some("!legacy:example.org"));
    // A blank signal room falls back to the scalar room.
    let blank = config_from_value(Some(&json!({"room": "!r:x", "rooms": {"signal": "  "}})));
    assert_eq!(blank.signal_room(), Some("!r:x"));
}

#[test]
fn retired_narration_and_claims_keys_are_named_but_accepted() {
    let block = json!({
        "enabled": true,
        "operatorMention": "@op:example.org",
        "peerClaimTtlSecs": 30,
        "rooms": {"signal": "!s:x", "claims": "!c:x", "byRepo": {"loom": "!l:x"}, "alerts": "!a:x"},
    });
    let keys = retired_keys(Some(&block), |_| false);
    for expected in [
        "safehouse.rooms.claims",
        "safehouse.rooms.byRepo",
        "safehouse.rooms.alerts",
        "safehouse.operatorMention",
        "safehouse.peerClaimTtlSecs",
    ] {
        assert!(keys.iter().any(|k| k == expected), "{expected} missing from {keys:?}");
    }
    assert!(!keys.iter().any(|k| k == "safehouse.rooms.signal"));
    // Still parsed without error, with the retired keys ignored.
    let cfg = config_from_value(Some(&block));
    assert_eq!(cfg.signal_room(), Some("!s:x"));

    let env_keys = retired_keys(None, |name| name == "LOOM_SAFEHOUSE_ROOMS_BY_REPO");
    assert_eq!(env_keys, vec!["LOOM_SAFEHOUSE_ROOMS_BY_REPO".to_owned()]);
    assert!(retired_keys(Some(&json!({"enabled": true})), |_| false).is_empty());
}

#[test]
fn the_retired_key_warning_fires_once() {
    let once = std::sync::Once::new();
    let keys = vec!["safehouse.rooms.byRepo".to_owned()];
    assert!(!warn_retired_keys_once(&once, &[]), "nothing to warn about");
    assert!(warn_retired_keys_once(&once, &keys));
    assert!(!warn_retired_keys_once(&once, &keys), "only the first call warns");
}

#[test]
#[serial]
fn env_overrides_config_and_signal_env_sets_the_room() {
    for key in [
        ENABLED_ENV,
        SOCKET_ENV,
        ROOM_ENV,
        PERSONA_ENV,
        ROOM_SIGNAL_ENV,
        SAFEHOUSED_SOCKET_ENV,
    ] {
        std::env::remove_var(key);
    }
    let base = config_from_value(Some(&json!({"enabled": false, "persona": "from_config"})));
    assert_eq!(apply_env_overrides(base.clone()), base, "no env ⇒ unchanged");

    std::env::set_var(ENABLED_ENV, "yes");
    std::env::set_var(PERSONA_ENV, "from_env");
    std::env::set_var(ROOM_SIGNAL_ENV, "!fleet:example.org");
    let cfg = apply_env_overrides(base);
    assert!(cfg.enabled);
    assert_eq!(cfg.persona, "from_env");
    assert_eq!(cfg.signal_room(), Some("!fleet:example.org"));
    for key in [ENABLED_ENV, PERSONA_ENV, ROOM_SIGNAL_ENV] {
        std::env::remove_var(key);
    }
}

#[test]
#[serial]
fn socket_env_beats_the_configured_path() {
    std::env::remove_var(SOCKET_ENV);
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
    let cfg = SafehouseConfig {
        socket: Some(PathBuf::from("/from/config.sock")),
        ..SafehouseConfig::default()
    };
    assert_eq!(resolve_socket(&cfg), Some(PathBuf::from("/from/config.sock")));
    std::env::set_var(SAFEHOUSED_SOCKET_ENV, "/from/env.sock");
    assert_eq!(resolve_socket(&cfg), Some(PathBuf::from("/from/env.sock")));
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
    assert_eq!(resolve_socket(&SafehouseConfig::default()), None);
}

// ---- connection state -------------------------------------------------------

#[test]
fn the_state_cell_starts_not_configured_and_renders_each_state() {
    let cell = new_shared_state();
    assert_eq!(snapshot_state(&cell), SafehouseState::NotConfigured);
    assert_eq!(SafehouseState::NotConfigured.to_status().state, "not_configured");

    let socket = PathBuf::from("/run/s.sock");
    set_state(
        &cell,
        SafehouseState::Unreachable {
            socket: socket.clone(),
        },
    );
    assert_eq!(snapshot_state(&cell).to_status().state, "unreachable");
    set_state(
        &cell,
        SafehouseState::Connected {
            socket: socket.clone(),
            room: Some("!r:x".to_owned()),
        },
    );
    let status = snapshot_state(&cell).to_status();
    assert_eq!((status.state.as_str(), status.room.as_deref()), ("connected", Some("!r:x")));
    set_state(
        &cell,
        SafehouseState::SendRejected {
            socket,
            reason: "'room' required".to_owned(),
        },
    );
    let status = snapshot_state(&cell).to_status();
    assert_eq!(status.state, "send_rejected");
    assert_eq!(status.reason.as_deref(), Some("'room' required"));
    set_not_configured(&cell);
    assert_eq!(snapshot_state(&cell), SafehouseState::NotConfigured);
}

#[test]
fn a_disabled_config_subscribes_nothing_and_reports_not_configured() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let bus = crate::event_bus::EventBus::new();
    let state = new_shared_state();
    set_state(
        &state,
        SafehouseState::Unreachable {
            socket: PathBuf::from("/stale"),
        },
    );
    let handle = spawn_merge_reporter(
        &SafehouseConfig::default(),
        &bus,
        rt.handle(),
        state.clone(),
        PathBuf::from("/nonexistent"),
    );
    assert!(handle.is_none(), "disabled ⇒ no task");
    assert_eq!(snapshot_state(&state), SafehouseState::NotConfigured);
}

#[tokio::test]
#[serial]
async fn an_enabled_reporter_connects_once_at_start_so_status_reads_connected() {
    std::env::remove_var(SOCKET_ENV);
    std::env::remove_var(SAFEHOUSED_SOCKET_ENV);
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let bus = crate::event_bus::EventBus::new();
    let state = new_shared_state();
    let config = SafehouseConfig {
        enabled: true,
        socket: Some(socket.clone()),
        room: Some("!fleet:example.org".to_owned()),
        ..SafehouseConfig::default()
    };
    let handle = spawn_merge_reporter(
        &config,
        &bus,
        &tokio::runtime::Handle::current(),
        state.clone(),
        dir.path().to_owned(),
    )
    .expect("enabled with a socket ⇒ a reporter task");
    let mut status = snapshot_state(&state);
    for _ in 0..100 {
        if matches!(status, SafehouseState::Connected { .. }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        status = snapshot_state(&state);
    }
    assert_eq!(
        status,
        SafehouseState::Connected {
            socket,
            room: Some("!fleet:example.org".to_owned()),
        }
    );
    assert!(room.lines().is_empty(), "the startup probe posts nothing");
    handle.abort();
}

// ---- wire builder -----------------------------------------------------------

fn chat(body: &str) -> Envelope {
    Envelope {
        to: "*".to_owned(),
        kind: "chat".to_owned(),
        task_id: None,
        body: body.to_owned(),
    }
}

#[test]
fn the_send_request_has_no_from_and_omits_absent_keys() {
    let req = build_send_request(&chat("hi"), 7, None).unwrap();
    assert_eq!(req["op"], json!("send"));
    assert_eq!(req["id"], json!(7));
    assert_eq!(req["v"], json!(ENVELOPE_VERSION));
    assert_eq!(req["to"], json!("*"));
    assert_eq!(req["type"], json!("chat"));
    assert!(req.get("from").is_none(), "safehoused stamps from");
    assert!(req.get("room").is_none());
    assert!(req.get("task_id").is_none());
    assert!(req.get("meta").is_none());

    let mut env = chat("hi");
    env.task_id = Some("42".to_owned());
    let req = build_send_request(&env, 1, Some("!r:x")).unwrap();
    assert_eq!(req["room"], json!("!r:x"));
    assert_eq!(req["task_id"], json!("42"));
}

#[test]
fn the_send_request_refuses_bad_types_task_ids_and_recipients() {
    let mut env = chat("x");
    env.kind = "completion".to_owned();
    assert!(build_send_request(&env, 1, None).is_err(), "Loom no longer sends completions");
    env.kind = "bogus".to_owned();
    assert!(build_send_request(&env, 1, None).is_err());
    let mut env = chat("x");
    env.task_id = Some("has-dash".to_owned());
    assert!(build_send_request(&env, 1, None).is_err());
    env.task_id = Some(String::new());
    assert!(build_send_request(&env, 1, None).is_err());
    let mut env = chat("x");
    env.to = "Not A Persona".to_owned();
    assert!(build_send_request(&env, 1, None).is_err());
    env.to = "Loom-Builder".to_owned();
    assert_eq!(build_send_request(&env, 1, None).unwrap()["to"], json!("loom_builder"));
    env.to = "@op:example.org".to_owned();
    assert_eq!(build_send_request(&env, 1, None).unwrap()["to"], json!("@op:example.org"));
}

// ---- client -----------------------------------------------------------------

#[tokio::test]
async fn the_client_sends_and_reads_back_through_the_room() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let mut client = SafehouseClient::connect(&socket, "loom_daemon")
        .await
        .unwrap();
    let event_id = client.send_to(&chat("first"), Some("!r:x")).await.unwrap();
    assert_eq!(event_id.as_deref(), Some("$evt"));
    client.send_to(&chat("second"), Some("!r:x")).await.unwrap();
    assert_eq!(
        client.read_recent(Some("!r:x"), 10).await.unwrap().bodies,
        vec!["first".to_owned(), "second".to_owned()]
    );
    assert_eq!(client.read_recent(None, 1).await.unwrap().bodies, vec!["second".to_owned()]);
    assert_eq!(room.sends.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_refused_request_is_a_rejection_not_a_transport_failure() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    room.refuse_sends.store(true, Ordering::SeqCst);
    room.fail_reads.store(true, Ordering::SeqCst);
    let mut client = SafehouseClient::connect(&socket, "loom_daemon")
        .await
        .unwrap();
    assert!(matches!(
        client.send_to(&chat("x"), None).await,
        Err(SendError::Rejected { .. })
    ));
    assert!(matches!(client.read_recent(None, 5).await, Err(SendError::Rejected { .. })));
}

#[tokio::test]
async fn read_recent_skips_rows_without_a_body_and_push_lines() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("s.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (r, mut w) = stream.into_split();
        let mut lines = BufReader::new(r).lines();
        for _ in 0..2 {
            let req: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let id = req["id"].clone();
            let reply = if req["op"] == "hello" {
                json!({"ok": true, "id": id})
            } else {
                // A push line first, then rows: one with a body, one an attachment,
                // one an unsupported envelope version.
                w.write_all(b"{\"event\":\"message\",\"body\":\"pushed\"}\n")
                    .await
                    .unwrap();
                json!({"ok": true, "id": id, "messages": [
                    {"event_id": "$1", "envelope": {"body": "kept"}},
                    {"event_id": "$2", "attachment": {"name": "a.png"}},
                    {"event_id": "$3", "unsupported_version": 9},
                ]})
            };
            w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
        }
    });
    let mut client = SafehouseClient::connect(&socket, "loom_daemon")
        .await
        .unwrap();
    let recent = client.read_recent(None, 20).await.unwrap();
    assert_eq!(recent.bodies, vec!["kept".to_owned()]);
    assert!(!recent.complete);
}

// ---- the delivery rule ------------------------------------------------------

#[tokio::test]
async fn a_merge_posts_one_line_to_the_fleet_room() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let t = target(&socket);
    let r = report_for(&merged(13946, "2AMLogic/loom", minutes(1)));
    assert_eq!(merge_report::post(&t, &r).await, Outcome::Posted);
    assert_eq!(
        room.lines(),
        vec!["merged ✓ 2AMLogic/loom#13946 · change 13946 · https://github.com/2AMLogic/loom/pull/13946".to_owned()]
    );
    assert_eq!(room.sends.load(Ordering::SeqCst), 1, "no read or retry on the happy path");
    assert!(matches!(snapshot_state(&t.state), SafehouseState::Connected { .. }));
}

#[tokio::test]
async fn a_lost_reply_for_a_line_that_landed_is_not_resent() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    room.close_after_store.store(1, Ordering::SeqCst);
    let r = report_for(&merged(5, "a/b", minutes(1)));
    assert_eq!(merge_report::post(&target(&socket), &r).await, Outcome::AlreadyReported);
    assert_eq!(room.count_containing("a/b#5"), 1, "read-back saw the line; no second post");
    assert_eq!(room.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_lost_reply_for_a_line_that_did_not_land_is_resent_exactly_once() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    room.close_before_store.store(1, Ordering::SeqCst);
    let r = report_for(&merged(5, "a/b", minutes(1)));
    assert_eq!(merge_report::post(&target(&socket), &r).await, Outcome::Posted);
    assert_eq!(room.count_containing("a/b#5"), 1);
    assert_eq!(room.sends.load(Ordering::SeqCst), 2, "one lost send, one retry");
}

#[tokio::test]
async fn a_timed_out_reply_counts_as_lost_and_reads_back() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    room.hang_after_store.store(1, Ordering::SeqCst);
    let mut t = target(&socket);
    t.rpc_timeout = Duration::from_millis(300);
    let r = report_for(&merged(5, "a/b", minutes(1)));
    assert_eq!(merge_report::post(&t, &r).await, Outcome::AlreadyReported);
    assert_eq!(room.count_containing("a/b#5"), 1);
}

#[tokio::test]
async fn an_unreadable_room_after_a_lost_reply_never_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    room.close_before_store.store(1, Ordering::SeqCst);
    room.fail_reads.store(true, Ordering::SeqCst);
    let r = report_for(&merged(5, "a/b", minutes(1)));
    let outcome = merge_report::post(&target(&socket), &r).await;
    assert!(
        matches!(&outcome, Outcome::Uncertain(why) if why.contains("read-back failed")),
        "{outcome:?}"
    );
    assert_eq!(room.sends.load(Ordering::SeqCst), 1, "unknown stays unknown: no blind retry");
    assert!(room.lines().is_empty());
}

#[tokio::test]
async fn a_second_lost_reply_gives_up_instead_of_looping() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    room.close_before_store.store(2, Ordering::SeqCst);
    let r = report_for(&merged(5, "a/b", minutes(1)));
    assert!(matches!(merge_report::post(&target(&socket), &r).await, Outcome::Uncertain(_)));
    assert_eq!(room.sends.load(Ordering::SeqCst), 2, "at most two sends");
}

#[tokio::test]
async fn a_refused_send_is_not_retried_and_sets_send_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    room.refuse_sends.store(true, Ordering::SeqCst);
    let t = target(&socket);
    let r = report_for(&merged(5, "a/b", minutes(1)));
    let outcome = merge_report::post(&t, &r).await;
    assert!(
        matches!(&outcome, Outcome::Failed(why) if why.contains("rejected")),
        "{outcome:?}"
    );
    assert_eq!(room.sends.load(Ordering::SeqCst), 1);
    assert!(matches!(snapshot_state(&t.state), SafehouseState::SendRejected { .. }));
    assert!(merge_report::read_room(&t).await.unwrap().complete);
    assert!(matches!(snapshot_state(&t.state), SafehouseState::SendRejected { .. }));
    assert!(room.lines().is_empty());
    room.refuse_sends.store(false, Ordering::SeqCst);
    assert_eq!(merge_report::post(&t, &r).await, Outcome::Posted);
    assert!(matches!(snapshot_state(&t.state), SafehouseState::Connected { .. }));
}

#[tokio::test]
async fn an_unreachable_safehoused_fails_without_a_send() {
    let dir = tempfile::tempdir().unwrap();
    let t = target(&dir.path().join("absent.sock"));
    let r = report_for(&merged(5, "a/b", minutes(1)));
    assert!(matches!(merge_report::post(&t, &r).await, Outcome::Failed(_)));
    assert!(matches!(snapshot_state(&t.state), SafehouseState::Unreachable { .. }));
}

#[tokio::test]
async fn the_same_pr_number_in_two_repos_is_two_lines() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let t = target(&socket);
    assert_eq!(
        merge_report::post(&t, &report_for(&merged(7, "a/one", minutes(1)))).await,
        Outcome::Posted
    );
    assert_eq!(
        merge_report::post(&t, &report_for(&merged(7, "a/two", minutes(1)))).await,
        Outcome::Posted
    );
    assert_eq!(room.count_containing("a/one#7"), 1);
    assert_eq!(room.count_containing("a/two#7"), 1);
}

// ---- host path --------------------------------------------------------------

fn reporter(socket: &Path, root: &Path) -> Reporter {
    Reporter::new(target(socket), root.to_owned()).with_windows(hours(6), minutes(10))
}

#[tokio::test]
async fn the_host_posts_a_merge_once_however_many_events_see_it() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let host = reporter(&socket, dir.path());
    let pr = merged(9, "a/b", minutes(1));
    host.report_host_merge(&pr, Trigger::SweepMerge).await;
    host.report_host_merge(&pr, Trigger::Step).await;
    assert_eq!(
        room.count_containing("a/b#9"),
        1,
        "sweep merge phase and merge step share one report"
    );
}

#[tokio::test]
async fn the_host_ignores_a_merge_too_old_to_be_the_one_it_just_made() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let host = reporter(&socket, dir.path());
    host.report_host_merge(&merged(9, "a/b", hours(7)), Trigger::SweepMerge)
        .await;
    assert!(room.lines().is_empty(), "a stale merge on a reused branch is not reported");
}

#[tokio::test]
async fn a_failed_host_post_is_retried_by_a_later_event() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let host = reporter(&socket, dir.path());
    let pr = merged(9, "a/b", minutes(1));
    room.refuse_sends.store(true, Ordering::SeqCst);
    host.report_host_merge(&pr, Trigger::SweepMerge).await;
    assert!(room.lines().is_empty());
    room.refuse_sends.store(false, Ordering::SeqCst);
    host.report_host_merge(&pr, Trigger::Step).await;
    assert_eq!(room.count_containing("a/b#9"), 1);
}

#[tokio::test]
async fn partial_and_malformed_reads_never_authorize_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let host = reporter(&socket, dir.path());
    let pr = merged(9, "a/b", minutes(1));
    room.close_after_store.store(1, Ordering::SeqCst);
    for reply in [
        json!({"ok": true, "messages": [], "undecryptable": 1}),
        json!({"ok": true, "undecryptable": 0}),
        json!({"ok": true, "messages": {}, "undecryptable": 0}),
        json!({"ok": true, "messages": []}),
    ] {
        *room.read_reply.lock().unwrap() = Some(reply);
        host.report_host_merge(&pr, Trigger::Step).await;
        assert_eq!(room.sends.load(Ordering::SeqCst), 1);
    }
    // A visible match settles the report even while another event is unreadable.
    *room.read_reply.lock().unwrap() = Some(json!({
        "ok": true, "messages": [{"envelope": {"body": room.lines()[0]}}], "undecryptable": 1
    }));
    host.report_host_merge(&pr, Trigger::Step).await;
    host.report_host_merge(&pr, Trigger::Step).await;
    assert_eq!(room.reads.load(Ordering::SeqCst), 5);
    assert_eq!(room.lines().len(), 1);
}

#[tokio::test]
async fn a_later_host_event_reads_before_resending_an_uncertain_report() {
    for landed in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (socket, room) = start_room(dir.path());
        let host = reporter(&socket, dir.path());
        let pr = merged(9, "a/b", minutes(1));
        if landed {
            room.close_after_store.store(1, Ordering::SeqCst);
        } else {
            room.close_before_store.store(1, Ordering::SeqCst);
        }
        room.fail_reads.store(true, Ordering::SeqCst);
        host.report_host_merge(&pr, Trigger::SweepMerge).await;
        assert_eq!(room.sends.load(Ordering::SeqCst), 1);
        room.fail_reads.store(false, Ordering::SeqCst);
        host.report_host_merge(&pr, Trigger::Step).await;
        assert_eq!(room.reads.load(Ordering::SeqCst), 2);
        assert_eq!(room.sends.load(Ordering::SeqCst), if landed { 1 } else { 2 });
        assert_eq!(room.lines().len(), 1);
    }
}

#[tokio::test]
async fn an_ambiguous_host_send_hidden_by_newer_messages_is_not_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let host = reporter(&socket, dir.path());
    let pr = merged(9, "a/b", minutes(1));
    room.close_after_store.store(1, Ordering::SeqCst);
    room.fail_reads.store(true, Ordering::SeqCst);
    host.report_host_merge(&pr, Trigger::Step).await;
    room.fail_reads.store(false, Ordering::SeqCst);
    let ts = Utc::now().timestamp_millis();
    room.messages
        .lock()
        .unwrap()
        .extend((0..merge_report::READ_BACK_LIMIT).map(|_| (ts, "later message".to_owned())));
    host.report_host_merge(&pr, Trigger::Step).await;
    assert_eq!(room.sends.load(Ordering::SeqCst), 1);
    assert_eq!(room.reads.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn a_lost_acknowledgement_on_the_second_send_still_requires_a_fresh_read() {
    let dir = tempfile::tempdir().unwrap();
    let (socket, room) = start_room(dir.path());
    let host = reporter(&socket, dir.path());
    let pr = merged(9, "a/b", minutes(1));
    room.close_before_store.store(1, Ordering::SeqCst);
    room.close_after_store.store(1, Ordering::SeqCst);
    host.report_host_merge(&pr, Trigger::SweepMerge).await;
    host.report_host_merge(&pr, Trigger::Step).await;
    assert_eq!(room.reads.load(Ordering::SeqCst), 2);
    assert_eq!(room.sends.load(Ordering::SeqCst), 2);
    assert_eq!(room.lines().len(), 1);
}

// ---- captain path -----------------------------------------------------------

/// Isolates the workspace registry the captain scan consults and the `gh`
/// binary it runs; restored on drop.
struct CaptainEnv;

impl CaptainEnv {
    fn set(gh: &Path, registry: &Path) -> Self {
        std::env::set_var(GH_BIN_ENV, gh);
        std::env::set_var(crate::workspace_registry::REGISTRY_PATH_ENV, registry);
        Self
    }
}

impl Drop for CaptainEnv {
    fn drop(&mut self) {
        std::env::remove_var(GH_BIN_ENV);
        std::env::remove_var(crate::workspace_registry::REGISTRY_PATH_ENV);
    }
}

/// A fake `gh` whose stdout is `rows.json` next to it, failing while `fail`
/// exists, and logging every invocation.
struct FakeGh {
    dir: PathBuf,
}

impl FakeGh {
    fn new(dir: &Path) -> Self {
        let dir = dir.join("gh");
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("gh.sh");
        // Search in fixture (creation) order, with the forge's inclusive
        // second-resolution merge filter and row limit.
        let body = r#"#!/usr/bin/env bash
set -eu
dir=$(dirname "$0")
echo "$*" >> "$dir/calls.log"
if [[ -f "$dir/fail" ]]; then exit 1; fi
if [[ -f "$dir/fail-call" && $(wc -l < "$dir/calls.log") -eq $(cat "$dir/fail-call") ]]; then
    exit 1
fi
verb=$2
number=$3
repo=${GH_REPO:-unset}
while [[ $# -gt 0 ]]; do
    case "$1" in
        --repo) repo=${2#github.com/}; shift ;;
        --search) window=${2#merged:}; shift ;;
        --limit) limit=$2; shift ;;
    esac
    shift
done
prefix="https://github.com/$repo/pull/"
if [[ "$verb" == view ]]; then
    exec jq --arg prefix "$prefix" --argjson number "$number" \
        'first(.[] | select((.url | startswith($prefix)) and .number == $number))' "$dir/rows.json"
fi
exec jq --arg prefix "$prefix" --arg lo "${window%..*}" --arg hi "${window#*..}" --argjson limit "$limit" \
    '[.[] | select((.url | startswith($prefix)) and .mergedAt >= $lo and .mergedAt <= $hi)][: $limit]' "$dir/rows.json"
"#;
        std::fs::write(&script, body).unwrap();
        let mut perms = std::fs::metadata(&script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).unwrap();
        let gh = Self { dir };
        gh.rows(&[]);
        gh
    }

    fn bin(&self) -> PathBuf {
        self.dir.join("gh.sh")
    }

    fn rows(&self, rows: &[Value]) {
        std::fs::write(self.dir.join("rows.json"), Value::Array(rows.to_vec()).to_string())
            .unwrap();
    }

    fn fail(&self, failing: bool) {
        let flag = self.dir.join("fail");
        if failing {
            std::fs::write(flag, "").unwrap();
        } else {
            let _ = std::fs::remove_file(flag);
        }
    }

    fn calls(&self) -> usize {
        std::fs::read_to_string(self.dir.join("calls.log"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }
}

fn row(number: u32, slug: &str, age: chrono::Duration) -> Value {
    json!({
        "number": number,
        "url": format!("https://github.com/{slug}/pull/{number}"),
        "mergedAt": (Utc::now() - age).to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "title": format!("change {number}"),
    })
}

/// A workspace whose config names `captain`, plus the env/fake-gh scaffolding.
struct Captain {
    repos: crate::forge_repo_facts::test_support::Env,
    _env: CaptainEnv,
    root: PathBuf,
    gh: FakeGh,
    room: Arc<Room>,
    socket: PathBuf,
}

fn captain_fixture(dir: &Path, captain: &str) -> Captain {
    let repos = crate::forge_repo_facts::test_support::Env::new(&[]);
    let root = repos.repo("workspace", &[("origin", "https://github.com/a/b.git")]);
    std::fs::create_dir_all(root.join(".loom")).unwrap();
    std::fs::write(
        root.join(".loom/config.json"),
        json!({"fleet": {"captain": captain}}).to_string(),
    )
    .unwrap();
    let gh = FakeGh::new(dir);
    let env = CaptainEnv::set(&gh.bin(), &dir.join("workspaces.json"));
    let (socket, room) = start_room(dir);
    // Normal catch-up fixtures have a visible message predating their merge window.
    room.messages
        .lock()
        .unwrap()
        .push(((Utc::now() - hours(8)).timestamp_millis(), "hello".into()));
    Captain {
        repos,
        _env: env,
        root,
        gh,
        room,
        socket,
    }
}

fn this_host() -> String {
    crate::sweep_registry::host_identity()
}

#[tokio::test]
#[serial]
async fn host_looks_up_the_numbered_merge_on_a_reused_branch() {
    use crate::types::Event;
    let _socket_env =
        crate::env_test_support::EnvGuard::clear(&[SOCKET_ENV, SAFEHOUSED_SOCKET_ENV]);
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), "another-host");
    c.gh.rows(&[row(36, "a/b", minutes(50)), row(50, "a/b", minutes(1))]);
    let bus = crate::event_bus::EventBus::new();
    let handle = spawn_merge_reporter(
        &SafehouseConfig {
            enabled: true,
            socket: Some(c.socket.clone()),
            ..Default::default()
        },
        &bus,
        &tokio::runtime::Handle::current(),
        new_shared_state(),
        c.root.clone(),
    )
    .unwrap();
    let repo = Some(c.root.to_str().unwrap().to_owned());
    bus.publish(Event::SweepExited {
        issue: 9,
        exit_code: None,
        duration_sec: 20,
        no_progress: false,
        death_class: None,
        repo: repo.clone(),
    })
    .unwrap();
    bus.publish(Event::SweepPhase {
        issue: 9,
        phase: "merge".into(),
        pr_number: Some(50),
        repo,
    })
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while c.room.count_containing("a/b#50 ·") == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    handle.abort();
    let calls = std::fs::read_to_string(c.gh.dir.join("calls.log")).unwrap();
    assert_eq!(
        calls.lines().collect::<Vec<_>>(),
        ["pr view 50 --json number,url,mergedAt,title --repo github.com/a/b"]
    );
    assert_eq!(c.room.count_containing("a/b#50 ·"), 1);
    assert_eq!(c.room.count_containing("a/b#36 ·"), 0);
}

#[tokio::test]
#[serial]
async fn the_captain_posts_a_merge_no_host_reported() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    let captain = reporter(&c.socket, &c.root);
    assert_eq!(captain.captain_pass().await, 1);
    assert_eq!(c.room.count_containing("a/b#5"), 1);
    // A second pass finds the line in the room and posts nothing.
    assert_eq!(captain.captain_pass().await, 0);
    assert_eq!(c.room.count_containing("a/b#5"), 1);
}

#[tokio::test]
#[serial]
async fn an_empty_room_defers_backfill_and_the_merging_host_can_post_first() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    c.room.messages.lock().unwrap().clear();
    let captain = reporter(&c.socket, &c.root);
    assert_eq!(captain.captain_pass().await, 0);
    assert!(captain.watermark(&c.root).is_none());
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 0);
    // A confirmed host merge does not need historical room coverage.
    let host = reporter(&c.socket, &c.root);
    host.report_host_merge(&merged(5, "a/b", minutes(60)), Trigger::SweepMerge)
        .await;
    assert_eq!(c.room.count_containing("a/b#5"), 1);
    // The captain's own reporter has no memory of the host post.
    assert_eq!(captain.captain_pass().await, 0);
    assert_eq!(c.room.count_containing("a/b#5"), 1, "two reporters, one line");
}

#[tokio::test]
#[serial]
async fn out_of_range_report_windows_use_their_own_defaults() {
    // u64::MAX, one second beyond Chrono's duration limit, and a valid
    // duration that would put the cutoff before Chrono's earliest date.
    for invalid in ["18446744073709551615", "9223372036854776", "10000000000000"] {
        for (catchup, grace, expected) in [(invalid, "0", 3), ("7200", invalid, 1)] {
            let _windows = crate::env_test_support::EnvGuard::set(
                "LOOM_SAFEHOUSE_REPORT_CATCHUP_SECS",
                catchup,
            )
            .and_set("LOOM_SAFEHOUSE_REPORT_GRACE_SECS", Some(grace.as_ref()));
            let dir = tempfile::tempdir().unwrap();
            let c = captain_fixture(dir.path(), &this_host());
            c.gh.rows(&[
                row(1, "a/b", hours(7)),
                row(2, "a/b", hours(5)),
                row(3, "a/b", hours(1)),
                row(4, "a/b", minutes(5)),
            ]);
            let captain = Reporter::new(target(&c.socket), c.root.clone());
            assert_eq!(captain.captain_pass().await, expected, "catchup={catchup}, grace={grace}");
            assert_eq!(c.room.count_containing("a/b#1 ·"), 0);
            assert_eq!(c.room.count_containing("a/b#3 ·"), 1);
            assert!(captain.watermark(&c.root).is_some());
        }
    }
}

#[tokio::test]
#[serial]
async fn the_captain_leaves_a_fresh_merge_to_the_host_that_made_it() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(2))]);
    let captain = reporter(&c.socket, &c.root);
    assert_eq!(captain.captain_pass().await, 0, "inside the grace period");
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial]
async fn host_and_captain_select_each_workspaces_origin_over_the_ambient_repo() {
    let _ambient = crate::env_test_support::EnvGuard::set("GH_REPO", "ambient/repo")
        .and_set("LOOM_REPO", Some("ambient/repo".as_ref()));
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    let second = c
        .repos
        .repo("second", &[("origin", "git@github.com:a/two.git")]);
    let mut registry = crate::workspace_registry::WorkspaceRegistry::default();
    registry.add(&c.root, None).unwrap();
    registry.add(&second, None).unwrap();
    registry.save_default().unwrap();
    let mut unmerged = row(8, "a/b", minutes(60));
    unmerged["mergedAt"] = Value::Null;
    c.gh.rows(&[
        row(7, "ambient/repo", minutes(60)),
        row(7, "a/b", minutes(60)),
        row(7, "a/two", minutes(50)),
        unmerged,
    ]);
    for (root, slug) in [(&c.root, "a/b"), (&second, "a/two")] {
        let pr = merge_source::merged_pr_by_number(root, 7).await.unwrap();
        assert_eq!(pr.repo_slug().as_deref(), Some(slug));
    }
    let calls = c.gh.calls();
    assert!(merge_source::merged_pr_by_number(dir.path(), 7)
        .await
        .is_none());
    assert_eq!(c.gh.calls(), calls, "a missing origin must never query the ambient repo");

    let captain = reporter(&c.socket, &c.root);
    assert_eq!(captain.captain_pass().await, 2);
    assert_eq!(c.room.count_containing("a/b#7"), 1);
    assert_eq!(c.room.count_containing("a/two#7"), 1);
    assert!(captain.watermark(&c.root).is_some());
    assert!(captain.watermark(&second).is_some());
    assert_eq!(c.room.count_containing("ambient/repo"), 0);
    assert_eq!(c.room.count_containing("a/b#8"), 0);
}

#[tokio::test]
#[serial]
async fn the_captain_scans_only_current_registry_roots_after_workspace_events() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    let managed = c
        .repos
        .repo("managed", &[("origin", "https://github.com/a/managed.git")]);
    let mut registry = crate::workspace_registry::WorkspaceRegistry::default();
    registry.add(&managed, None).unwrap();
    registry.save_default().unwrap();
    c.gh.rows(&[row(1, "a/b", minutes(60)), row(1, "a/managed", minutes(60))]);

    let captain = Arc::new(reporter(&c.socket, &c.root));
    super::reporter::on_event(
        &captain,
        &crate::types::Event::SweepPhase {
            issue: 1,
            phase: "judge".into(),
            pr_number: Some(1),
            repo: Some(managed.to_str().unwrap().to_owned()),
        },
        &tokio::runtime::Handle::current(),
    );
    assert_eq!(captain.captain_pass().await, 1);
    assert_eq!(c.room.count_containing("a/managed#1 ·"), 1);
    assert_eq!(c.room.count_containing("a/b#1 ·"), 0);

    // Removing the last entry restores the startup checkout as the sole target.
    assert!(registry.remove(&managed));
    registry.save_default().unwrap();
    c.gh.rows(&[row(1, "a/b", minutes(60)), row(2, "a/managed", minutes(20))]);
    assert_eq!(captain.captain_pass().await, 1);
    assert_eq!(c.room.count_containing("a/b#1 ·"), 1);
    assert_eq!(c.room.count_containing("a/managed#2 ·"), 0);
    assert!(captain.watermark(&managed).is_none());

    let calls = std::fs::read_to_string(c.gh.dir.join("calls.log")).unwrap();
    let repos: Vec<_> = calls
        .lines()
        .map(|call| call.rsplit_once(" --repo ").unwrap().1)
        .collect();
    assert_eq!(repos, ["github.com/a/managed", "github.com/a/b"]);
}

#[tokio::test]
#[serial]
async fn an_unreadable_workspace_registry_defers_captain_catch_up() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(1, "a/b", minutes(60))]);
    std::fs::write(dir.path().join("workspaces.json"), "{").unwrap();
    let captain = reporter(&c.socket, &c.root);
    assert_eq!(captain.captain_pass().await, 0);
    assert_eq!(c.gh.calls(), 0);
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 0);
    assert!(captain.watermark(&c.root).is_none());

    crate::workspace_registry::WorkspaceRegistry::default()
        .save_default()
        .unwrap();
    assert_eq!(captain.captain_pass().await, 1);
    assert_eq!(c.room.count_containing("a/b#1 ·"), 1);
}

#[tokio::test]
#[serial]
async fn a_host_that_is_not_the_captain_never_scans_or_posts() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), "some-other-host");
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    let worker = reporter(&c.socket, &c.root);
    assert_eq!(worker.captain_pass().await, 0);
    assert_eq!(c.gh.calls(), 0, "a refused gate makes no forge call");
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[serial]
async fn a_failed_forge_scan_leaves_the_window_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    let captain = reporter(&c.socket, &c.root);
    c.gh.fail(true);
    assert_eq!(captain.captain_pass().await, 0);
    assert!(captain.watermark(&c.root).is_none(), "a failed scan advances nothing");
    c.gh.fail(false);
    assert_eq!(captain.captain_pass().await, 1, "the same window is read again");
    assert!(captain.watermark(&c.root).is_some());
}

#[tokio::test]
#[serial]
async fn an_unreadable_room_stops_the_pass_before_any_post() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    c.room.fail_reads.store(true, Ordering::SeqCst);
    let captain = reporter(&c.socket, &c.root);
    assert_eq!(captain.captain_pass().await, 0);
    assert_eq!(
        c.room.sends.load(Ordering::SeqCst),
        0,
        "without the room read the captain cannot tell what is missing"
    );
    assert!(captain.watermark(&c.root).is_none());
    c.room.fail_reads.store(false, Ordering::SeqCst);
    assert_eq!(captain.captain_pass().await, 1);
}

#[tokio::test]
#[serial]
async fn an_incomplete_room_read_preserves_the_captain_window() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    *c.room.read_reply.lock().unwrap() = Some(json!({"ok":true,"messages":[],"undecryptable":1}));
    let captain = reporter(&c.socket, &c.root);
    assert_eq!(captain.captain_pass().await, 0);
    assert!(captain.watermark(&c.root).is_none());
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 0);
    *c.room.read_reply.lock().unwrap() = None;
    assert_eq!(captain.captain_pass().await, 1);
}

#[tokio::test]
#[serial]
async fn a_fresh_captain_checks_how_far_back_the_room_read_reaches() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    let mut rows = Vec::new();
    let old = Utc::now() - hours(2);
    let boundary = Utc::now() - hours(1);
    {
        let mut messages = c.room.messages.lock().unwrap();
        for n in 1..=20 {
            let pr = merged(n, "a/b", hours(2));
            rows.push(row(n, "a/b", hours(2)));
            messages.push((old.timestamp_millis(), report_for(&pr).body()));
        }
        for _ in 0..merge_report::READ_BACK_LIMIT {
            messages.push((boundary.timestamp_millis(), "ordinary message".into()));
        }
    }
    // This genuinely missing merge is inside the visible history.
    rows.push(row(21, "a/b", minutes(30)));
    c.gh.rows(&rows);
    let captain = reporter(&c.socket, &c.root);
    // A page of reactions hides every earlier host report. Safehoused returns
    // the same payload as an empty room on every pass.
    *c.room.read_reply.lock().unwrap() = Some(json!({"ok":true,"messages":[],"undecryptable":0}));
    for _ in 0..2 {
        assert_eq!(captain.captain_pass().await, 0);
        assert!(captain.watermark(&c.root).is_none());
    }
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 0);
    assert_eq!(c.room.count_containing("merged ✓"), 20);

    *c.room.read_reply.lock().unwrap() = None;
    assert_eq!(captain.captain_pass().await, 1);
    assert_eq!(c.room.count_containing("merged ✓"), 21);
    assert_eq!(c.room.count_containing("a/b#21 ·"), 1);
    assert!(captain.watermark(&c.root).is_none());
    // Safehoused can filter out a non-message event after applying the limit.
    *c.room.read_reply.lock().unwrap() = Some(json!({
        "ok":true,"undecryptable":0,
        "messages":[{"ts":boundary.timestamp_millis(),"envelope":{"body":"ordinary message"}}]
    }));
    assert_eq!(captain.captain_pass().await, 0);
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 1);
}

#[tokio::test]
#[serial]
async fn a_dense_burst_resumes_across_ticks_within_the_query_budget() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    let now = Utc::now();
    // Creation order puts #1 last. All merges fall in a two-minute interval
    // whose split takes more than a single tick's budget to reach.
    let rows: Vec<_> = (1..=101)
        .rev()
        .map(|n| {
            let mut pr = row(n, "a/b", minutes(15));
            pr["mergedAt"] = json!((now - minutes(15) + chrono::Duration::seconds(i64::from(n)))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
            pr
        })
        .collect();
    c.gh.rows(&rows);
    let captain = reporter(&c.socket, &c.root);
    for tick in 0..10 {
        let calls = c.gh.calls();
        captain.captain_pass_at(now + minutes(5 * tick)).await;
        assert!(c.gh.calls() - calls <= 5);
        if tick == 0 {
            assert!(captain.watermark(&c.root).is_none());
        }
        if captain.watermark(&c.root).is_some() {
            break;
        }
    }
    assert!(captain.watermark(&c.root).is_some());
    assert_eq!(c.room.count_containing("merged ✓"), 101);
    assert_eq!(c.room.count_containing("a/b#1 ·"), 1);
}

#[tokio::test]
#[serial]
async fn a_saturated_creation_order_page_does_not_hide_a_late_merge() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    // PR #1 was opened first, returned last, and merged after all the others.
    let mut rows: Vec<_> = (2..=101)
        .rev()
        .map(|n| row(n, "a/b", if n % 2 == 0 { hours(5) } else { hours(2) }))
        .collect();
    rows.push(row(1, "a/b", minutes(15)));
    c.gh.rows(&rows);
    let captain = reporter(&c.socket, &c.root);
    std::fs::write(c.gh.dir.join("fail-call"), "3").unwrap();
    assert_eq!(captain.captain_pass().await, 0, "discovery has not finished");
    assert!(captain.watermark(&c.root).is_none(), "the second half failed");
    assert_eq!(c.room.count_containing("a/b#1 ·"), 0);

    assert_eq!(captain.captain_pass().await, rows.len());
    assert_eq!(c.room.count_containing("a/b#1 ·"), 1);
    assert_eq!(c.room.count_containing("merged ✓"), rows.len());
    assert_eq!(c.gh.calls(), 4, "only the failed half needed a second query");
    assert!(captain.watermark(&c.root).is_some());
}

#[tokio::test]
#[serial]
async fn a_failed_post_leaves_the_merge_for_the_next_pass() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    let captain = reporter(&c.socket, &c.root);
    c.room.refuse_sends.store(true, Ordering::SeqCst);
    assert_eq!(captain.captain_pass().await, 0);
    assert!(captain.watermark(&c.root).is_none(), "an undelivered merge holds the watermark");
    c.room.refuse_sends.store(false, Ordering::SeqCst);
    assert_eq!(captain.captain_pass().await, 1);
    assert_eq!(c.room.count_containing("a/b#5"), 1);
}

#[tokio::test]
#[serial]
async fn a_full_page_that_cannot_be_walked_back_is_not_marked_done() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    // A full page of merges that all share one timestamp: the scan cannot narrow
    // the window past them, so it must not claim to have seen everything.
    let at = Utc::now() - minutes(60);
    let rows: Vec<Value> = (1..=super::merge_source::SCAN_LIMIT)
        .map(|n| {
            json!({
                "number": n,
                "url": format!("https://github.com/a/b/pull/{n}"),
                "mergedAt": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "title": "bulk",
            })
        })
        .collect();
    c.gh.rows(&rows);
    let captain = reporter(&c.socket, &c.root);
    for _ in 0..10 {
        if captain.captain_pass().await > 0 {
            break;
        }
        assert!(captain.watermark(&c.root).is_none());
    }
    assert_eq!(c.room.count_containing("merged ✓"), rows.len());
    assert!(captain.watermark(&c.root).is_none(), "incomplete scan holds the watermark");
    // Re-running posts nothing new: the room read-back already holds every line.
    assert_eq!(captain.captain_pass().await, 0);
    assert_eq!(c.room.count_containing("merged ✓"), rows.len());
}

// ---- rate-limit breaker (#8997) ---------------------------------------------

const RATE_LIMITED: &str = "GraphQL: API rate limit already exceeded for installation ID 151241294";

/// A probe that reads nothing, so the breaker falls back to its fixed cooldown.
struct NoProbe;

impl BudgetProbe for NoProbe {
    fn probe(&self, _ctx: &FailureContext, _now: chrono::DateTime<Utc>) -> Option<BudgetSnapshot> {
        None
    }
}

fn breaker() -> Arc<SharedRateLimitBreaker> {
    Arc::new(SharedRateLimitBreaker::new(RateLimitBreakerConfig {
        enabled: true,
        fallback_cooldown_secs: 60,
    }))
}

fn gate(breaker: &Arc<SharedRateLimitBreaker>) -> Option<BreakerHandle> {
    Some(BreakerHandle {
        breaker: Arc::clone(breaker),
        probe: Arc::new(NoProbe) as Arc<dyn BudgetProbe>,
        mode: ProbeMode::Inline,
    })
}

#[tokio::test]
#[serial]
async fn a_cooling_breaker_defers_the_scan_with_no_forge_call() {
    let dir = tempfile::tempdir().unwrap();
    let c = captain_fixture(dir.path(), &this_host());
    c.gh.rows(&[row(5, "a/b", minutes(60))]);
    let cooling = breaker();
    cooling
        .trip("test", &ResetEvidence::Fallback { reason: "test" }, Utc::now())
        .unwrap();
    let captain = reporter(&c.socket, &c.root);

    let posted = forge_gate::scoped(gate(&cooling), captain.captain_pass()).await;
    assert_eq!(posted, 0);
    assert_eq!(c.gh.calls(), 0, "no `gh` call while the breaker cools");
    assert_eq!(c.room.sends.load(Ordering::SeqCst), 0);
    assert!(captain.watermark(&c.root).is_none(), "the deferred window stays open");

    // Once the breaker is out of the way the same window is scanned and posted.
    assert_eq!(forge_gate::scoped(None, captain.captain_pass()).await, 1);
    assert_eq!(c.room.count_containing("a/b#5"), 1);
}

#[tokio::test]
async fn a_rate_limited_lookup_trips_the_shared_breaker() {
    let dir = tempfile::tempdir().unwrap();
    let shared = breaker();
    let handle = gate(&shared).unwrap();
    assert!(!handle.is_suppressed());
    forge_gate::scoped(gate(&shared), async {
        assert!(!forge_gate::suppressed("pr list (scan)", dir.path()));
        forge_gate::failed("pr list (scan)", dir.path(), "gh", RATE_LIMITED.as_bytes());
        assert!(forge_gate::suppressed("pr list (scan)", dir.path()));
    })
    .await;
    assert!(handle.is_suppressed(), "the failure was reported to the shared breaker");
}
