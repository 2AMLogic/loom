//! The fleet curation-write broker client (loom#14465; 2am#8390 / 2am#8397).
//!
//! In fleet mode `loom-tools curation-complete` and
//! `loom-tools notify-cleared-blockers` must not run their label edits and
//! comments on an ambient `gh` credential: the fleet writer Apps hold no
//! label or conversation scope, and the write-scope probe would consult a
//! reader that cannot answer for them. The operator ruling of 2026-11-23
//! (`/fleet:decide` round 20261121-r19, r19-q339) authorized exactly two
//! purpose-built broker operations for these workflows —
//! `POST /v1/complete_curation` and `POST /v1/notify_cleared_blocker` on the
//! `github-write` broker, callable by the `fleet-roles` caller profile only
//! — and nothing else: no generic label or comment grant.
//!
//! **Fail closed, no fallback** (the same contract
//! `forge.mergeQueue.statusCommand` holds, #12679): a transport failure, a
//! refusal, or an unreadable answer is an error; nothing here ever retries
//! as a local `gh` write. The server verifies the caller, the repository
//! (`curation-policy.json`) and the target before its first mutation, so a
//! refusal leaves nothing written.
//!
//! # Provisioning (the names the merge-queue status poster already uses)
//!
//! - URL: [`URL_ENV`] (`GITHUB_WRITE_BROKER_URL`), default
//!   <https://bot-issues.2amlogic.com>.
//! - Caller key: [`KEY_ENV`] (`FLEET_ROLES_GITHUB_WRITE_KEY`), else the first
//!   line of [`KEY_FILE_ENV`] ([`DEFAULT_KEY_FILE`]), which must be mode
//!   0600. The key travels only in the `Authorization` header — never in
//!   argv, a log line or an error.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;

/// Where the broker answers, when the environment does not name it.
pub const DEFAULT_URL: &str = "https://bot-issues.2amlogic.com";

/// Environment override for the broker origin.
pub const URL_ENV: &str = "GITHUB_WRITE_BROKER_URL";

/// The `fleet-roles` caller key, when it is exported directly.
pub const KEY_ENV: &str = "FLEET_ROLES_GITHUB_WRITE_KEY";

/// Where [`KEY_ENV`] is not set: names the key file to read instead.
pub const KEY_FILE_ENV: &str = "FLEET_ROLES_GITHUB_WRITE_KEY_FILE";

/// The provisioned key file's default location
/// (`provision.sh apply --section fleetroleskey` writes it, 0600).
pub const DEFAULT_KEY_FILE: &str = "~/.config/2am/github-write-fleet-roles.key";

/// Bound on one broker call, matching the status poster's command timeout.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// How one request reaches the broker. Injected so the request/response
/// contract is testable without a network.
pub trait Transport {
    /// `POST url` with `Authorization: Bearer <key>` and `body` as JSON.
    /// Returns the HTTP status and the response body text; `Err` is a
    /// transport-level failure (unreachable, timeout, malformed answer).
    fn post_json(&self, url: &str, key: &str, body: &str) -> Result<(u16, String), String>;
}

/// The production transport: `reqwest` on a private current-thread runtime,
/// no redirects (a redirect could carry the statement somewhere other than
/// the configured broker). Callers are synchronous CLI paths.
#[derive(Debug, Clone, Copy, Default)]
pub struct Reqwest;

impl Transport for Reqwest {
    fn post_json(&self, url: &str, key: &str, body: &str) -> Result<(u16, String), String> {
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("runtime: {e}"))?;
                    runtime.block_on(async move {
                        let client = reqwest::Client::builder()
                            .timeout(REQUEST_TIMEOUT)
                            .redirect(reqwest::redirect::Policy::none())
                            .build()
                            .map_err(|e| format!("client: {e}"))?;
                        let response = client
                            .post(url)
                            .header(reqwest::header::AUTHORIZATION, format!("Bearer {key}"))
                            .header(reqwest::header::CONTENT_TYPE, "application/json")
                            .body(body.to_owned())
                            .send()
                            .await
                            .map_err(|e| format!("send: {e}"))?;
                        let status = response.status().as_u16();
                        let text = response.text().await.map_err(|e| format!("body: {e}"))?;
                        Ok((status, text))
                    })
                })
                .join()
                .map_err(|_| "transport thread panicked".to_string())?
        })
    }
}

/// A shared broker handle is a broker: the handlers hold `&Broker<T>`.
impl<T: Transport> Transport for &T {
    fn post_json(&self, url: &str, key: &str, body: &str) -> Result<(u16, String), String> {
        (**self).post_json(url, key, body)
    }
}

/// Why a broker call did not produce its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BrokerError {
    /// The broker could not be reached or its answer was transport-level
    /// unusable. Nothing was written.
    Transport(String),
    /// The broker refused (any non-2xx): `code` is its `error` field.
    Refused {
        status: u16,
        code: String,
        message: String,
    },
    /// A 2xx whose body is not the documented shape.
    Unreadable(String),
}

impl std::fmt::Display for BrokerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(why) => write!(f, "fleet broker unreachable: {why}"),
            Self::Refused {
                status,
                code,
                message,
            } => {
                if message.is_empty() {
                    write!(f, "fleet broker refused ({status} {code})")
                } else {
                    write!(f, "fleet broker refused ({status} {code}): {message}")
                }
            }
            Self::Unreadable(why) => write!(f, "fleet broker answer unreadable: {why}"),
        }
    }
}

impl std::error::Error for BrokerError {}

/// The `complete_curation` outcome, read back by the broker from GitHub.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CompleteCuration {
    pub number: i64,
    pub points: String,
    #[serde(default)]
    pub previous_points: Vec<String>,
    #[serde(default)]
    pub curated: bool,
    #[serde(default)]
    pub changed: bool,
    #[serde(default)]
    pub replayed: bool,
}

/// The `notify_cleared_blocker` outcome.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct NotifyCleared {
    pub number: i64,
    pub closed: i64,
    #[serde(default)]
    pub unblocked: bool,
    #[serde(default)]
    pub comment: Option<CommentRef>,
    #[serde(default)]
    pub replayed: bool,
}

/// The one marker comment the op guarantees (existing or freshly posted).
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct CommentRef {
    pub id: i64,
    pub url: String,
}

/// A broker handle: where it answers and with whose key.
pub struct Broker<T: Transport = Reqwest> {
    url: String,
    key: String,
    transport: T,
}

impl Broker<Reqwest> {
    /// Resolve the broker from the environment: the URL override, then the
    /// caller key by env or key file. `Err` is the actionable refusal text;
    /// resolving never touches the network, so a missing key fails before
    /// any forge read.
    ///
    /// # Errors
    /// The key is absent, empty, malformed, or its file is not 0600.
    pub fn resolve_from_env() -> Result<Self, String> {
        let url = match std::env::var(URL_ENV) {
            Ok(u) if !u.trim().is_empty() => u.trim().trim_end_matches('/').to_owned(),
            _ => DEFAULT_URL.to_owned(),
        };
        let key = resolve_key(
            std::env::var(KEY_ENV).ok().as_deref(),
            std::env::var(KEY_FILE_ENV).ok().as_deref(),
        )?;
        Ok(Self {
            url,
            key,
            transport: Reqwest,
        })
    }
}

impl<T: Transport> Broker<T> {
    /// Build a broker over an injected transport (tests, probes).
    #[must_use]
    pub fn with_transport(url: impl Into<String>, key: impl Into<String>, transport: T) -> Self {
        Self {
            url: url.into(),
            key: key.into(),
            transport,
        }
    }

    /// Complete curation on the admitted issue: the broker removes every
    /// stale `points:*` label, adds `points:N` and `loom:curated`, and reads
    /// the result back. A retry with the same arguments replays or finishes
    /// the cleanup; a refused or unreachable broker is an error, never a
    /// local label write.
    ///
    /// # Errors
    /// See [`BrokerError`]; nothing was written on any error path.
    pub fn complete_curation(
        &self,
        repo: &str,
        number: u32,
        points: u32,
    ) -> Result<CompleteCuration, BrokerError> {
        let (request, _replay_key) = curation_request(repo, number, points);
        self.call("complete_curation", &request).and_then(|v| {
            serde_json::from_value(v).map_err(|e| BrokerError::Unreadable(e.to_string()))
        })
    }

    /// Notify one dependent of one just-closed blocker: the broker verifies
    /// the trigger and the citation, performs the eligible issue's
    /// `loom:blocked` -> `loom:curated` transition first, and posts the one
    /// marker comment deduplicated by loom's own
    /// `<!-- loom:blocker-cleared:#N -->` marker — so a comment posted
    /// locally and one posted here suppress each other.
    ///
    /// # Errors
    /// See [`BrokerError`]; nothing was written on any error path.
    pub fn notify_cleared_blocker(
        &self,
        repo: &str,
        number: i64,
        closed: i64,
    ) -> Result<NotifyCleared, BrokerError> {
        let (request, _replay_key) = notify_request(repo, number, closed);
        self.call("notify_cleared_blocker", &request).and_then(|v| {
            serde_json::from_value(v).map_err(|e| BrokerError::Unreadable(e.to_string()))
        })
    }

    fn call(&self, op: &str, request: &Value) -> Result<Value, BrokerError> {
        let url = format!("{}/v1/{op}", self.url.trim_end_matches('/'));
        let (status, text) = self
            .transport
            .post_json(&url, &self.key, &request.to_string())
            .map_err(BrokerError::Transport)?;
        if (200..300).contains(&status) {
            serde_json::from_str(&text).map_err(|e| BrokerError::Unreadable(e.to_string()))
        } else {
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            Err(BrokerError::Refused {
                status,
                code: body
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned(),
                message: body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            })
        }
    }
}

/// The `complete_curation` request and its stable idempotency key. The key
/// names the points value: a re-estimate is a new call (the broker's record
/// is payload-hashed), while a retry of the same projection replays or
/// finishes a partial cleanup.
#[must_use]
pub fn curation_request(repo: &str, number: u32, points: u32) -> (Value, String) {
    let repo = repo.to_ascii_lowercase();
    let key =
        hashed(b"loom:curation-complete\0", &[&repo, &number.to_string(), &points.to_string()]);
    (
        serde_json::json!({
            "repo": repo,
            "number": number,
            "points": points.to_string(),
            "idempotency_key": key,
        }),
        key,
    )
}

/// The `notify_cleared_blocker` request and its stable idempotency key: one
/// key per (dependent, closed) pair, so a retried notification replays
/// instead of double-posting.
#[must_use]
pub fn notify_request(repo: &str, number: i64, closed: i64) -> (Value, String) {
    let repo = repo.to_ascii_lowercase();
    let key = hashed(
        b"loom:notify-cleared-blocker\0",
        &[&repo, &number.to_string(), &closed.to_string()],
    );
    (
        serde_json::json!({
            "repo": repo,
            "number": number,
            "closed": closed,
            "idempotency_key": key,
        }),
        key,
    )
}

fn hashed(domain: &[u8], parts: &[&str]) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(domain);
    for p in parts {
        h.update(p.as_bytes());
        h.update([0]);
    }
    hex::encode(h.finalize())
}

/// The caller key: the env value when it is non-blank, else the first line
/// of the key file (mode 0600). Mirrors `post-merge-status.py` (2am#5102),
/// the other consumer of the `fleet-roles` key, so one provisioning section
/// serves both.
fn resolve_key(env_key: Option<&str>, key_file_env: Option<&str>) -> Result<String, String> {
    if let Some(key) = env_key.map(str::trim).filter(|k| !k.is_empty()) {
        return check_key_chars(key);
    }
    let raw = key_file_env
        .filter(|p| !p.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| expand_home(DEFAULT_KEY_FILE));
    let missing = || {
        format!(
            "no fleet-roles caller key: set {KEY_ENV} or provide {} (0600); \
             provision.sh apply --section fleetroleskey pulls this host's own key",
            raw.display()
        )
    };
    let meta = std::fs::metadata(&raw).map_err(|_| missing())?;
    if std::os::unix::fs::MetadataExt::mode(&meta) & 0o777 != 0o600 {
        return Err(format!("{} must be mode 0600", raw.display()));
    }
    let first = std::fs::read_to_string(&raw)
        .map_err(|_| missing())?
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_owned();
    if first.is_empty() {
        return Err(format!("{} is empty", raw.display()));
    }
    check_key_chars(&first)
}

/// A key with whitespace or a control character would make the HTTP layer
/// raise with the key in its message. Refuse without naming it.
fn check_key_chars(key: &str) -> Result<String, String> {
    if key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("{KEY_ENV} contains whitespace or control characters"));
    }
    Ok(key.to_owned())
}

/// Prefix-only `~/` expansion for the default key file: a bare `~` stays
/// literal (issue #15372 — delegates to the shared
/// [`crate::fs_util::expand_tilde_with_home`] for the `~/` form only).
fn expand_home(path: &str) -> PathBuf {
    if !path.starts_with("~/") {
        return PathBuf::from(path);
    }
    crate::fs_util::expand_tilde_with_home(path, dirs::home_dir().as_deref())
}

#[cfg(test)]
mod tests;
