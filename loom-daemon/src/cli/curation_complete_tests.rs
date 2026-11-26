#![allow(clippy::unwrap_used)]
use super::*;
use std::os::unix::fs::PermissionsExt;

const ESTIMATE: &str = "<!-- loom:estimate lines=12 files=1 doctor_risk=0.1 -->";

/// A fake `gh` that logs every invocation and answers by argument shape:
/// label writes succeed, `…/comments` serves `comments.json`, anything else
/// serves `issue.json`.
struct Forge {
    dir: tempfile::TempDir,
}

impl Forge {
    fn new(issue: &serde_json::Value, comments: &serde_json::Value) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let gh = dir.path().join("fake-gh");
        std::fs::write(
            &gh,
            r#"#!/bin/sh
d=$(dirname "$0")
printf '%s\n' "$*" >> "$d/gh.log"
case "$*" in
  *"-X POST"*|*"-X DELETE"*) [ -e "$d/fail-writes" ] && exit 1; exit 0 ;;
  *"/comments"*) cat "$d/comments.json" ;;
  *) sleep 0.05; cat "$d/issue.json" ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.path().join("issue.json"), issue.to_string()).unwrap();
        std::fs::write(dir.path().join("comments.json"), comments.to_string()).unwrap();
        Self { dir }
    }

    fn args(&self, id: &str) -> CurationCompleteArgs {
        CurationCompleteArgs {
            repo: "fixture/repo".to_string(),
            issue: 42,
            model: "actual-curator-model".to_string(),
            runtime: "codex".to_string(),
            completion_id: id.to_string(),
            workspace_root: self.dir.path().to_path_buf(),
        }
    }

    fn run(&self, id: &str) -> Result<()> {
        let policy = TrustPolicy::default();
        let args = self.args(id);
        let gh = self.dir.path().join("fake-gh");
        args.run_with(gh.as_os_str(), &policy, Some(&|| Ok(())), &|issue, points| {
            args.project_labels(gh.as_os_str(), issue, points)
        })
    }

    /// Run a fresh completion and assert it was refused with nothing written.
    fn assert_refused(&self) -> String {
        let err = self.run(&uuid::Uuid::new_v4().to_string()).unwrap_err();
        assert!(self.label_writes().is_empty(), "{}", self.log());
        assert!(self.journal().is_empty());
        format!("{err:#}")
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("gh.log")).unwrap_or_default()
    }

    fn label_writes(&self) -> Vec<String> {
        self.log()
            .lines()
            .filter(|l| l.contains("-X POST") || l.contains("-X DELETE"))
            .map(str::to_string)
            .collect()
    }

    fn journal(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.dir.path().join(".loom/logs/issue-estimates"))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default()
    }
}

fn issue(body: &str, association: &str, labels: &[&str]) -> serde_json::Value {
    serde_json::json!({
        "body": body,
        "user": {"login": "filer", "type": "User"},
        "author_association": association,
        "labels": labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
    })
}

fn comment(body: &str, association: &str) -> serde_json::Value {
    serde_json::json!({
        "body": body,
        "user": {"login": "someone", "type": "User"},
        "author_association": association,
    })
}

/// repo#623: a tier marker posted in a trusted comment must not complete.
#[test]
fn repo_623_tier_marker_in_trusted_comment_is_refused() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &["loom:curating"]),
        &serde_json::json!([comment("<!-- loom:complexity=3 -->", "MEMBER")]),
    );
    let err = forge.run(&uuid::Uuid::new_v4().to_string()).unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("loom:complexity"), "{message}");
    assert!(message.contains("loom:points="), "{message}");
    assert!(forge.label_writes().is_empty(), "{}", forge.log());
    assert!(forge.journal().is_empty());
}

#[test]
fn refuses_missing_bare_malformed_out_of_vocabulary_conflicting_and_mixed_markers() {
    for (comments, expect) in [
        (vec![], "no points estimate marker"),
        (vec!["3"], "no points estimate marker"),
        (vec!["<!-- loom:points=x -->"], "malformed"),
        (vec!["<!-- loom:points=21 -->"], "'21'"),
        (vec!["<!-- loom:points=3 -->\n<!-- loom:points=5 -->"], "conflicting"),
        (vec!["<!-- loom:points=3 -->\n<!-- loom:complexity=routine -->"], "retired"),
    ] {
        let listing: Vec<_> = comments.iter().map(|c| comment(c, "MEMBER")).collect();
        let forge = Forge::new(&issue(ESTIMATE, "OWNER", &[]), &serde_json::json!(listing));
        let message = forge.assert_refused();
        assert!(message.contains(expect), "{comments:?}: {message}");
    }
}

#[test]
fn invalid_newest_marker_never_falls_back_to_an_older_valid_one() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &[]),
        &serde_json::json!([
            comment("<!-- loom:points=3 -->", "MEMBER"),
            comment("<!-- loom:points=21 -->", "OWNER"),
        ]),
    );
    forge.assert_refused();
}

#[test]
fn trusted_comment_marker_projects_labels_and_writes_no_telemetry() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &["loom:curating", "points:3"]),
        &serde_json::json!([comment("<!-- loom:points=5 -->", "MEMBER")]),
    );
    let id = uuid::Uuid::new_v4().to_string();
    forge.run(&id).unwrap();
    let writes = forge.label_writes();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(writes[0].contains("-X POST") && writes[0].contains("labels[]=points:5"));
    assert!(writes[0].contains("labels[]=loom:curated"));
    assert!(writes[1].contains("-X DELETE") && writes[1].ends_with("labels/points%3A3 --silent"));
    // The daemon emits `issue.estimate` (#12118); this command journals nothing.
    assert!(forge.journal().is_empty());
}

#[test]
fn untrusted_comment_marker_reads_as_absent() {
    for association in ["CONTRIBUTOR", "NONE"] {
        let forge = Forge::new(
            &issue(ESTIMATE, "OWNER", &[]),
            &serde_json::json!([comment("<!-- loom:points=5 -->", association)]),
        );
        assert!(forge.assert_refused().contains("no points estimate marker"));
    }
}

#[test]
fn comment_beats_body_and_body_counts_only_for_a_trusted_author() {
    let body = format!("{ESTIMATE}\n<!-- loom:points=2 -->");
    let forge = Forge::new(
        &issue(&body, "OWNER", &[]),
        &serde_json::json!([comment("<!-- loom:points=8 -->", "COLLABORATOR")]),
    );
    forge.run(&uuid::Uuid::new_v4().to_string()).unwrap();
    assert!(forge.label_writes()[0].contains("labels[]=points:8"));

    let forge = Forge::new(&issue(&body, "MEMBER", &[]), &serde_json::json!([]));
    forge.run(&uuid::Uuid::new_v4().to_string()).unwrap();
    assert!(forge.label_writes()[0].contains("labels[]=points:2"));

    let forge = Forge::new(&issue(&body, "NONE", &[]), &serde_json::json!([]));
    assert!(forge.assert_refused().contains("no points estimate marker"));
}

#[test]
fn retry_after_a_failed_label_write_reresolves_points_and_journals_nothing() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &["points:13"]),
        &serde_json::json!([comment("<!-- loom:points=3 -->", "MEMBER")]),
    );
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::write(forge.dir.path().join("fail-writes"), b"").unwrap();
    assert!(forge.run(&id).is_err());
    assert!(forge.journal().is_empty());
    std::fs::remove_file(forge.dir.path().join("fail-writes")).unwrap();
    forge.run(&id).unwrap();
    let writes = forge.label_writes();
    let last_post = writes.iter().rfind(|w| w.contains("-X POST")).unwrap();
    assert!(last_post.contains("labels[]=points:3"), "{writes:?}");
    assert!(writes
        .last()
        .unwrap()
        .ends_with("labels/points%3A13 --silent"));
    assert!(forge.journal().is_empty());
}

#[test]
fn unwritable_repo_is_refused_before_any_read_or_write() {
    let forge = Forge::new(
        &issue(ESTIMATE, "OWNER", &[]),
        &serde_json::json!([comment("<!-- loom:points=3 -->", "MEMBER")]),
    );
    let gh = forge.dir.path().join("fake-gh");
    let err = forge
        .args(&uuid::Uuid::new_v4().to_string())
        .run_with(
            gh.as_os_str(),
            &TrustPolicy::default(),
            Some(&|| Err("not managed".into())),
            &|_, _| Ok(()),
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("not managed"));
    assert!(forge.log().is_empty());
    assert!(forge.journal().is_empty());
}

#[test]
fn stale_label_names_are_a_single_path_segment() {
    let forge = Forge::new(
        &issue("<!-- loom:points=5 -->", "OWNER", &["points:old/label?x"]),
        &serde_json::json!([]),
    );
    forge.run("retry").unwrap();
    assert!(forge.label_writes()[1].ends_with("labels/points%3Aold%2Flabel%3Fx --silent"));
}

// ---- #14465: the fleet broker route ---------------------------------------

use crate::fleet_curation_broker::{Broker, Transport};

/// Records every broker request as the (repo, number, points) projection it
/// carried, and answers from a scripted result.
struct FakeBroker {
    calls: std::sync::Mutex<Vec<(String, u32, u32)>>,
    answer: Result<(u16, String), String>,
}

impl FakeBroker {
    fn ok() -> Self {
        Self {
            calls: Default::default(),
            answer: Ok((
                200,
                r#"{"number":42,"points":"5","previous_points":["points:2"],
                    "curated":true,"changed":true,"replayed":false}"#
                    .to_owned(),
            )),
        }
    }

    fn refusing(status: u16, body: &'static str) -> Self {
        Self {
            calls: Default::default(),
            answer: Ok((status, body.to_owned())),
        }
    }

    fn handle(&self) -> Broker<&Self> {
        Broker::with_transport("http://broker.test", "test-key", self)
    }
}

fn fake_gh(forge: &Forge) -> std::path::PathBuf {
    forge.dir.path().join("fake-gh")
}

impl Transport for &FakeBroker {
    fn post_json(&self, _url: &str, _key: &str, body: &str) -> Result<(u16, String), String> {
        let v: serde_json::Value = serde_json::from_str(body).unwrap();
        self.calls.lock().unwrap().push((
            v["repo"].as_str().unwrap_or_default().to_owned(),
            v["number"].as_u64().unwrap_or_default() as u32,
            v["points"]
                .as_str()
                .and_then(|p| p.parse().ok())
                .unwrap_or_default(),
        ));
        self.answer.clone()
    }
}

#[test]
fn fleet_route_projects_resolved_points_through_the_broker() {
    let forge = Forge::new(
        &issue("<!-- loom:points=5 -->", "OWNER", &["points:2"]),
        &serde_json::json!([]),
    );
    let broker = FakeBroker::ok();
    forge
        .args("fleet-1")
        .run_fleet_with(fake_gh(&forge).as_os_str(), &broker.handle())
        .unwrap();
    assert_eq!(broker.calls.lock().unwrap().clone(), vec![("fixture/repo".to_string(), 42, 5)]);
    // Reads only: no label mutation ever reached a gh credential.
    assert!(forge.label_writes().is_empty(), "{}", forge.log());
    assert!(forge.log().contains("issues/42"));
}

#[test]
fn fleet_route_refusal_fails_the_command_with_no_local_fallback() {
    let forge = Forge::new(&issue("<!-- loom:points=5 -->", "OWNER", &[]), &serde_json::json!([]));
    let broker =
        FakeBroker::refusing(403, r#"{"error":"label_refused","message":"carries loom:operator"}"#);
    let err = forge
        .args("fleet-2")
        .run_fleet_with(fake_gh(&forge).as_os_str(), &broker.handle())
        .unwrap_err();
    let text = format!("{err:#}");
    assert!(text.contains("label_refused") && text.contains("fleet"), "{text}");
    assert!(forge.label_writes().is_empty(), "{}", forge.log());
}

#[test]
fn fleet_route_refuses_invalid_points_before_contacting_the_broker() {
    let forge = Forge::new(
        &issue("<!-- loom:points=x -->", "OWNER", &["points:2"]),
        &serde_json::json!([]),
    );
    let broker = FakeBroker::ok();
    let err = forge
        .args("fleet-3")
        .run_fleet_with(fake_gh(&forge).as_os_str(), &broker.handle())
        .unwrap_err();
    assert!(format!("{err:#}").contains("malformed"));
    assert!(broker.calls.lock().unwrap().is_empty());
    assert!(forge.label_writes().is_empty(), "{}", forge.log());
}
