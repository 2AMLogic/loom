//! Shared transport for the Champion PR-diff carve-out verbs (#15605).
//!
//! `forge version-only-diff` (#9611), `forge additive-migrations` (#13102),
//! `forge locked-deps-only` (2am#6942) and `forge suspect-critical-diff` (#12999) each answer a
//! fail-closed question about one PR's diff. Their decision logic differs; the way they reach the
//! forge does not, so it lives here once instead of being cloned into every verb:
//!
//! - the paged `pulls/<n>/files` reader ([`list_files`] for a whole-PR decision, [`find_patch`]
//!   for a single-file lookup) and its [`FileEntry`] model;
//! - `OWNER/REPO` validation and resolution ([`valid_repo`], [`resolve_repo`],
//!   [`resolve_gh_repo`]);
//! - the PR-head file reader ([`PrHeadReader`]) with its [`safe_path`] guard, one lazily cached
//!   head-SHA read per PR, and [`decode_contents`].
//!
//! Every forge read takes the calling verb's `op` label, so `forge_call_stats` attributes the
//! file-list pages and head-SHA/contents reads to the verb that made them rather than to
//! whichever sibling module happened to own the helper.
//!
//! # Two pagination contracts
//!
//! - [`find_patch`] answers "what is this one file's patch?" and returns as soon as the file is
//!   found, without reading later pages. A file not found by the page cap is "not in the list".
//! - [`list_files`] feeds a decision over the **whole** PR, so a list that is still full at
//!   [`MAX_PAGES`] pages may be truncated and is refused ([`ListError::Truncated`]) rather than
//!   decided on a partial file set.

use std::path::Path;

use serde::Deserialize;

/// `per_page` for the file-list read (GitHub's maximum).
pub(crate) const PAGE_SIZE: usize = 100;

/// GitHub's `pulls/<n>/files` stops at 3000 files, i.e. 30 full pages.
pub(crate) const MAX_PAGES: u32 = 30;

/// One entry of a `pulls/<n>/files` page. `filename` is required, so an entry without it fails
/// the whole page; every other field is optional because each verb reads a different subset.
#[derive(Debug, Deserialize)]
pub(crate) struct FileEntry {
    pub(crate) filename: String,
    #[serde(default)]
    pub(crate) status: String,
    #[serde(default)]
    pub(crate) previous_filename: Option<String>,
    #[serde(default)]
    pub(crate) patch: Option<String>,
}

/// Why [`list_files`] could not return the whole list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ListError {
    /// A page fetch failed or did not parse as the expected file list.
    Fetch(String),
    /// The list was still full at [`MAX_PAGES`] pages, so it may be truncated.
    Truncated,
}

/// Why [`find_patch`] could not return a file's patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FindError {
    /// A page fetch failed or did not parse as the expected file list.
    Fetch(String),
    /// The list ended without the file.
    NotInList,
}

/// Parse one raw `pulls/<n>/files` page.
fn parse_page(page: u32, body: &[u8]) -> Result<Vec<FileEntry>, String> {
    serde_json::from_slice(body).map_err(|e| format!("page {page} did not parse: {e}"))
}

/// Walk the whole file list page by page through `fetch_page` (1-based page to raw JSON body).
///
/// # Errors
///
/// [`ListError::Fetch`] on any fetch or parse failure, [`ListError::Truncated`] when the list is
/// still full after [`MAX_PAGES`] pages.
pub(crate) fn list_files<F>(mut fetch_page: F) -> Result<Vec<FileEntry>, ListError>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
{
    let mut all = Vec::new();
    for page in 1..=MAX_PAGES {
        let body = fetch_page(page).map_err(ListError::Fetch)?;
        let entries = parse_page(page, &body).map_err(ListError::Fetch)?;
        let full = entries.len() >= PAGE_SIZE;
        all.extend(entries);
        if !full {
            return Ok(all);
        }
    }
    Err(ListError::Truncated)
}

/// Walk the file list page by page through `fetch_page` and return `file`'s `patch` field,
/// stopping at the page that holds it.
///
/// # Errors
///
/// [`FindError::Fetch`] on any fetch or parse failure, [`FindError::NotInList`] when the list
/// ends (or the page cap is reached) without `file`.
pub(crate) fn find_patch<F>(file: &str, mut fetch_page: F) -> Result<Option<String>, FindError>
where
    F: FnMut(u32) -> Result<Vec<u8>, String>,
{
    for page in 1..=MAX_PAGES {
        let body = fetch_page(page).map_err(FindError::Fetch)?;
        let entries = parse_page(page, &body).map_err(FindError::Fetch)?;
        let full = entries.len() >= PAGE_SIZE;
        if let Some(entry) = entries.into_iter().find(|e| e.filename == file) {
            return Ok(entry.patch);
        }
        if !full {
            break;
        }
    }
    Err(FindError::NotInList)
}

/// Is `repo` a plain `OWNER/REPO`? It is interpolated into an API path, so `..`, extra `/` or
/// anything outside `[A-Za-z0-9._-]` is refused.
pub(crate) fn valid_repo(repo: &str) -> bool {
    let mut parts = repo.split('/');
    let ok = |s: Option<&str>| {
        s.is_some_and(|s| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        })
    };
    ok(parts.next()) && ok(parts.next()) && parts.next().is_none()
}

/// The `OWNER/REPO` every `repos/<r>/…` read is addressed to (#13296): the caller's `--repo`,
/// else the repo `gh`/the git remote resolves for the cwd. Never `gh`'s `{owner}/{repo}`
/// placeholder, which expands from the cwd's remote and silently fails when the process runs
/// outside the PR's checkout.
pub(crate) fn resolve_repo(arg: Option<&str>, gh_bin: &str) -> Result<String, String> {
    if let Some(r) = arg {
        return Ok(r.to_string());
    }
    crate::forge_cmd::repo_nwo(gh_bin)
        .filter(|r| valid_repo(r))
        .ok_or_else(|| {
            "cannot resolve OWNER/REPO (no --repo, and neither `gh repo view` nor the \
             git remote of the working directory names one)"
                .to_string()
        })
}

/// Why [`resolve_gh_repo`] produced no `(gh, repo)` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RepoError {
    /// `--repo` was not a plain `OWNER/REPO`.
    Invalid,
    /// No `--repo`, and none could be resolved from the cwd.
    Unresolved(String),
}

/// Validate `--repo`, then resolve the `gh` binary and the `OWNER/REPO` the reads address:
/// the step every verb's `handle` opens with.
///
/// # Errors
///
/// [`RepoError::Invalid`] for a `--repo` that is not a plain `OWNER/REPO`,
/// [`RepoError::Unresolved`] when there is none and the cwd names none.
pub(crate) fn resolve_gh_repo(arg: Option<&str>) -> Result<(String, String), RepoError> {
    if arg.is_some_and(|r| !valid_repo(r)) {
        return Err(RepoError::Invalid);
    }
    let gh = crate::forge_cmd::gh_bin();
    let repo = resolve_repo(arg, &gh).map_err(RepoError::Unresolved)?;
    Ok((gh, repo))
}

/// Production fetcher: one `gh api` page read through the counted facade, attributed to `op`.
pub(crate) fn gh_fetch_page(
    op: &'static str,
    gh_bin: &Path,
    repo: &str,
    pr: u32,
    page: u32,
) -> Result<Vec<u8>, String> {
    let path = format!("repos/{repo}/pulls/{pr}/files?per_page={PAGE_SIZE}&page={page}");
    crate::verdict_equivalence::gh_api_checked(op, gh_bin, None, &path)
        .map_err(|e| format!("forge read of `{path}` failed: {e}"))
}

/// Reject file names that could address a different API endpoint.
pub(crate) fn safe_path(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('/')
        && !file
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
        && file
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'/'))
}

/// Decode a `repos/<r>/contents/<f>?ref=<sha>` JSON body to text.
pub(crate) fn decode_contents(body: &[u8]) -> Result<String, String> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine as _;
    let v: serde_json::Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    if v.get("encoding").and_then(|e| e.as_str()) != Some("base64") {
        return Err("contents API returned no base64 body".into());
    }
    let raw: String = v
        .get("content")
        .and_then(|c| c.as_str())
        .ok_or("contents API returned no content")?
        .split_whitespace()
        .collect();
    let bytes = STANDARD.decode(raw).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

/// The PR head SHA, read under `op`.
fn gh_head_sha(op: &'static str, gh_bin: &Path, repo: &str, pr: u32) -> Result<String, String> {
    let body = crate::verdict_equivalence::gh_api_checked(
        op,
        gh_bin,
        None,
        &format!("repos/{repo}/pulls/{pr}"),
    )?;
    let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| e.to_string())?;
    v.pointer("/head/sha")
        .and_then(|s| s.as_str())
        .filter(|s| s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .map(str::to_string)
        .ok_or_else(|| "PR head sha missing".into())
}

/// Reads files at one PR's head. The head SHA is looked up on the first read and cached, so a
/// decision that reads several files costs one SHA read per PR; every read is attributed to the
/// calling verb's `op`.
pub(crate) struct PrHeadReader<'a> {
    op: &'static str,
    gh_bin: &'a Path,
    repo: &'a str,
    pr: u32,
    sha: Option<String>,
}

impl<'a> PrHeadReader<'a> {
    pub(crate) fn new(op: &'static str, gh_bin: &'a Path, repo: &'a str, pr: u32) -> Self {
        Self {
            op,
            gh_bin,
            repo,
            pr,
            sha: None,
        }
    }

    /// The text of `file` at the PR head.
    ///
    /// # Errors
    ///
    /// A one-line reason: an unsafe path, or a failed or undecodable forge read.
    pub(crate) fn read(&mut self, file: &str) -> Result<String, String> {
        if !safe_path(file) {
            return Err("unsafe file path".into());
        }
        if self.sha.is_none() {
            self.sha = Some(gh_head_sha(self.op, self.gh_bin, self.repo, self.pr)?);
        }
        let sha = self.sha.as_deref().unwrap_or_default();
        let repo = self.repo;
        let path = format!("repos/{repo}/contents/{file}?ref={sha}");
        decode_contents(&crate::verdict_equivalence::gh_api_checked(
            self.op,
            self.gh_bin,
            None,
            &path,
        )?)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// #13296: an executable stub `gh` that logs its args and fails like the forge.
    #[cfg(unix)]
    fn failing_gh(dir: &Path, stderr: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("gh");
        let log = dir.join("args.log");
        std::fs::write(
            &path,
            format!("#!/bin/sh\necho \"$*\" >> '{}'\necho '{stderr}' >&2\nexit 1\n", log.display()),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    /// `n` entries named `src/f<i>.rs`, as one raw page body.
    fn full_page(n: usize) -> Vec<u8> {
        let entries: Vec<serde_json::Value> = (0..n)
            .map(|i| serde_json::json!({"filename": format!("src/f{i}.rs"), "status": "modified"}))
            .collect();
        serde_json::Value::Array(entries).to_string().into_bytes()
    }

    #[test]
    fn resolve_repo_prefers_explicit_arg() {
        assert_eq!(
            resolve_repo(Some("acme/widgets"), "/nonexistent/gh"),
            Ok("acme/widgets".into())
        );
    }

    #[test]
    fn resolve_gh_repo_refuses_an_invalid_repo_before_any_lookup() {
        assert_eq!(resolve_gh_repo(Some("../x")), Err(RepoError::Invalid));
        assert_eq!(resolve_gh_repo(Some("a/b/c")), Err(RepoError::Invalid));
    }

    #[cfg(unix)]
    #[test]
    fn gh_fetch_page_error_carries_http_status() {
        let dir = tempfile::tempdir().unwrap();
        let gh = failing_gh(dir.path(), "gh: Resource not accessible (HTTP 403)");
        let err =
            gh_fetch_page("forge.additive_migrations", &gh, "acme/widgets", 7, 1).unwrap_err();
        assert!(err.contains("HTTP 403"), "{err}");
        assert!(err.contains("repos/acme/widgets/pulls/7/files"), "{err}");
        assert!(!err.contains("{owner}"), "{err}");
    }

    #[test]
    fn repo_argument_is_validated() {
        assert!(valid_repo("rjwalters/loom"));
        assert!(valid_repo("a-b/c.d_e"));
        for bad in ["", "loom", "a/b/c", "../x", "a/..", "a/b?x=1", "a /b"] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }

    #[test]
    fn safe_path_rejects_traversal_and_query() {
        assert!(safe_path("loom-daemon/Cargo.toml"));
        assert!(safe_path("migrations/0001_x.sql"));
        for bad in [
            "",
            "/a",
            "a/../b",
            "a//b",
            "a?ref=x",
            "a b",
            "./a",
            "migrations/../x.sql",
        ] {
            assert!(!safe_path(bad), "{bad}");
        }
    }

    #[test]
    fn contents_body_decodes_base64_with_newlines() {
        let body =
            serde_json::json!({"encoding": "base64", "content": "W3BhY2thZ2Vd\nCg=="}).to_string();
        assert_eq!(decode_contents(body.as_bytes()).unwrap(), "[package]\n");
        assert!(decode_contents(br#"{"encoding":"none","content":""}"#).is_err());
        assert!(decode_contents(br#"{"encoding":"base64"}"#).is_err());
        assert!(decode_contents(b"nope").is_err());
    }

    #[test]
    fn pr_head_reader_refuses_an_unsafe_path_without_a_forge_read() {
        let mut reader =
            PrHeadReader::new("forge.locked_deps_only", Path::new("/nonexistent/gh"), "a/b", 1);
        assert_eq!(reader.read("../etc/passwd"), Err("unsafe file path".into()));
        assert!(reader.sha.is_none());
    }

    // ---- whole-PR list: refuses a truncated list

    #[test]
    fn list_files_collects_every_page() {
        let page2 = br#"[{"filename":"migrations/9.sql","status":"added"}]"#.to_vec();
        let list = list_files(|p| {
            Ok(if p == 1 {
                full_page(PAGE_SIZE)
            } else {
                page2.clone()
            })
        })
        .unwrap();
        assert_eq!(list.len(), PAGE_SIZE + 1);
        assert_eq!(list.last().unwrap().filename, "migrations/9.sql");
        assert_eq!(list.last().unwrap().status, "added");
    }

    #[test]
    fn list_files_refuses_a_list_still_full_at_the_cap() {
        let mut pages = 0;
        let r = list_files(|_| {
            pages += 1;
            Ok(full_page(PAGE_SIZE))
        });
        assert_eq!(r.unwrap_err(), ListError::Truncated);
        assert_eq!(pages, MAX_PAGES);
    }

    #[test]
    fn list_files_accepts_exactly_max_pages_when_the_last_is_short() {
        let r = list_files(|p| Ok(full_page(if p < MAX_PAGES { PAGE_SIZE } else { 3 }))).unwrap();
        assert_eq!(r.len(), PAGE_SIZE * (MAX_PAGES as usize - 1) + 3);
    }

    #[test]
    fn list_files_fetch_and_parse_errors_are_fetch_errors() {
        assert_eq!(
            list_files(|_| Err("api down".into())).unwrap_err(),
            ListError::Fetch("api down".into())
        );
        let r = list_files(|_| Ok(b"not json".to_vec())).unwrap_err();
        assert!(
            matches!(r, ListError::Fetch(ref why) if why.starts_with("page 1 did not parse")),
            "{r:?}"
        );
        let r = list_files(|_| Ok(br#"{"message":"Not Found"}"#.to_vec())).unwrap_err();
        assert!(matches!(r, ListError::Fetch(_)), "{r:?}");
    }

    // ---- single-file lookup: returns as soon as the file is found

    #[test]
    fn find_patch_returns_without_reading_later_pages() {
        let page1 =
            br#"[{"filename":"a.rs"},{"filename":"Cargo.lock","patch":"@@ -1 +1 @@"}]"#.to_vec();
        let mut pages = Vec::new();
        let r = find_patch("Cargo.lock", |p| {
            pages.push(p);
            Ok(page1.clone())
        });
        assert_eq!(r, Ok(Some("@@ -1 +1 @@".to_string())));
        assert_eq!(pages, vec![1]);
    }

    #[test]
    fn find_patch_does_not_stop_at_a_full_page_that_lacks_the_file() {
        let page2 = br#"[{"filename":"Cargo.lock"}]"#.to_vec();
        let mut pages = Vec::new();
        let r = find_patch("Cargo.lock", |p| {
            pages.push(p);
            Ok(if p == 1 {
                full_page(PAGE_SIZE)
            } else {
                page2.clone()
            })
        });
        assert_eq!(r, Ok(None));
        assert_eq!(pages, vec![1, 2]);
    }

    #[test]
    fn find_patch_reports_a_missing_file_and_a_failing_page_differently() {
        assert_eq!(find_patch("x", |_| Ok(b"[]".to_vec())), Err(FindError::NotInList));
        // A full page then a failing one is a fetch failure, never "not in list".
        let r = find_patch("x", |p| {
            if p == 1 {
                Ok(full_page(PAGE_SIZE))
            } else {
                Err("HTTP 502".into())
            }
        });
        assert_eq!(r, Err(FindError::Fetch("HTTP 502".into())));
        let r = find_patch("x", |_| Ok(b"not json".to_vec()));
        assert!(matches!(r, Err(FindError::Fetch(_))), "{r:?}");
    }

    #[test]
    fn find_patch_stops_at_the_page_cap_without_the_file() {
        let mut pages = 0;
        let r = find_patch("missing", |_| {
            pages += 1;
            Ok(full_page(PAGE_SIZE))
        });
        assert_eq!(r, Err(FindError::NotInList));
        assert_eq!(pages, MAX_PAGES);
    }
}
