//! Relay Git reads for private-seat preparation (#14935).
//!
//! A fleet host whose GitHub egress goes through the `github-read` relay
//! cannot clone or fetch private seats directly: the direct fetch is what
//! exits 128 there. The relay's git transport (2AMLogic/2am §"The git
//! transport", 2am#6694) serves `git-upload-pack` under
//! `https://<gateway>/<owner>/<repo>.git` to a **Git-enabled read profile**
//! — a routed `read` profile with a `repos` list; the relay API profiles of
//! `forge.gateway.keyRefs` have no Git access — and refuses
//! `git-receive-pack` for every profile, so writes stay direct.
//!
//! Two halves, one contract:
//!
//! * the daemon (host) resolves [`RelayRoute`] — `forge.gateway` plus the
//!   repository owner's `forge.gateway.git.keyRefs` entry, the key read per
//!   exec like [`super::forge_auth::ExecAuth`] resolves a token — and hands
//!   it to the `docker exec` as two bare `--env NAME` variables, so the key
//!   never appears in argv, in repository configuration or in a log;
//! * the worker's `prepare` ([`RelayRead`], inside the exec) routes the
//!   initial clone and the preparation fetch through the relay with a
//!   per-process whole-URL `url.<relay>.insteadOf` rewrite and the relay's
//!   Bearer key as `http.extraHeader` — both `GIT_CONFIG_*` environment,
//!   never argv, never the clone's config — with the exec's installation
//!   token stripped from the Git child. The clone keeps its canonical
//!   `origin`, remote-tracking refs and push destination; `recover`'s
//!   publish and every other verb keep the direct owner-authorized path.
//!
//! A configured route has no fallback: a refused profile, repository or key
//! is an actionable refusal (the relay's own diagnostic), never a silent
//! switch to the writer — the direct fetch is what the relay replaces. With
//! no `git` entry for the owner, nothing changes anywhere.

use anyhow::Result;
use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

/// The exec variable carrying the relay's Git URL for one repository.
pub(super) const RELAY_URL_ENV: &str = "LOOM_PRIVATE_GIT_RELAY_URL";

/// The exec variable carrying the relay's Bearer header line.
pub(super) const RELAY_HEADER_ENV: &str = "LOOM_PRIVATE_GIT_RELAY_HEADER";

/// How long the read-endpoint probe may take, whatever the network does.
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

/// The most probe bytes read from a refusal body: the relay's diagnostics
/// are one short JSON line, and Git's own stderr is never read at all.
const PROBE_BODY_LIMIT: usize = 8192;

/// The relay's Git URL for one repository: the gateway origin plus
/// `/<owner>/<name>.git`, the Worker's served path shape.
fn relay_url(api_host: &str, owner: &str, name: &str) -> String {
    format!("https://{api_host}/{owner}/{name}.git")
}

/// The lowercase `(owner, name)` a canonical repository URL names, whatever
/// scheme or forge hosts it (the path's last two segments). The relay route
/// must name exactly this pair, so a route can never steer a read at
/// another repository.
fn owner_name_of(repository: &str) -> Option<(String, String)> {
    let url = reqwest::Url::parse(repository).ok()?;
    let segments: Vec<String> = url
        .path()
        .trim_matches('/')
        .split('/')
        .map(|s| s.strip_suffix(".git").unwrap_or(s).to_ascii_lowercase())
        .collect();
    if segments.len() < 2 || segments.iter().any(|s| s.is_empty()) {
        return None;
    }
    Some((segments[segments.len() - 2].clone(), segments[segments.len() - 1].clone()))
}

/// Whether `host[:port]` is a loopback endpoint: the one place a plaintext
/// `http://` origin is accepted (the loopback test seam this crate's other
/// fixtures already use; nothing leaves the host).
fn is_loopback(host: &str) -> bool {
    let name = if host.starts_with('[') {
        host.split(']').next().unwrap_or(host)
    } else {
        host.rsplit_once(':').map_or(host, |(h, _)| h)
    };
    name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]"
}

/// The lowercase `(owner, name)` of a github.com repository URL — the one
/// host the relay serves. `None` for anything else (Gitea, GitHub
/// Enterprise, an unparseable URL).
fn github_owner_name(repository: &str) -> Option<(String, String)> {
    let id = super::identify(repository).ok()?;
    let rest = id.key.strip_prefix("github.com/")?;
    let (owner, name) = rest.split_once('/')?;
    (!name.contains('/')).then(|| (owner.to_owned(), name.to_owned()))
}

// ---------------------------------------------------------------------------
// The host half: what one preparation exec carries.
// ---------------------------------------------------------------------------

/// The relay route one preparation exec carries (#14935): the relay's
/// credential-free Git URL for the target repository, and the Bearer header
/// line of its Git-enabled read profile. Deliberately not `Debug` or
/// `Clone`: the header must not be printable or copied into state.
pub(super) struct RelayRoute {
    url: String,
    host: String,
    header: String,
}

impl RelayRoute {
    /// Resolve the route for `repository` from the effective config of
    /// `workspace`. `Ok(None)` — today's direct path — when the gateway is
    /// unconfigured, the repository is not on github.com, or the owner has
    /// no `forge.gateway.git.keyRefs` entry.
    ///
    /// # Errors
    ///
    /// A configured Git key that cannot be read: preparation is refused
    /// with the ref and the reason (never the key) rather than silently
    /// falling back to the direct fetch the relay exists to replace.
    pub(super) fn resolve(workspace: &Path, repository: &str) -> Result<Option<Self>> {
        let effective = crate::config_resolver::resolve_effective_config(workspace);
        Self::from_config(&effective, repository)
    }

    /// [`Self::resolve`] over an injected effective config (tests).
    pub(super) fn from_config(
        effective: &serde_json::Value,
        repository: &str,
    ) -> Result<Option<Self>> {
        let Some((owner, name)) = github_owner_name(repository) else {
            return Ok(None);
        };
        let (gateway, _) = crate::forge_gateway::parse(effective);
        let Some(gateway) = gateway else {
            return Ok(None);
        };
        let Some(key_ref) = gateway.git_key_refs.get(&owner) else {
            return Ok(None);
        };
        let region = crate::forge_gateway::resolve_region(gateway.region.as_deref(), &|k| {
            std::env::var(k).ok().filter(|v| !v.is_empty())
        });
        let aws = crate::forge_gateway::AwsAccess {
            region: region.as_deref(),
            profile: &gateway.aws_profile,
        };
        let key = key_ref
            .read(&aws, crate::forge_gateway::GIT_REFS_KEY)
            .map_err(|why| {
                anyhow::anyhow!(
                    "relay Git key for {owner} cannot be read ({}.{owner}: {why}); \
                     fix the key ref or drop the git block to prepare with direct Git reads",
                    crate::forge_gateway::GIT_REFS_KEY
                )
            })?;
        let url = relay_url(&gateway.api_host, &owner, &name);
        let host = gateway.api_host.clone();
        Ok(Some(Self {
            header: format!("Authorization: Bearer {key}"),
            host,
            url,
        }))
    }

    /// The variables set on the `docker` client child — values, never argv.
    pub(super) fn env(&self) -> Vec<(String, String)> {
        vec![
            (RELAY_URL_ENV.to_owned(), self.url.clone()),
            (RELAY_HEADER_ENV.to_owned(), self.header.clone()),
        ]
    }

    /// The relay host, for logs that never carry the key.
    pub(super) fn host(&self) -> &str {
        &self.host
    }
}

// ---------------------------------------------------------------------------
// The worker half: what `prepare` consumes inside the exec.
// ---------------------------------------------------------------------------

/// The relay route the worker's `prepare` consumes (#14935), adopted from
/// the exec's environment. Exists only when the daemon proved a route;
/// carries no fallback by construction.
pub(super) struct RelayRead {
    url: String,
    host: String,
    header: String,
}

impl RelayRead {
    /// Adopt the exec's relay variables for `repository`. `Ok(None)` when
    /// neither is set (the direct path). `Err` — actionable, and never
    /// carrying the header's value — when the contract is violated: one
    /// variable alone, a URL that is not this repository's relay shape, or
    /// a header that is not one Bearer line.
    pub(super) fn from_env_for(
        repository: &str,
        env: &dyn Fn(&str) -> Option<OsString>,
    ) -> std::result::Result<Option<Self>, String> {
        let (url, header) = match (env(RELAY_URL_ENV), env(RELAY_HEADER_ENV)) {
            (None, None) => return Ok(None),
            (None, Some(_)) | (Some(_), None) => {
                return Err(format!(
                    "relay Git reads need both {RELAY_URL_ENV} and {RELAY_HEADER_ENV}; \
                     the preparation exec carries only one"
                ));
            }
            (Some(url), Some(header)) => (url, header),
        };
        let url = url
            .into_string()
            .map_err(|_| format!("{RELAY_URL_ENV} is not valid UTF-8"))?;
        let header = header
            .into_string()
            .map_err(|_| format!("{RELAY_HEADER_ENV} is not valid UTF-8"))?;
        let parsed =
            reqwest::Url::parse(&url).map_err(|e| format!("{RELAY_URL_ENV} is not a URL: {e}"))?;
        let Some(hostname) = parsed.host_str().map(str::to_owned) else {
            return Err(format!("{RELAY_URL_ENV} names no host"));
        };
        let host = match parsed.port() {
            Some(port) => format!("{hostname}:{port}"),
            None => hostname,
        };
        // The key rides this URL: HTTPS, except a loopback endpoint the
        // same seam every loopback fixture in this crate uses (a plaintext
        // http:// origin that never leaves the host).
        if (parsed.scheme() != "https" && !(parsed.scheme() == "http" && is_loopback(&host)))
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(format!("{RELAY_URL_ENV} must be a credential-free HTTPS URL"));
        }
        let segments: Vec<&str> = parsed.path().trim_matches('/').split('/').collect();
        let [owner, name] = segments[..] else {
            return Err(format!("{RELAY_URL_ENV} must be the relay's /<owner>/<name>.git URL"));
        };
        let Some(name) = name.strip_suffix(".git") else {
            return Err(format!("{RELAY_URL_ENV} must be the relay's /<owner>/<name>.git URL"));
        };
        if !super::safe_segment(owner)
            || !super::safe_segment(name)
            || owner.is_empty()
            || name.is_empty()
        {
            return Err(format!(
                "{RELAY_URL_ENV} names an owner or repository unsafe as a path component"
            ));
        }
        let (canonical_owner, canonical_name) = owner_name_of(repository).ok_or_else(|| {
            format!(
                "relay Git reads need a canonical repository URL; {RELAY_URL_ENV} names a \
                 relay route for a repository that is not one"
            )
        })?;
        if (owner.to_ascii_lowercase(), name.to_ascii_lowercase())
            != (canonical_owner, canonical_name)
        {
            return Err(format!(
                "{RELAY_URL_ENV} names {owner}/{name}, not the prepared repository's \
                 owner and repository; refusing to read a different repository through the relay"
            ));
        }
        let key = header
            .strip_prefix("Authorization: Bearer ")
            .ok_or_else(|| format!("{RELAY_HEADER_ENV} must be one Bearer authorization line"))?;
        if key.is_empty() || key.chars().any(char::is_control) {
            return Err(format!(
                "{RELAY_HEADER_ENV} must be one non-empty Bearer authorization line"
            ));
        }
        Ok(Some(Self { url, host, header }))
    }

    /// The Git child's environment for one routed read: a whole-URL
    /// `insteadOf` rewriting exactly the canonical URL (nothing else), the
    /// relay's Bearer key as `http.extraHeader`, and no redirects — the
    /// relay answers inline. All `GIT_CONFIG_*` (git ≥ 2.31), so neither the
    /// key nor the rewrite is ever argv or repository configuration.
    pub(super) fn git_env(&self, canonical: &str) -> Vec<(String, String)> {
        vec![
            ("GIT_CONFIG_COUNT".to_owned(), "3".to_owned()),
            ("GIT_CONFIG_KEY_0".to_owned(), format!("url.{}.insteadOf", self.url)),
            ("GIT_CONFIG_VALUE_0".to_owned(), canonical.to_owned()),
            ("GIT_CONFIG_KEY_1".to_owned(), "http.extraHeader".to_owned()),
            ("GIT_CONFIG_VALUE_1".to_owned(), self.header.clone()),
            ("GIT_CONFIG_KEY_2".to_owned(), "http.followRedirects".to_owned()),
            ("GIT_CONFIG_VALUE_2".to_owned(), "false".to_owned()),
        ]
    }

    /// Ask the relay for the repository's read endpoint before Git runs
    /// (#14935): a refused profile, repository or key answers with the
    /// relay's own diagnostic — actionable, unlike Git's exit 128 — and the
    /// read stops there rather than switching to a writer.
    ///
    /// # Errors
    ///
    /// The refusal or transport failure, never the key.
    pub(super) fn probe(&self) -> std::result::Result<(), String> {
        let url = format!("{}/info/refs?service=git-upload-pack", self.url);
        let host = self.host.clone();
        let key = self
            .header
            .strip_prefix("Authorization: Bearer ")
            .unwrap_or_default()
            .to_owned();
        // A private current-thread runtime on its own thread: the worker is
        // synchronous, and this must not depend on — or block — any runtime
        // an embedding process carries.
        let join_host = self.host.clone();
        std::thread::scope(|scope| {
            scope
                .spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|e| format!("no HTTP runtime: {e}"))?;
                    runtime.block_on(async {
                        let client = reqwest::Client::builder()
                            .timeout(PROBE_TIMEOUT)
                            .redirect(reqwest::redirect::Policy::none())
                            .build()
                            .map_err(|e| format!("no HTTP client: {e}"))?;
                        let mut response =
                            client.get(url).bearer_auth(key).send().await.map_err(|e| {
                                format!("relay {host} is unreachable: {}", e.without_url())
                            })?;
                        let status = response.status();
                        if status.is_success() {
                            return Ok(());
                        }
                        let mut body = String::new();
                        while body.len() < PROBE_BODY_LIMIT {
                            let Some(chunk) = response
                                .chunk()
                                .await
                                .map_err(|e| format!("relay {host} body: {}", e.without_url()))?
                            else {
                                break;
                            };
                            body.push_str(&String::from_utf8_lossy(&chunk));
                        }
                        let message = serde_json::from_str::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|v| {
                                v.get("message").and_then(|m| m.as_str()).map(str::to_owned)
                            });
                        match message {
                            Some(message) => Err(format!(
                                "relay {host} refused the Git read (HTTP {status}): {message}"
                            )),
                            None => Err(format!(
                                "relay {host} answered HTTP {status} where the Git read \
                                 endpoint was expected"
                            )),
                        }
                    })
                })
                .join()
                .unwrap_or_else(|_| Err(format!("relay {join_host} probe thread panicked")))
        })
    }
}

// ---------------------------------------------------------------------------
// The routed invocation (kept beside `repository::git`'s concerns).
// ---------------------------------------------------------------------------

/// The environment variables a routed Git read never inherits: the exec's
/// installation token (#14935 acceptance: the reads succeed without one in
/// their Git process) and git's legacy serialized `-c` channel, which could
/// otherwise inject a competing `url.*.insteadOf` rewrite (#14726 release
/// review, item 1).
pub(super) const ROUTED_STRIP: [&str; 3] = ["GH_TOKEN", "GITHUB_TOKEN", "GIT_CONFIG_PARAMETERS"];

/// One preparation Git read (#14935): direct, or relay-routed — the rewrite
/// and the relay key on the child's environment, the installation token
/// stripped, after asking the relay for the read endpoint so a refusal is
/// the relay's own diagnostic instead of Git's exit 128. The canonical URL
/// stays what the clone records as `origin`.
pub(super) fn git_read(
    root: &Path,
    args: &[&str],
    relay: Option<&RelayRead>,
    canonical: &str,
) -> Result<String> {
    match relay {
        None => super::repository::git(root, args),
        Some(relay) => {
            relay.probe().map_err(anyhow::Error::msg)?;
            let env = relay.git_env(canonical);
            super::repository::git_routed(root, args, &env, &ROUTED_STRIP)
        }
    }
}
