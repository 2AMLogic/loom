use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;

/// Records every (url, key, body) and answers from a script.
struct Fake {
    seen: Mutex<Vec<(String, String, String)>>,
    answers: Vec<Result<(u16, String), String>>,
}

impl Fake {
    fn new(answers: Vec<Result<(u16, String), String>>) -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
            answers,
        }
    }

    fn calls(&self) -> Vec<(String, String, String)> {
        self.seen.lock().unwrap().clone()
    }
}

impl Transport for Fake {
    fn post_json(&self, url: &str, key: &str, body: &str) -> Result<(u16, String), String> {
        self.seen
            .lock()
            .unwrap()
            .push((url.to_owned(), key.to_owned(), body.to_owned()));
        self.answers[self.seen.lock().unwrap().len() - 1].clone()
    }
}

fn ok(body: &str) -> Result<(u16, String), String> {
    Ok((200, body.to_owned()))
}

#[test]
fn curation_request_shape_and_key_stability() {
    let (a, ka) = curation_request("2AMLogic/loom", 12, 5);
    assert_eq!(
        a,
        serde_json::json!({
            "repo": "2amlogic/loom",
            "number": 12,
            "points": "5",
            "idempotency_key": ka,
        })
    );
    let (_b, kb) = curation_request("2amlogic/loom", 12, 5);
    assert_eq!(ka, kb, "the same projection must replay");
    let (_, kc) = curation_request("2amlogic/loom", 12, 8);
    assert_ne!(ka, kc, "a re-estimate is a new call, not a replay");
    let (_, kd) = curation_request("2amlogic/other", 12, 5);
    assert_ne!(ka, kd);
}

#[test]
fn notify_request_shape_and_key_stability() {
    let (a, ka) = notify_request("2AMLogic/loom", 30, 29);
    assert_eq!(
        a,
        serde_json::json!({
            "repo": "2amlogic/loom",
            "number": 30,
            "closed": 29,
            "idempotency_key": ka,
        })
    );
    let (_, kb) = notify_request("2amlogic/loom", 30, 29);
    assert_eq!(ka, kb);
    let (_, kc) = notify_request("2amlogic/loom", 30, 28);
    assert_ne!(ka, kc, "one key per dependent-and-blocker pair");
}

#[test]
fn complete_curation_posts_the_documented_contract() {
    let fake = Fake::new(vec![ok(
        r#"{"number":12,"points":"5","previous_points":["points:2"],
            "curated":true,"changed":true,"replayed":false}"#,
    )]);
    let broker = Broker::with_transport("http://broker.example", "k1", &fake);
    let out = broker.complete_curation("2AMLogic/loom", 12, 5).unwrap();
    assert!(out.curated && out.changed && !out.replayed);
    assert_eq!(out.previous_points, vec!["points:2".to_owned()]);
    let (url, key, body) = &fake.calls()[0];
    assert_eq!(url, "http://broker.example/v1/complete_curation");
    assert_eq!(key, "k1");
    let v: Value = serde_json::from_str(body).unwrap();
    assert_eq!(v["repo"], "2amlogic/loom");
    assert_eq!(v["number"], 12);
    assert_eq!(v["points"], "5");
    assert!(v["idempotency_key"].as_str().is_some_and(|k| !k.is_empty()));
}

#[test]
fn notify_posts_the_documented_contract_and_parses_a_null_comment() {
    let fake = Fake::new(vec![ok(
        r#"{"number":30,"closed":29,"unblocked":false,"comment":null,"replayed":false}"#,
    )]);
    let broker = Broker::with_transport("https://b.example/", "k1", &fake);
    let out = broker
        .notify_cleared_blocker("2AMLogic/loom", 30, 29)
        .unwrap();
    assert!(!out.unblocked && out.comment.is_none());
    let (url, _, _) = &fake.calls()[0];
    assert_eq!(url, "https://b.example/v1/notify_cleared_blocker");
}

#[test]
fn refusals_transport_and_unreadable_answers_map_to_distinct_errors() {
    let fake = Fake::new(vec![
        Err("dns: no such host".to_owned()),
        Ok((403, r#"{"error":"label_refused","message":"carries loom:operator"}"#.to_owned())),
        Ok((502, "not json".to_owned())),
        ok("also not the documented shape"),
    ]);
    let broker = Broker::with_transport("http://x", "k", &fake);
    let e = broker.complete_curation("r", 1, 2).unwrap_err();
    assert!(matches!(e, BrokerError::Transport(_)), "{e:?}");
    let e = broker.complete_curation("r", 1, 2).unwrap_err();
    assert_eq!(
        e,
        BrokerError::Refused {
            status: 403,
            code: "label_refused".to_owned(),
            message: "carries loom:operator".to_owned(),
        }
    );
    assert!(e.to_string().contains("403 label_refused"));
    let e = broker.complete_curation("r", 1, 2).unwrap_err();
    assert!(matches!(e, BrokerError::Refused { status: 502, .. }), "{e:?}");
    let e = broker.complete_curation("r", 1, 2).unwrap_err();
    assert!(matches!(e, BrokerError::Unreadable(_)), "{e:?}");
}

#[test]
fn key_resolution_env_wins_then_file_then_actionable_error() {
    assert_eq!(resolve_key(Some("  env-key  "), None).unwrap(), "env-key");
    assert_eq!(
        resolve_key(Some("has space"), None).unwrap_err(),
        format!("{KEY_ENV} contains whitespace or control characters")
    );

    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("k");
    std::fs::write(&file, "file-key\nsecond-line\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(resolve_key(None, Some(file.to_str().unwrap())).unwrap(), "file-key");
    assert_eq!(resolve_key(Some(""), Some(file.to_str().unwrap())).unwrap(), "file-key");

    let loose = dir.path().join("loose");
    std::fs::write(&loose, "k\n").unwrap();
    std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
    let e = resolve_key(None, Some(loose.to_str().unwrap())).unwrap_err();
    assert!(e.contains("0600"), "{e}");

    let e = resolve_key(None, Some(dir.path().join("missing").to_str().unwrap())).unwrap_err();
    assert!(e.contains(KEY_ENV) && e.contains("fleetroleskey"), "{e}");
}
