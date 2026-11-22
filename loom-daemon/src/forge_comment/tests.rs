//! `forge_comment` tests: footer format (the contract the shell twin pins
//! to, #9774), idempotence, env override, reference parsing, and the POST
//! wire shape against a stub `gh`.

use super::*;

#[test]
fn dashboard_url_issues_vs_pull() {
    let _env = DefaultDashboardEnv::hold();
    assert_eq!(
        forge_dashboard_url("o/r", 42, false),
        format!("{DEFAULT_DASHBOARD_BASE_URL}/github.com/o/r/issues/42")
    );
    assert_eq!(
        forge_dashboard_url("o/r", 42, true),
        format!("{DEFAULT_DASHBOARD_BASE_URL}/github.com/o/r/pull/42")
    );
}

#[test]
fn footer_is_the_pinned_format() {
    // Byte-exact: the shell twin (#9774) asserts the same string. Change both
    // or neither.
    assert_eq!(
        build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, "body text"),
        "body text\n\n[loom dashboard](https://dashboard.2amlogic.com/github.com/o/r/issues/9)\n<!-- loom:dashboard-link -->\n"
    );
}

#[test]
fn footer_is_idempotent_on_its_marker() {
    let once = build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, "body");
    assert_eq!(
        build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, &once),
        once,
        "a body already carrying the marker must not double-append"
    );
}

#[test]
fn footer_or_body_leaves_unresolvable_slugs_untouched() {
    let _env = DefaultDashboardEnv::hold();
    assert_eq!(footer_or_body(None, 9, false, "body"), "body");
    assert_eq!(
        footer_or_body(Some("o/r"), 9, false, "body"),
        build_dashboard_footer(DEFAULT_DASHBOARD_BASE_URL, "o/r", 9, false, "body")
    );
}

#[test]
fn env_override_changes_the_base_and_trims_trailing_slashes() {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let previous = std::env::var(ENV_DASHBOARD_BASE_URL).ok();
    std::env::set_var(ENV_DASHBOARD_BASE_URL, "https://d.example.com///");
    assert_eq!(dashboard_base_url(), "https://d.example.com");
    assert_eq!(
        forge_dashboard_url("o/r", 1, false),
        "https://d.example.com/github.com/o/r/issues/1"
    );
    match previous {
        Some(value) => std::env::set_var(ENV_DASHBOARD_BASE_URL, value),
        None => std::env::remove_var(ENV_DASHBOARD_BASE_URL),
    }
    drop(guard);
}

#[test]
fn blank_env_falls_back_to_the_default() {
    let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let previous = std::env::var(ENV_DASHBOARD_BASE_URL).ok();
    std::env::set_var(ENV_DASHBOARD_BASE_URL, "   ");
    assert_eq!(dashboard_base_url(), DEFAULT_DASHBOARD_BASE_URL);
    match previous {
        Some(value) => std::env::set_var(ENV_DASHBOARD_BASE_URL, value),
        None => std::env::remove_var(ENV_DASHBOARD_BASE_URL),
    }
    drop(guard);
}

#[test]
fn parse_issue_ref_accepts_the_stored_shapes() {
    assert_eq!(parse_issue_ref("9772"), Some((None, 9772)));
    assert_eq!(parse_issue_ref(" 9772 "), Some((None, 9772)));
    assert_eq!(parse_issue_ref("o/r#9772"), Some((Some("o/r".to_string()), 9772)));
    assert_eq!(parse_issue_ref("o/r/issues/9772"), Some((Some("o/r".to_string()), 9772)));
    assert_eq!(
        parse_issue_ref("https://github.com/o/r/issues/9772"),
        Some((Some("o/r".to_string()), 9772))
    );
    assert_eq!(
        parse_issue_ref("http://www.github.com/o/r/pull/9"),
        Some((Some("o/r".to_string()), 9))
    );
    assert_eq!(parse_issue_ref(""), None);
    assert_eq!(parse_issue_ref("o/r"), None);
    assert_eq!(parse_issue_ref("o/r/issues/notanumber"), None);
    assert_eq!(parse_issue_ref("https://github.com/o/r/pull/"), None);
}

#[test]
fn post_comment_sends_the_footer_through_the_rest_endpoint() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh();
    let result = post_comment(stub.path().join("gh"), None, "o/r", 42, false, "the body")
        .expect("stub gh succeeds");
    assert_eq!(result, "ok\n");

    let recorded =
        std::fs::read_to_string(stub.path().join("gh.args")).expect("stub recorded its args");
    assert!(
        recorded.contains("repos/o/r/issues/42/comments"),
        "REST endpoint, one number sequence: {recorded}"
    );
    assert!(
        recorded.contains("--input") && !recorded.contains(" -f "),
        "JSON body file, never -f: {recorded}"
    );
    let payload = std::fs::read_to_string(stub.path().join("gh.stdin"))
        .expect("stub recorded the --input body");
    let parsed: serde_json::Value = serde_json::from_str(&payload).expect("valid JSON payload");
    let body = parsed["body"].as_str().expect("body field");
    assert!(
        body.starts_with("the body\n\n[loom dashboard]("),
        "footer appended to the caller's body: {body}"
    );
    assert!(
        body.contains("](https://dashboard.2amlogic.com/github.com/o/r/issues/42)\n"),
        "link points at the issues page: {body}"
    );
    assert!(
        body.ends_with(&format!("{FOOTER_MARKER}\n")),
        "marker terminates (with the format's trailing newline): {body}"
    );
}

#[test]
fn post_comment_surveys_stderr_on_failure() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_failing();
    let error =
        post_comment(stub.path().join("gh"), None, "o/r", 42, false, "b").expect_err("stub fails");
    assert!(error.contains("API rate limit exceeded"), "{error}");
}

// ---------------------------------------------------------------------------
// #10025: GraphQL `addComment` fallback on a REST rate limit.
// ---------------------------------------------------------------------------

/// A stub `gh` whose REST POST fails with `rest_stderr` and whose `api
/// graphql` calls answer the node-id lookup and the mutation — or fail too,
/// when `graphql_ok` is false. Every call's `--input <file>` body lands in
/// `gh.stdin.<n>` (n = 1-based call index) so the bodies can be compared byte
/// for byte.
fn temp_stub_gh_rest_limited(rest_stderr: &str, graphql_ok: bool) -> StubGh {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let graphql = if graphql_ok {
        r#"case "$(cat "$in")" in
      *addComment*) echo '{"data":{"addComment":{"commentEdge":{"node":{"id":"IC_new","url":"https://github.com/o/r/issues/42#issuecomment-7"}}}}}' ;;
      *) echo '{"data":{"repository":{"issueOrPullRequest":{"id":"I_node42"}}}}' ;;
    esac"#
            .to_string()
    } else {
        "echo 'GraphQL: Something went wrong' >&2; exit 1".to_string()
    };
    let script = format!(
        "#!/bin/sh
echo \"$@\" >> {d}/gh.args
n=$(wc -l < {d}/gh.args | tr -d ' ')
in={d}/gh.stdin.$n
prev=
for a in \"$@\"; do [ \"$prev\" = --input ] && cat \"$a\" > \"$in\"; prev=\"$a\"; done
if [ \"$2\" = graphql ]; then
    {graphql}
    exit 0
fi
echo '{rest_stderr}' >&2
exit 1
"
    );
    let path = dir.path().join("gh");
    std::fs::write(&path, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    StubGh(dir)
}

fn recorded_json(stub: &StubGh, call: usize) -> serde_json::Value {
    let raw = std::fs::read_to_string(stub.path().join(format!("gh.stdin.{call}")))
        .unwrap_or_else(|e| panic!("stub recorded call {call}'s --input body: {e}"));
    serde_json::from_str(&raw).expect("each request body is JSON")
}

#[test]
fn rest_rate_limit_falls_back_to_graphql_add_comment_byte_identically() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_rest_limited("HTTP 403: API rate limit exceeded for user ID 1.", true);
    let body = "multi-line\n\n- with `markdown` and \"quotes\"\n";
    let response = post_comment(stub.path().join("gh"), None, "o/r", 42, false, body)
        .expect("GraphQL fallback answers");

    let args = std::fs::read_to_string(stub.path().join("gh.args")).unwrap();
    let calls: Vec<&str> = args.lines().collect();
    assert_eq!(calls.len(), 3, "REST POST, node-id lookup, addComment: {args}");
    assert!(calls[0].contains("repos/o/r/issues/42/comments"), "{args}");
    assert!(calls[1].starts_with("api graphql --input "), "{args}");
    assert!(calls[2].starts_with("api graphql --input "), "{args}");

    let lookup = recorded_json(&stub, 2);
    assert_eq!(lookup["variables"]["owner"], "o");
    assert_eq!(lookup["variables"]["name"], "r");
    assert_eq!(lookup["variables"]["number"], 42);

    // Byte-identity: the GraphQL body IS the footered body REST was handed.
    let rest_body = recorded_json(&stub, 1)["body"]
        .as_str()
        .unwrap()
        .to_string();
    let mutation = recorded_json(&stub, 3);
    assert_eq!(mutation["variables"]["subjectId"], "I_node42");
    assert_eq!(mutation["variables"]["body"].as_str().unwrap(), rest_body);
    assert_eq!(rest_body, append_dashboard_footer("o/r", 42, false, body));
    assert!(rest_body.ends_with(&format!("{FOOTER_MARKER}\n")));

    let parsed: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(parsed["html_url"], "https://github.com/o/r/issues/42#issuecomment-7");
}

#[test]
fn graphql_fallback_failure_returns_the_original_rest_error() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_rest_limited("HTTP 403: API rate limit exceeded for user ID 1.", false);
    let error = post_comment(stub.path().join("gh"), None, "o/r", 42, false, "b")
        .expect_err("both transports fail");
    assert!(error.starts_with("gh api (comment on o/r#42) failed:"), "{error}");
    assert!(error.contains("API rate limit exceeded"), "{error}");
    assert!(!error.contains("GraphQL"), "the REST error, not the fallback's: {error}");
}

#[test]
fn non_rate_limit_rest_failure_does_not_try_graphql() {
    let _env = DefaultDashboardEnv::hold();
    let stub = temp_stub_gh_rest_limited("HTTP 404: Not Found", true);
    let error = post_comment(stub.path().join("gh"), None, "o/r", 42, false, "b")
        .expect_err("a 404 is not retried");
    assert!(error.contains("HTTP 404"), "{error}");
    let args = std::fs::read_to_string(stub.path().join("gh.args")).unwrap();
    assert_eq!(args.lines().count(), 1, "no GraphQL call for a non-rate-limit failure: {args}");
}

// ---------------------------------------------------------------------------
// Stub `gh` fixtures (the `merge_pr/redate` pattern: a shell script standing
// in for the binary, recording what it was handed).
// ---------------------------------------------------------------------------

struct StubGh(tempfile::TempDir);

impl StubGh {
    fn path(&self) -> &Path {
        self.0.path()
    }
}

fn write_stub(dir: &Path, exit_code: u8) {
    let args_path = dir.join("gh.args");
    let stdin_path = dir.join("gh.stdin");
    let script = dir.join("gh");
    let record = format!(
        "echo \"$@\" >> {args}\nprev=\nfor a in \"$@\"; do [ \"$prev\" = --input ] && cat \"$a\" > {stdin}; prev=\"$a\"; done\n",
        args = args_path.display(),
        stdin = stdin_path.display(),
    );
    let body = if exit_code == 0 {
        format!("#!/bin/sh\n{record}echo ok\n")
    } else {
        format!("#!/bin/sh\n{record}echo 'API rate limit exceeded' >&2\nexit {exit_code}\n")
    };
    std::fs::write(&script, body).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn temp_stub_gh() -> StubGh {
    let dir = tempfile::tempdir().unwrap();
    write_stub(dir.path(), 0);
    StubGh(dir)
}

fn temp_stub_gh_failing() -> StubGh {
    let dir = tempfile::tempdir().unwrap();
    write_stub(dir.path(), 1);
    StubGh(dir)
}

/// Serializes the env-mutating tests — `cargo test` runs in parallel threads
/// in one process, and a global env var would race (the same reasoning
/// `merge_pr/redate` documents for parameterizing on the binary).
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds `ENV_LOCK` and pins `ENV_DASHBOARD_BASE_URL` unset for a test whose
/// result depends on the footer URL (every `post_comment` call reads it), so a
/// parallel env-override test cannot change the base mid-test. Restores the
/// previous value on drop, before releasing the lock.
struct DefaultDashboardEnv {
    previous: Option<String>,
    _guard: std::sync::MutexGuard<'static, ()>,
}

impl DefaultDashboardEnv {
    fn hold() -> Self {
        let guard = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var(ENV_DASHBOARD_BASE_URL).ok();
        std::env::remove_var(ENV_DASHBOARD_BASE_URL);
        Self {
            previous,
            _guard: guard,
        }
    }
}

impl Drop for DefaultDashboardEnv {
    fn drop(&mut self) {
        if let Some(value) = self.previous.take() {
            std::env::set_var(ENV_DASHBOARD_BASE_URL, value);
        }
    }
}

// ── #14286: `--body @path` must not post the literal string ──────────────

#[test]
fn lone_at_token_bodies_are_refused_before_posting() {
    for body in ["@path", "@-", "@/tmp/x", "  @path\n"] {
        let err = reject_lone_at_body(body).expect_err(body).to_string();
        assert!(err.contains("--body-file"), "{body:?}: {err}");
    }
}

#[test]
fn cli_entrypoint_refuses_a_lone_at_body_without_posting() {
    // The refusal fires before repo resolution and the POST, so this needs no
    // `gh` and no network: an `Err` naming `--body-file` proves it stopped early.
    let err = cli_entrypoint(CommentArgs {
        number: Some(1),
        repo: Some("o/r".to_string()),
        body: Some("@path".to_string()),
        body_file: None,
        is_pr: false,
        patch_created: None,
        authorized: None,
    })
    .expect_err("a lone @-token body must be refused")
    .to_string();
    assert!(err.contains("refusing to post") && err.contains("--body-file"), "{err}");
}

#[test]
fn short_and_mention_bodies_still_pass() {
    for body in [
        "LGTM",
        "ok",
        "-",
        "",
        "@alice thanks for the fix",
        "see @path here",
    ] {
        assert!(reject_lone_at_body(body).is_ok(), "{body:?} must not be refused");
    }
}

// ── #14940: the Gitea transport ───────────────────────────────────────────
//
// The loopback fixture mirrors `write_scope/gitea_tests.rs`'s stub: a
// one-request TCP listener that answers `status` + `body` and hands back the
// raw request bytes, so the tests assert the exact POST path, the
// stdin-ridden `Authorization` header, and the decoded JSON body.

/// A temp checkout configured as Gitea (`forge.type` + `forge.gitea.{url,
/// token}` in `.loom/config.json`), the shape a Gitea consumer install has.
/// Callers must hold the `loom_config_env` serial group (`isolate_gitea_env`
/// below) so env overrides cannot beat the config.
fn gitea_root(url: &str, token: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let loom = dir.path().join(".loom");
    std::fs::create_dir_all(&loom).unwrap();
    let gitea = match token {
        Some(t) => serde_json::json!({ "url": url, "token": t }),
        None => serde_json::json!({ "url": url }),
    };
    std::fs::write(
        loom.join("config.json"),
        serde_json::json!({ "forge": { "type": "gitea", "gitea": gitea } }).to_string(),
    )
    .unwrap();
    dir
}

/// Clear the Gitea env (the way `write_scope`'s gitea tests do) and pin the
/// config resolver to no defaults file, so a host credential or fleet default
/// can neither answer for the stub nor override the fixture's config.
fn isolate_gitea_env() {
    std::env::set_var("LOOM_CONFIG_DEFAULTS_FILE", "");
    std::env::remove_var("LOOM_FORGE_TYPE");
    for k in ["GITEA_TOKEN", "FORGE_TOKEN", "GITEA_URL", "GITEA_USERNAME"] {
        std::env::remove_var(k);
    }
}

fn clear_gitea_env() {
    std::env::remove_var("LOOM_CONFIG_DEFAULTS_FILE");
    std::env::remove_var("LOOM_FORGE_TYPE");
    for k in ["GITEA_TOKEN", "FORGE_TOKEN", "GITEA_URL", "GITEA_USERNAME"] {
        std::env::remove_var(k);
    }
}

/// One-request loopback Gitea stub: answers with `status` + `body` and hands
/// back the raw request bytes (headers + body), so a test can assert the
/// `Authorization` header arrived on curl's stdin, never its argv.
fn gitea_post_stub(
    status: &'static str,
    body: &'static str,
) -> (String, std::thread::JoinHandle<String>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = String::new();
        let mut buf = [0u8; 8192];
        // Read until the headers end and Content-Length body bytes arrived
        // (bounded: the fixture's bodies are small).
        while request.len() < 64 * 1024 {
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
                break;
            }
            let n = match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            request.push_str(&String::from_utf8_lossy(&buf[..n]));
        }
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        request
    });
    (format!("http://{addr}"), handle)
}

/// A `gh` stand-in that records every invocation to `<dir>/gh.log` and exits
/// non-zero: on the Gitea transport the log must stay absent (zero `gh`
/// calls), and had `gh` run it would fail loudly anyway.
fn recording_gh_stub() -> tempfile::TempDir {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    let log = dir.path().join("gh.log");
    std::fs::write(&script, format!("#!/bin/sh\necho \"$@\" >> {}\nexit 1\n", log.display()))
        .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    dir
}

/// The JSON body of a recorded HTTP request: the bytes after the blank line.
fn request_body(request: &str) -> &str {
    request.split_once("\r\n\r\n").map_or("", |(_, b)| b)
}

#[test]
#[serial_test::serial(loom_config_env)]
fn gitea_root_posts_through_the_gitea_api_without_gh() {
    isolate_gitea_env();
    let _env = DefaultDashboardEnv::hold();
    let stub = recording_gh_stub();
    let (base, request) = gitea_post_stub(
        "201 Created",
        r#"{"id":11,"html_url":"http://gitea.test/acme/w/issues/42#issuecomment-11"}"#,
    );
    let root = gitea_root(&base, Some("stub-token"));
    let body = "multi-line\n\n- with `markdown` and \"quotes\"\n<!-- loom:builder-note -->\n";
    let response =
        post_comment(stub.path().join("gh"), Some(root.path()), "acme/w", 42, false, body)
            .expect("the Gitea transport posts");

    // The created comment's JSON is returned verbatim, so `html_url`
    // printing works unchanged.
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&response).unwrap()["html_url"],
        "http://gitea.test/acme/w/issues/42#issuecomment-11"
    );

    let request = request.join().unwrap();
    assert!(
        request.starts_with("POST /api/v1/repos/acme/w/issues/42/comments HTTP/1.1"),
        "the exact Gitea comments endpoint: {request}"
    );
    assert!(
        request.contains("Authorization: token stub-token"),
        "the credential rides the request (curl's stdin, never argv): {request}"
    );
    assert!(
        request.contains("Content-Type: application/json"),
        "JSON, not form-urlencoded: {request}"
    );
    let posted: serde_json::Value =
        serde_json::from_str(request_body(&request)).expect("the posted body is JSON");
    assert_eq!(
        posted["body"].as_str().unwrap(),
        append_dashboard_footer("acme/w", 42, false, body),
        "multiline markdown and the role marker survive byte-for-byte, footer applied once"
    );
    assert!(!stub.path().join("gh.log").exists(), "the Gitea transport never invokes gh");
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn gitea_post_does_not_double_an_existing_footer() {
    isolate_gitea_env();
    let _env = DefaultDashboardEnv::hold();
    let stub = recording_gh_stub();
    let (base, request) = gitea_post_stub("201 Created", r#"{"id":12,"html_url":"u"}"#);
    let root = gitea_root(&base, Some("stub-token"));
    let once = append_dashboard_footer("acme/w", 42, true, "already footered");
    post_comment(stub.path().join("gh"), Some(root.path()), "acme/w", 42, true, &once)
        .expect("posts");
    let request = request.join().unwrap();
    let posted: serde_json::Value = serde_json::from_str(request_body(&request)).unwrap();
    assert_eq!(
        posted["body"].as_str().unwrap(),
        once,
        "idempotent on the marker, and --pr picks /pull/N in the link"
    );
    assert!(once.contains("/pull/42"));
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn a_gitea_root_without_a_credential_refuses_before_any_request() {
    isolate_gitea_env();
    let _env = DefaultDashboardEnv::hold();
    let stub = recording_gh_stub();
    // A URL that would answer if any request were made; none is, because the
    // connection cannot resolve without a token.
    let root = gitea_root("http://127.0.0.1:9", None);
    let error = post_comment(stub.path().join("gh"), Some(root.path()), "acme/w", 42, false, "b")
        .expect_err("no credential: refuse, never fall through to gh");
    assert!(error.contains("token is required"), "names the missing credential: {error}");
    assert!(!error.contains("stub"), "no credential value to leak: {error}");
    assert!(
        !stub.path().join("gh.log").exists(),
        "a refused Gitea post must not reach for gh either"
    );
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn a_rejected_gitea_post_reports_failure_without_leaking_the_credential() {
    isolate_gitea_env();
    let _env = DefaultDashboardEnv::hold();
    let stub = recording_gh_stub();
    // The error body quotes the token back; the transport must redact it.
    let (base, request) = gitea_post_stub(
        "403 Forbidden",
        r#"{"message":"token stub-token is not allowed to comment"}"#,
    );
    let root = gitea_root(&base, Some("stub-token"));
    let error = post_comment(stub.path().join("gh"), Some(root.path()), "acme/w", 42, false, "b")
        .expect_err("a 403 is a failure");
    assert!(
        error.contains("403") && error.contains("failed"),
        "names the rejected POST: {error}"
    );
    assert!(!error.contains("stub-token"), "credential redacted: {error}");
    assert!(error.contains("[redacted]"), "the leak site is visibly redacted: {error}");
    request.join().unwrap();
    clear_gitea_env();
}

#[test]
#[serial_test::serial(loom_config_env)]
fn the_backend_is_github_without_a_gitea_checkout() {
    isolate_gitea_env();
    assert!(matches!(comment_backend(None), Ok(CommentBackend::GitHub)));
    let plain = tempfile::tempdir().unwrap();
    assert!(
        matches!(comment_backend(Some(plain.path())), Ok(CommentBackend::GitHub)),
        "a checkout with no Gitea config stays on gh"
    );
    let base = gitea_root("http://127.0.0.1:9", Some("t"));
    assert!(matches!(comment_backend(Some(base.path())), Ok(CommentBackend::Gitea(_))));
    clear_gitea_env();
}
