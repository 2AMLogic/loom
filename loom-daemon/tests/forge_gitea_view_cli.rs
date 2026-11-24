//! `loom-daemon forge <issue|pr> view <N> --json …` on a Gitea checkout
//! (#14939): the live gate read of the numbered Judge and Doctor paths.
//!
//! The checkout's origin is a private repository on a loopback Gitea on an
//! explicit port. A synthetic token is the only credential, so a read that
//! reaches the seeded fixtures proves the resolved connection and port were
//! used; the request log proves each read carried the token; and `gh` is a
//! stub that records its argv, so the exercised path is proven never to
//! reach the GitHub client.
#[path = "support/gitea_stub.rs"]
mod gitea_stub;

use gitea_stub::{checkout, daemon, failing_gh, issue, pull, Reply, Stub};
use serde_json::{json, Value};
use std::process::{Command, Output};

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const TOKEN: &str = "synthetic-token";
const GATE: &str = "number,state,labels,headRefOid,mergeable";

struct Fixture {
    dir: tempfile::TempDir,
    stub: Stub,
    gh_log: std::path::PathBuf,
}

/// A checkout of `qual/consumer` on a loopback Gitea, with a `gh` that
/// records and fails.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    checkout(dir.path(), &stub, "qual/consumer");
    let (_gh, gh_log) = failing_gh(dir.path());
    Fixture { dir, stub, gh_log }
}

impl Fixture {
    fn run(&self, args: &[&str]) -> Output {
        let mut cmd = daemon(self.dir.path(), &self.stub, TOKEN);
        cmd.args(args)
            .env("LOOM_GH_BIN", self.dir.path().join("gh"));
        cmd.output().expect("run loom-daemon")
    }

    fn view(&self, entity: &str, args: &[&str]) -> Output {
        let mut all = vec!["forge", entity, "view"];
        all.extend_from_slice(args);
        self.run(&all)
    }

    fn assert_gh_unused(&self) {
        assert!(
            !self.gh_log.exists(),
            "the Gitea read reached gh: {}",
            std::fs::read_to_string(&self.gh_log).unwrap_or_default()
        );
    }
}

fn json(out: &Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!("stdout is not JSON ({e}): {:?}", String::from_utf8_lossy(&out.stdout))
    })
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn refused(out: &Output) {
    assert_eq!(out.status.code(), Some(1), "{}", stderr(out));
    assert!(
        out.stdout.is_empty(),
        "produced output a role could act on: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[cfg(unix)]
#[test]
fn pr_view_serves_the_gate_read_from_the_private_repo_with_the_inherited_token() {
    let f = fixture();
    f.stub.route(
        "/pulls/7",
        Reply::json("200 OK", pull(7, "open", &["loom:review-requested", "loom:important"], HEAD)),
    );
    let out = f.view("pr", &["7", "--json", GATE]);
    assert!(out.status.success(), "{}", stderr(&out));
    let row = json(&out);
    assert_eq!(row["number"], 7);
    assert_eq!(row["state"], "OPEN");
    assert_eq!(
        row["labels"],
        json!([
            {"color": "0075ca", "description": "", "id": "1", "name": "loom:review-requested"},
            {"color": "0075ca", "description": "", "id": "1", "name": "loom:important"},
        ]),
        "the forge's own label order, as gh keeps it"
    );
    assert_eq!(row["headRefOid"], HEAD);
    assert_eq!(row["mergeable"], "MERGEABLE");

    let requests = f.stub.requests();
    assert_eq!(
        requests.iter().map(|r| r.target()).collect::<Vec<_>>(),
        ["/api/v1/repos/qual/consumer/pulls/7"],
        "one read of the origin's own repository, on the port the origin names"
    );
    assert_eq!(requests[0].authorization.as_deref(), Some("token synthetic-token"));
    f.assert_gh_unused();
}

#[cfg(unix)]
#[test]
fn view_projects_closed_merged_and_hold_targets_without_refusing_them() {
    let f = fixture();
    let mut merged = pull(8, "closed", &["loom:pr"], HEAD);
    merged["merged"] = Value::Bool(true);
    merged["merged_at"] = Value::String("2026-11-21T11:00:00Z".into());
    f.stub.route("/pulls/8", Reply::json("200 OK", merged));
    f.stub
        .route("/pulls/9", Reply::json("200 OK", pull(9, "closed", &[], HEAD)));
    f.stub.route(
        "/pulls/10",
        Reply::json("200 OK", pull(10, "open", &["loom:frozen", "loom:operator"], HEAD)),
    );

    let out = f.view("pr", &["8", "--json", GATE]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json(&out)["state"], "MERGED", "a merged PR is MERGED, as gh reports it");
    let out = f.view("pr", &["9", "--json", GATE]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json(&out)["state"], "CLOSED");
    let out = f.view("pr", &["10", "--json", GATE]);
    assert!(out.status.success(), "{}", stderr(&out));
    let names: Vec<_> = json(&out)["labels"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        names,
        ["loom:frozen", "loom:operator"],
        "the hold labels the role refuses on are named"
    );
    f.assert_gh_unused();
}

#[cfg(unix)]
#[test]
fn view_reports_unknown_mergeability_as_unknown() {
    let f = fixture();
    let mut null = pull(7, "open", &["loom:changes-requested"], HEAD);
    null["mergeable"] = Value::Null;
    f.stub.route("/pulls/7", Reply::json("200 OK", null));
    let mut conflicting = pull(8, "open", &[], HEAD);
    conflicting["mergeable"] = Value::Bool(false);
    f.stub.route("/pulls/8", Reply::json("200 OK", conflicting));
    let mut absent = pull(9, "open", &[], HEAD);
    absent.as_object_mut().unwrap().remove("mergeable");
    f.stub.route("/pulls/9", Reply::json("200 OK", absent));

    for (number, want) in [("7", "UNKNOWN"), ("8", "CONFLICTING"), ("9", "UNKNOWN")] {
        let out = f.view("pr", &[number, "--json", "mergeable"]);
        assert!(out.status.success(), "#{number}: {}", stderr(&out));
        assert_eq!(json(&out)["mergeable"], want, "#{number}");
    }
}

#[cfg(unix)]
#[test]
fn a_wrong_kind_or_unknown_target_is_refused_before_any_output() {
    let f = fixture();
    f.stub
        .route("/pulls/9", Reply::text("404 Not Found", r#"{"message":"Not Found"}"#));
    f.stub
        .route("/pulls/55", Reply::text("404 Not Found", r#"{"message":"Not Found"}"#));
    f.stub
        .route("/issues/55", Reply::text("404 Not Found", r#"{"message":"Not Found"}"#));
    f.stub
        .route("/issues/9", Reply::json("200 OK", issue(9, "open", &["loom:issue"])));
    f.stub.route(
        "/issues/7",
        Reply::json(
            "200 OK",
            json!({"number": 7, "state": "open",
                "labels": [{"id": 1, "name": "loom:review-requested"}],
                "pull_request": {"merged": false, "merged_at": "0001-01-01T00:00:00Z"}}),
        ),
    );

    let out = f.view("pr", &["9", "--json", GATE]);
    refused(&out);
    assert!(
        stderr(&out).contains("issue"),
        "an issue number is named as one: {}",
        stderr(&out)
    );

    let out = f.view("issue", &["7", "--json", "number,state"]);
    refused(&out);
    assert!(
        stderr(&out).contains("pull request"),
        "a PR number is named as one: {}",
        stderr(&out)
    );

    let out = f.view("issue", &["9", "--json", "number,state,labels"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(json(&out)["state"], "OPEN");
    assert_eq!(json(&out)["labels"][0]["name"], "loom:issue");

    let out = f.view("pr", &["55", "--json", GATE]);
    refused(&out);
    f.assert_gh_unused();
}

#[cfg(unix)]
#[test]
fn a_missing_credential_fails_before_any_request() {
    let f = fixture();
    f.stub
        .route("/pulls/7", Reply::json("200 OK", pull(7, "open", &[], HEAD)));
    let mut cmd = daemon(f.dir.path(), &f.stub, TOKEN);
    cmd.env_remove("GITEA_TOKEN")
        .args(["forge", "pr", "view", "7", "--json", GATE])
        .env("LOOM_GH_BIN", f.dir.path().join("gh"));
    let out = cmd.output().unwrap();
    refused(&out);
    assert!(
        stderr(&out).contains("GITEA_TOKEN"),
        "names the credential to set: {}",
        stderr(&out)
    );
    assert!(
        f.stub.requests().is_empty(),
        "no credential, no request: {:?}",
        f.stub.requests()
    );
    f.assert_gh_unused();
}

#[cfg(unix)]
#[test]
fn a_refused_or_unreadable_answer_fails_instead_of_answering() {
    let f = fixture();
    f.stub.route(
        "/pulls/7",
        Reply::text("401 Unauthorized", r#"{"message":"token is required"}"#),
    );
    f.stub
        .route("/pulls/8", Reply::text("200 OK", "<html>not json</html>"));
    let mut headless = pull(9, "open", &[], HEAD);
    headless.as_object_mut().unwrap().remove("head");
    f.stub.route("/pulls/9", Reply::json("200 OK", headless));
    f.stub
        .route("/pulls/10", Reply::json("200 OK", json!({"message": "wrong shape"})));
    f.stub
        .route("/pulls/11", Reply::json("200 OK", json!({"number": 12, "state": "open"})));

    for number in ["7", "8", "9", "10", "11"] {
        refused(&f.view("pr", &[number, "--json", GATE]));
    }
    let out = f.view("pr", &["7", "--json", GATE]);
    assert!(stderr(&out).contains("401"), "the refusal names the status: {}", stderr(&out));
    f.assert_gh_unused();
}

#[cfg(unix)]
#[test]
fn view_reads_the_connection_from_the_checkout_config_and_keeps_the_port() {
    let f = fixture();
    f.stub
        .route("/pulls/7", Reply::json("200 OK", pull(7, "open", &[], HEAD)));
    std::fs::write(
        f.dir.path().join(".loom/config.json"),
        format!(
            r#"{{"forge":{{"gitea":{{"url": "{}", "token": "config-token"}}}}}}"#,
            f.stub.base_url
        ),
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .current_dir(f.dir.path())
        .env_remove("LOOM_FORGE_TYPE")
        .env_remove("GITEA_URL")
        .env_remove("GITEA_TOKEN")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_GH_BIN", f.dir.path().join("gh"))
        .args(["forge", "pr", "view", "7", "--json", "number,headRefOid"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let row = json(&out);
    assert_eq!(row["number"], 7);
    assert_eq!(row["headRefOid"], HEAD);
    let requests = f.stub.requests();
    assert_eq!(requests.len(), 1, "the configured host, its port kept, is the Gitea host");
    assert_eq!(requests[0].authorization.as_deref(), Some("token config-token"));
    f.assert_gh_unused();
}

#[cfg(unix)]
#[test]
fn view_applies_jq_honours_repo_and_declines_shapes_it_cannot_serve() {
    let f = fixture();
    f.stub.route(
        "/pulls/7",
        Reply::json("200 OK", pull(7, "open", &["loom:review-requested"], HEAD)),
    );
    let out = f.view("pr", &["7", "--json", "labels,state", "--jq", ".labels[].name"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "loom:review-requested\n");
    let out = f.view(
        "pr",
        &[
            "7",
            "--json",
            "state,labels",
            "--jq",
            "[.state, .labels[].name] | @tsv",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "OPEN\tloom:review-requested\n");
    let out = f.view(
        "pr",
        &[
            "7",
            "--json",
            "state",
            "--repo",
            "qual/other",
            "--jq",
            ".state",
        ],
    );
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        f.stub
            .requests()
            .iter()
            .any(|r| r.target().ends_with("/repos/qual/other/pulls/7")),
        "--repo names the repository: {:?}",
        f.stub.requests()
    );

    for args in [
        vec!["7"],
        vec!["7", "--json", "author,state"],
        vec!["7", "--json", "mergeStateStatus"],
        vec!["7", "--json", "state", "--comments"],
        vec![
            "https://gitea.local/qual/consumer/pull/7",
            "--json",
            "state",
        ],
    ] {
        let out = f.view("pr", &args);
        assert_eq!(out.status.code(), Some(3), "{args:?} declines to the caller's shell path");
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    f.assert_gh_unused();
}

#[cfg(unix)]
#[test]
fn a_github_checkout_keeps_the_gh_passthrough() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Stub::start();
    checkout(dir.path(), &stub, "qual/consumer");
    let (gh, log) = failing_gh(dir.path());
    let out = Command::new(env!("CARGO_BIN_EXE_loom-daemon"))
        .current_dir(dir.path())
        .env("LOOM_FORGE_TYPE", "github")
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_GH_BIN", &gh)
        .env_remove("GITEA_URL")
        .env_remove("GITEA_TOKEN")
        .args(["forge", "pr", "view", "7", "--json", "state"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(97), "GitHub keeps the passthrough and gh's exit status");
    assert_eq!(
        std::fs::read_to_string(&log).unwrap(),
        "pr view 7 --json state\n",
        "the argv reaches gh unchanged"
    );
    assert!(stub.requests().is_empty(), "no Gitea read on a GitHub checkout");
}
