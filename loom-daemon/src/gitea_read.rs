//! Native Gitea reads (#14939): the transport behind the role gates and the
//! PR queue on a Gitea checkout.
//!
//! On GitHub those reads ride `gh`. On Gitea `gh` cannot resolve the remote
//! (`none of the git remotes point to a known GitHub host`) and refuses a
//! `host:port` as a hostname, so a targeted Judge and Doctor refused with
//! `forge_unreachable` and `pr-queue` exited on an invalid `--hostname`
//! before either reached a decision (qualification #14850). A read from here
//! goes to the checkout's own Gitea API instead, on the connection the
//! write-scope probe already resolves ([`crate::write_scope::probe`]):
//! `GITEA_URL`, else `forge.gitea.url`, its port kept as written; the token
//! `GITEA_TOKEN`, else `forge.gitea.token`, else `FORGE_TOKEN`; and
//! `GITEA_USERNAME` for Basic auth.
//!
//! Nothing is cached: a gate read answers for the moment it is asked, and a
//! queue page is read again on every pass.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::forge_cmd::ForgeType;

/// How long a root's forge stands before it is detected again, the window
/// [`crate::forge_host::CACHE_TTL`] gives its host.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// Rows the planner's listing reads ask a page for: GitHub's `per_page`.
const PAGE: usize = crate::forge_pull_listing::PER_PAGE;

type Cache = Mutex<HashMap<PathBuf, (Instant, bool)>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

/// Whether the forge reads from `root` go to is Gitea. Detection spawns `git`
/// three times and resolves every config tier, and the queue gates ask per
/// listing page and per PR, so it is settled once per root per [`CACHE_TTL`].
pub(crate) fn on_gitea(root: &Path) -> bool {
    if let Some((at, gitea)) = cache().lock().ok().and_then(|c| c.get(root).copied()) {
        if at.elapsed() < CACHE_TTL {
            return gitea;
        }
    }
    let gitea = crate::forge_cmd::detect_forge(Some(root)) == ForgeType::Gitea;
    if let Ok(mut cache) = cache().lock() {
        cache.insert(root.to_path_buf(), (Instant::now(), gitea));
    }
    gitea
}

/// One request, injected so the page translation is testable offline: the
/// path asked, `(status, body)` back.
type Fetch<'a> = dyn FnMut(&str) -> Result<(u16, Value), String> + 'a;

/// `GET /api/v1/{path}` on the connection `root` resolves. `Err` names why
/// nothing was read — no credential configured, curl could not run, or a body
/// that is not JSON — and never carries the credential. A refused request is
/// `Ok` with its status, which every caller fails closed on.
pub(crate) fn read(root: &Path, path: &str) -> Result<(u16, Value), String> {
    let connection = crate::write_scope::probe::gitea_connection(root).map_err(|why| {
        format!(
            "{why}. A Gitea checkout reads with GITEA_URL and GITEA_TOKEN inherited \
             from the session"
        )
    })?;
    let (status, body) = crate::write_scope::probe::gitea_get(&connection, path)?;
    let value = serde_json::from_str(body.trim())
        .map_err(|e| format!("a {status} answer that is not JSON: {e}"))?;
    Ok((status, value))
}

/// One GitHub-shaped page of a Gitea listing, for the planner's page walks,
/// which ask for GitHub's [`PAGE`]-row pages. Gitea pages pull requests by
/// `limit` through its own URL
/// ([`crate::forge_pull_listing::build_gitea_pulls_url`]) and does not page
/// issue comments at all, so two of its pull pages make one page asked, and
/// any page of comments is a slice of the one unpaginated read. Any other
/// path is read as it stands.
///
/// Anything but a `200` carrying the expected shape is `Err`: a failed listing
/// is never a successful empty page.
pub(crate) fn read_page(root: &Path, path: &str) -> Result<Value, String> {
    read_page_with(path, &mut |path| read(root, path))
}

/// [`read_page`] with the request injected.
pub(crate) fn read_page_with(path: &str, fetch: &mut Fetch<'_>) -> Result<Value, String> {
    let Some((head, query)) = path.split_once('?') else {
        return answered(path, fetch);
    };
    let param = |key: &str| {
        query
            .split('&')
            .find_map(|kv| kv.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
    };
    let page = match param("page").and_then(|p| p.parse::<usize>().ok()) {
        Some(page)
            if param("per_page").and_then(|v| v.parse::<usize>().ok()) == Some(PAGE)
                && page > 0 =>
        {
            page
        }
        _ => return answered(path, fetch),
    };
    let gitea_page = crate::forge_etag_store::Shape::GiteaPulls.page_size();
    let pages = PAGE / gitea_page;
    if let Some(repo) = head
        .strip_prefix("repos/")
        .and_then(|h| h.strip_suffix("/pulls"))
    {
        let mut rows = Vec::new();
        for n in (page - 1) * pages + 1..=page * pages {
            let (status, value) =
                fetch(&crate::forge_pull_listing::build_gitea_pulls_url(Some(repo), n))?;
            let batch = listing(status, &value)?;
            let full = batch.len() >= gitea_page;
            rows.extend(batch);
            if !full {
                break;
            }
        }
        return Ok(Value::Array(rows));
    }
    if !head.ends_with("/comments") {
        return answered(path, fetch);
    }
    let (status, value) = fetch(head)?;
    let all = listing(status, &value)?;
    Ok(Value::Array(all.into_iter().skip((page - 1) * PAGE).take(PAGE).collect()))
}

fn answered(path: &str, fetch: &mut Fetch<'_>) -> Result<Value, String> {
    let (status, value) = fetch(path)?;
    if status == 200 {
        Ok(value)
    } else {
        Err(refusal(status, &value))
    }
}

fn listing(status: u16, value: &Value) -> Result<Vec<Value>, String> {
    if status != 200 {
        return Err(refusal(status, value));
    }
    value.as_array().cloned().ok_or_else(|| {
        format!("the Gitea API answered {status} with something that is not a listing: {value}")
    })
}

fn refusal(status: u16, value: &Value) -> String {
    match value["message"].as_str() {
        Some(message) if !message.is_empty() => {
            format!("the Gitea API answered {status}: {message}")
        }
        _ => format!("the Gitea API answered {status}"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    const PULLS: &str =
        "repos/o/r/pulls?state=open&sort=created&direction=desc&per_page=100&page=1";
    const GITEA_PAGE_1: &str = "repos/o/r/pulls?state=open&sort=recentupdate&limit=50&page=1";

    /// Scripted answers keyed by a suffix of the path asked, and the log of
    /// every path asked.
    struct Forged {
        routes: Vec<(&'static str, u16, Value)>,
        asked: Vec<String>,
    }

    impl Forged {
        fn new(routes: Vec<(&'static str, u16, Value)>) -> Self {
            Self {
                routes,
                asked: Vec::new(),
            }
        }

        fn fetch(&mut self, path: &str) -> Result<(u16, Value), String> {
            self.asked.push(path.to_string());
            Ok(self
                .routes
                .iter()
                .find(|(needle, _, _)| {
                    path.ends_with(needle) || path.contains(&format!("{needle}?"))
                })
                .map(|(_, status, value)| (*status, value.clone()))
                .unwrap_or((404, json!({"message": "Not Found"}))))
        }

        fn page(&mut self, path: &str) -> Result<Value, String> {
            read_page_with(path, &mut |p| self.fetch(p))
        }
    }

    fn rows(n: u64, from: u64) -> Value {
        Value::Array((from..from + n).map(|i| json!({"number": i})).collect())
    }

    fn numbers(page: &Value) -> Vec<u64> {
        page.as_array()
            .unwrap()
            .iter()
            .map(|r| r["number"].as_u64().unwrap())
            .collect()
    }

    #[test]
    fn a_page_asked_is_two_gitea_pages() {
        let mut forged = Forged::new(vec![
            ("&page=1", 200, rows(50, 0)),
            ("&page=2", 200, rows(50, 50)),
            ("&page=3", 200, rows(20, 100)),
            ("&page=4", 500, json!({"message": "must not be asked"})),
        ]);
        assert_eq!(numbers(&forged.page(PULLS).unwrap()), (0..100).collect::<Vec<_>>());
        let second = forged
            .page("repos/o/r/pulls?state=open&sort=created&direction=desc&per_page=100&page=2")
            .unwrap();
        assert_eq!(numbers(&second), (100..120).collect::<Vec<_>>());
        assert_eq!(
            forged.asked,
            [
                GITEA_PAGE_1,
                "repos/o/r/pulls?state=open&sort=recentupdate&limit=50&page=2",
                "repos/o/r/pulls?state=open&sort=recentupdate&limit=50&page=3",
            ],
            "a short page ends the walk: the page after it is never asked"
        );
    }

    #[test]
    fn a_short_first_page_is_the_whole_listing() {
        let mut forged = Forged::new(vec![
            ("&page=1", 200, rows(7, 0)),
            ("&page=2", 500, json!({"message": "must not be asked"})),
        ]);
        assert_eq!(numbers(&forged.page(PULLS).unwrap()), (0..7).collect::<Vec<_>>());
        assert_eq!(forged.asked, [GITEA_PAGE_1]);
    }

    #[test]
    fn comments_are_one_read_sliced_into_pages() {
        let mut forged = Forged::new(vec![("/comments", 200, rows(150, 0))]);
        for (page, len) in [(1, 100), (2, 50), (3, 0)] {
            let path = format!("repos/o/r/issues/7/comments?per_page=100&page={page}");
            assert_eq!(forged.page(&path).unwrap().as_array().unwrap().len(), len, "page {page}");
            assert_eq!(
                forged.asked.last().unwrap(),
                "repos/o/r/issues/7/comments",
                "Gitea returns every comment at once: no page, no per_page"
            );
        }
    }

    #[test]
    fn a_failed_listing_is_an_error_never_an_empty_page() {
        for (status, body) in [
            (500, json!({"message": "boom"})),
            (401, json!({"message": "token is required"})),
            (404, json!({"message": "Not Found"})),
            (200, json!({"message": "not a listing"})),
        ] {
            let mut forged = Forged::new(vec![("/pulls", status, body.clone())]);
            let err = forged.page(PULLS).unwrap_err();
            assert!(err.contains(&status.to_string()), "{status}: {err}");
            if status != 200 {
                assert!(err.contains(body["message"].as_str().unwrap()), "{err}");
            }
        }
    }

    #[test]
    fn other_paths_are_read_as_they_stand() {
        let mut forged = Forged::new(vec![
            ("/pulls/7", 200, json!({"number": 7})),
            ("/status", 200, json!({"state": "pending"})),
            ("/pulls", 200, rows(3, 0)),
        ]);
        for path in [
            "repos/o/r/pulls/7",
            "repos/o/r/commits/abc/status?per_page=100&page=1",
            "repos/o/r/pulls?state=open&per_page=30&page=1",
            "repos/o/r/pulls?state=open&per_page=100&page=0",
        ] {
            forged.page(path).unwrap();
            assert_eq!(forged.asked.last().unwrap(), path);
        }
    }

    /// One-request loopback stub, as `write_scope::gitea_tests` has it: the
    /// answer, and the raw request that reached it.
    fn stub(status: &'static str, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _) = listener.accept().unwrap();
            let mut buf = [0u8; 8192];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
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

    fn isolate_gitea_env() {
        std::env::set_var("LOOM_CONFIG_DEFAULTS_FILE", "");
        for k in ["GITEA_TOKEN", "FORGE_TOKEN", "GITEA_URL", "GITEA_USERNAME"] {
            std::env::remove_var(k);
        }
    }

    #[test]
    #[serial_test::serial(loom_config_env)]
    fn read_reaches_the_configured_port_with_the_token_and_reports_a_refusal() {
        isolate_gitea_env();
        let (base, request) = stub("401 Unauthorized", r#"{"message":"token is required"}"#);
        std::env::set_var("GITEA_URL", &base);
        std::env::set_var("GITEA_TOKEN", "synthetic-token");
        let dir = tempfile::tempdir().unwrap();
        let (status, value) = read(dir.path(), "repos/qual/consumer/pulls/7").unwrap();
        assert_eq!(status, 401, "a refusal is the caller's to fail closed on");
        assert_eq!(value["message"], "token is required");
        let request = request.join().unwrap();
        assert!(request.starts_with("GET /api/v1/repos/qual/consumer/pulls/7 "), "{request}");
        let port = base.rsplit(':').next().unwrap();
        assert!(request.contains(&format!("Host: 127.0.0.1:{port}")), "{request}");
        assert!(request.contains("Authorization: token synthetic-token"), "{request}");
        isolate_gitea_env();
    }

    #[test]
    #[serial_test::serial(loom_config_env)]
    fn a_non_json_answer_is_an_error_naming_the_status() {
        isolate_gitea_env();
        let (base, request) = stub("200 OK", "<html>not json</html>");
        std::env::set_var("GITEA_URL", &base);
        std::env::set_var("GITEA_TOKEN", "synthetic-token");
        let dir = tempfile::tempdir().unwrap();
        let err = read(dir.path(), "repos/qual/consumer/pulls/7").unwrap_err();
        assert!(err.contains("200") && err.contains("not JSON"), "{err}");
        request.join().unwrap();
        isolate_gitea_env();
    }

    #[test]
    #[serial_test::serial(loom_config_env)]
    fn no_credential_is_an_error_before_any_request() {
        isolate_gitea_env();
        let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        std::env::set_var("GITEA_URL", format!("http://{}", closed.local_addr().unwrap()));
        drop(closed);
        let dir = tempfile::tempdir().unwrap();
        let err = read(dir.path(), "repos/qual/consumer/pulls/7").unwrap_err();
        assert!(
            err.contains("token is required") && !err.contains("curl"),
            "the credential, not the connection, is what stopped it: {err}"
        );
        assert!(err.contains("GITEA_TOKEN"), "it names the env to set: {err}");
        isolate_gitea_env();
    }
}
