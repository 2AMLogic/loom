//! Builder gate contracts against a Gitea origin with an explicit port and no working gh.
#![cfg(unix)]
#[allow(dead_code)]
#[path = "support/gitea_stub.rs"]
mod gitea_stub;

use gitea_stub::{checkout, daemon, failing_gh, issue, pull, Reply, Stub};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;

const REPO: &str = "/api/v1/repos/qual/consumer";
const CLAIM: &[&str] = &["forge", "check-claim", "12"];
const SPLIT: &[&str] = &["forge", "parent", "split-state", "--parent", "12"];

struct Fixture {
    dir: tempfile::TempDir,
    stub: Stub,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let stub = Stub::start();
        checkout(dir.path(), &stub, "qual/consumer");
        failing_gh(dir.path());
        // Only the branch probe is scripted. Origin discovery uses the real git.
        let git = dir.path().join("git");
        std::fs::write(&git, "#!/bin/sh\nif [ \"$1\" = ls-remote ]; then cat branch; else exec \"$REAL_GIT\" \"$@\"; fi\n").unwrap();
        std::fs::set_permissions(git, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("branch"), "").unwrap();
        Self { dir, stub }
    }

    fn route(&self, path: &str, body: Value) {
        self.stub
            .route(&format!("{REPO}{path}"), Reply::json("200 OK", body));
    }

    fn defaults(&self) {
        self.route("/pulls?state=all&limit=50&page=1", json!([]));
        self.route("/issues/12", issue(12, "open", &[]));
        self.route("/issues/12/comments", json!([]));
    }

    fn expect(&self, args: &[&str], owned: bool, code: i32, stdout: &str) {
        let dir = self.dir.path();
        let mut cmd = daemon(dir, &self.stub, "synthetic-token");
        let path = std::env::var_os("PATH").unwrap();
        let real_git = std::env::split_paths(&path)
            .map(|p| p.join("git"))
            .find(|p| p.is_file())
            .unwrap();
        let paths = std::iter::once(dir.to_owned()).chain(std::env::split_paths(&path));
        cmd.env("PATH", std::env::join_paths(paths).unwrap())
            .env("REAL_GIT", real_git)
            .env("LOOM_GH_BIN", dir.join("gh"))
            .env_remove("LOOM_SWEEP_ID")
            .env_remove("LOOM_SWEEP_CLAIM_OWNED");
        if owned {
            cmd.env("LOOM_SWEEP_CLAIM_OWNED", "12");
        }
        let out = cmd.args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(code), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8(out.stdout).unwrap(), stdout);
        assert!(!dir.join("gh-calls").exists());
        assert!(self
            .stub
            .requests()
            .iter()
            .all(|r| r.authorization.as_deref() == Some("token synthetic-token")));
    }
}

#[test]
fn clean_claim_and_remote_branch() {
    let f = Fixture::new();
    f.defaults();
    f.expect(CLAIM, false, 1, "");
    std::fs::write(f.dir.path().join("branch"), "abc refs/heads/feature/issue-12\n").unwrap();
    f.expect(CLAIM, false, 0, "BRANCH_EXISTS feature/issue-12\n");
    f.expect(CLAIM, true, 1, "");
}

#[test]
fn split_reads_marker_children_in_both_states_and_all_pages() {
    for (last, code, stdout) in [
        (json!([]), 0, "SPLIT_ALL_CLOSED #14\n"),
        (
            json!([{"number":15,"state":"open","body":"<!-- loom:parent #12 -->"}]),
            1,
            "NOT_SPLIT\n",
        ),
        (json!({"message":"incomplete"}), 2, ""),
    ] {
        let f = Fixture::new();
        f.route("/issues/12", issue(12, "open", &[]));
        f.route(
            "/issues?state=all&type=issues&limit=50&page=1",
            json!([
                {"number":13,"state":"open","body":"<!-- loom:parent #123 -->"},
                {"number":14,"state":"closed","body":"<!-- loom:parent #12 -->"}
            ]),
        );
        f.route("/issues?state=all&type=issues&limit=50&page=2", last);
        f.route("/issues?state=all&type=issues&limit=50&page=3", json!([]));
        f.expect(SPLIT, false, code, stdout);
    }
}

const FORCE: &[&str] = &["forge", "check-claim", "12", "--force-claim"];
const PULLS: &str = "/pulls?state=all&limit=50&page=";

#[test]
fn claim_refuses_target_kind_state_holds_and_claim_labels() {
    for (mut row, token) in [
        (issue(12, "open", &[]), "TARGET_IS_PR #12"),
        (issue(12, "closed", &[]), "CLOSED"),
        (issue(12, "open", &["loom:frozen"]), "FROZEN"),
        (issue(12, "open", &["loom:operator"]), "OPERATOR"),
        (issue(12, "open", &["loom:triage"]), "TRIAGE"),
        (issue(12, "open", &["loom:building"]), "BUILDING"),
    ] {
        let f = Fixture::new();
        if token.starts_with("TARGET_IS_PR") {
            row["pull_request"] = json!({});
        }
        f.route("/issues/12", row);
        f.defaults();
        f.expect(CLAIM, false, 0, &format!("{token}\n"));
        if token == "BUILDING" {
            f.expect(FORCE, false, 1, "");
        }
    }
}

#[test]
fn linked_open_pulls_and_mandatory_read_failures_on_a_later_page() {
    for (second, code, token) in [
        (Some("**Part of:** #12"), 0, "OPEN_PR #8\n"),
        (Some("Fixes #12"), 0, "OPEN_PR #8\n"),
        (Some("Closes #123"), 1, ""),
        (None, 5, ""),
    ] {
        let f = Fixture::new();
        f.route(&format!("{PULLS}1"), json!([pull(7, "open", &[], "abc")]));
        if let Some(body) = second {
            let mut later = pull(8, "open", &[], "abc");
            later["body"] = json!(body);
            f.route(&format!("{PULLS}2"), json!([later]));
            f.route(&format!("{PULLS}3"), json!([]));
        }
        f.defaults();
        f.expect(FORCE, false, code, token);
    }
}

#[test]
fn merged_issue_branch_only_blocks_until_repromotion() {
    for (events, code, token) in [
        (json!([]), 0, "MERGED_PR #8\n"),
        (
            json!([{"id":1,"type":"label","label":{"name":"loom:issue"},"body":"1","created_at":"2026-11-21T10:00:00Z"}]),
            1,
            "",
        ),
        (
            json!([{"id":1,"type":"label","label":{"name":"loom:issue"},"body":"","created_at":"2026-11-21T10:00:00Z"}]),
            0,
            "MERGED_PR #8\n",
        ),
        (
            json!([{"id":1,"type":"label","label":{"name":"loom:issue"},"body":"1","created_at":"bad"}]),
            5,
            "",
        ),
    ] {
        let f = Fixture::new();
        let mut merged = pull(8, "closed", &[], "abc");
        // After deletion Gitea keeps the source branch in label and substitutes a pull ref.
        merged["head"]["label"] = json!("feature/issue-12");
        merged["head"]["ref"] = json!("refs/pull/8/head");
        merged["merged"] = json!(true);
        merged["merged_at"] = json!("2026-11-20T10:00:00Z");
        f.route(&format!("{PULLS}1"), json!([merged]));
        f.route(&format!("{PULLS}2"), json!([]));
        let mut first = Reply::json("200 OK", events.clone());
        first.total_count = events.as_array().map(Vec::len);
        f.stub
            .route(&format!("{REPO}/issues/12/timeline?limit=50&page=1"), first);
        let mut last = Reply::json("200 OK", Value::Null);
        last.total_count = Some(0);
        f.stub
            .route(&format!("{REPO}/issues/12/timeline?limit=50&page=2"), last);
        f.defaults();
        f.expect(FORCE, false, code, token);
    }
}

#[test]
fn only_trusted_live_unyielded_leases_block_dispatcher_owned_claims() {
    let now = chrono::Utc::now();
    let marker = "<!-- loom:lease host=peer sweep=other -->";
    let comment = |body: &str, author: &str, at: chrono::DateTime<chrono::Utc>| json!({"body":body, "updated_at":at, "user":{"login":author}});
    let lease = comment(marker, "worker", now);
    let yielded =
        "<!-- loom:lease-yield host=peer sweep=other earliest_host=h earliest_sweep=s -->";
    for (rows, code, token) in [
        (json!([lease]), 0, "LEASE_ALREADY_HELD peer other\n"),
        (
            json!([lease, comment(yielded, "outsider", now)]),
            0,
            "LEASE_ALREADY_HELD peer other\n",
        ),
        (json!([lease, comment(yielded, "worker", now)]), 1, ""),
        (json!([comment(marker, "outsider", now)]), 1, ""),
        (json!([comment(marker, "worker", now - chrono::Duration::days(2))]), 1, ""),
    ] {
        let f = Fixture::new();
        f.route("/issues/12", issue(12, "open", &["loom:building"]));
        f.route("/issues/12/comments", rows);
        f.route("/assignees", json!([{"login":"worker"}]));
        f.defaults();
        f.expect(CLAIM, true, code, token);
    }
}

#[test]
fn denied_or_malformed_evidence_fails_closed() {
    for (path, body, args) in [
        ("/issues/12", json!({"number":12,"state":"open","labels":[{}]}), CLAIM),
        ("/issues/12/comments", json!({"message":"truncated"}), CLAIM),
        ("/pulls?state=all&limit=50&page=1", json!([{"number":7}]), FORCE),
    ] {
        let f = Fixture::new();
        f.route(path, body);
        f.defaults();
        f.expect(args, false, 5, "");
    }
}

#[test]
fn explicit_split_target_with_no_children() {
    let f = Fixture::new();
    f.stub
        .route("/repos/other/project/issues/12", Reply::json("200 OK", issue(12, "open", &[])));
    f.stub.route(
        "/repos/other/project/issues?state=all&type=issues&limit=50&page=1",
        Reply::json("200 OK", json!([])),
    );
    f.expect(
        &[
            "forge",
            "parent",
            "split-state",
            "--parent",
            "12",
            "--repo",
            "other/project",
        ],
        false,
        1,
        "NOT_SPLIT\n",
    );
}

#[test]
fn a_successful_empty_response_cannot_hide_advertised_rows() {
    for (path, args, code) in [
        ("/issues?state=all&type=issues&limit=50&page=1", SPLIT, 2),
        ("/pulls?state=all&limit=50&page=1", FORCE, 5),
        ("/issues/12/comments", CLAIM, 5),
    ] {
        let f = Fixture::new();
        let mut truncated = Reply::json("200 OK", json!([]));
        truncated.total_count = Some(1);
        f.stub.route(&format!("{REPO}{path}"), truncated);
        f.defaults();
        f.expect(args, false, code, "");
    }
}

#[test]
fn a_link_from_a_fork_requires_a_trusted_author() {
    for (writers, code, token) in [
        (json!([]), 1, ""),
        (json!([{"login":"alice"}]), 0, "OPEN_PR #8\n"),
    ] {
        let f = Fixture::new();
        let mut pr = pull(8, "open", &[], "abc");
        pr["head"]["repo"]["full_name"] = json!("outsider/fork");
        pr["body"] = json!("Contributes to #12");
        f.route(&format!("{PULLS}1"), json!([pr]));
        f.route(&format!("{PULLS}2"), json!([]));
        f.route("/assignees", writers);
        f.defaults();
        f.expect(CLAIM, false, code, token);
    }
}

#[test]
fn empty_gitea_timeline_is_null_with_an_explicit_total() {
    for (count, code, token) in [
        (Some(0), 0, "MERGED_PR #8\n"),
        (Some(1), 5, ""),
        (None, 5, ""),
    ] {
        let f = Fixture::new();
        let mut pr = pull(8, "closed", &[], "abc");
        pr["head"]["label"] = json!("feature/issue-12");
        pr["head"]["ref"] = json!("refs/pull/8/head");
        pr["merged"] = json!(true);
        pr["merged_at"] = json!("2026-11-20T10:00:00Z");
        f.route(&format!("{PULLS}1"), json!([pr]));
        f.route(&format!("{PULLS}2"), json!([]));
        let mut end = Reply::json("200 OK", Value::Null);
        end.total_count = count;
        f.stub
            .route(&format!("{REPO}/issues/12/timeline?limit=50&page=1"), end);
        f.defaults();
        f.expect(FORCE, false, code, token);
    }
}
