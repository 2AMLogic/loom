use super::*;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use super::super::docker_cli;

/// Budget for the long `exec`s (`setup`, `prepare`): a clone or fetch inside
/// the session legitimately outlasts a reconcile-sized call.
pub(super) const EXEC_TIMEOUT: Duration = docker_cli::DOCKER_RUN_TIMEOUT;

/// Capture without forwarding Docker stderr: exec/auth errors may contain
/// credential material. All callers report the operation, never its payload.
///
/// Bounded by [`docker_cli::timeout_for`], so a wedged engine fails the call
/// with the typed [`docker_cli::DockerTimedOut`] instead of holding the lease
/// (and the reconciler thread) forever.
pub(super) fn command(args: &[&str]) -> Result<String> {
    command_with(args, docker_cli::timeout_for(args))
}

/// [`command`] with an explicit budget, for calls longer than their
/// subcommand's default (`exec` of `setup`/`prepare`).
pub(super) fn command_with(args: &[&str], timeout: Duration) -> Result<String> {
    command_env(args, &[], timeout)
}

/// [`command_with`] with variables set on the `docker` client process only
/// (#13916): a forge token resolved for one exec, named in `args` as a bare
/// `--env NAME` so its value never appears in argv.
pub(super) fn command_env(
    args: &[&str],
    env: &[(String, String)],
    timeout: Duration,
) -> Result<String> {
    let (success, stdout) = command_status_env(args, env, timeout)?;
    if !success {
        bail!(
            "Docker {} failed; inspect the session with Docker locally",
            args.first().unwrap_or(&"operation"),
        );
    }
    Ok(stdout)
}

/// Batch volume inspect can report found volumes on stdout after an ordinary
/// nonzero exit. Only the sampler reconciles those per-volume results.
pub(super) fn inspect_volume_batch(
    volumes: &[String],
    format: &str,
    timeout: Duration,
) -> Result<(bool, String)> {
    let mut args = vec!["volume", "inspect", "--format", format];
    args.extend(volumes.iter().map(String::as_str));
    command_status_env(&args, &[], timeout)
}

fn command_status_env(
    args: &[&str],
    env: &[(String, String)],
    timeout: Duration,
) -> Result<(bool, String)> {
    // Every Docker call of this module goes through here, so a fake installed
    // on this thread (`fake`, tests only) stands in for the whole engine.
    #[cfg(test)]
    if let Some(result) = fake::dispatch(args, env, timeout) {
        return result;
    }
    let (success, stdout, _stderr) = docker_cli::run_bounded_env("docker", args, env, timeout)
        .map_err(|error| match error.downcast::<docker_cli::DockerTimedOut>() {
            Ok(timed_out) => anyhow::Error::new(timed_out),
            Err(error) => error.context("Docker unavailable"),
        })?;
    Ok((success, stdout))
}

pub(super) fn desktop() -> Result<bool> {
    Ok(cfg!(target_os = "macos")
        && command(&["info", "--format", "{{.OperatingSystem}}"])?.trim() == "Docker Desktop")
}

pub(super) fn engine() -> Result<String> {
    let id = command(&["info", "--format", "{{.ID}}"])?;
    if id.trim().is_empty() {
        bail!("Docker engine identity is unavailable");
    }
    Ok(id.trim().to_owned())
}

pub(super) fn inspect(name: &str) -> Result<Option<Value>> {
    // Listing gives an authoritative missing result; transport errors never
    // masquerade as absence (which would incorrectly reclaim a stale lease).
    let names = command(&["container", "ls", "-a", "--format", "{{.Names}}"])?;
    if !names.lines().any(|line| line == name) {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&command(&["inspect", name])?)?;
    Ok(Some(value[0].clone()))
}

pub(super) fn inspect_id(id: &str) -> Result<Option<Value>> {
    if id.len() != 64 || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("lease has an invalid container identity; preserve it for operator recovery");
    }
    let ids = command(&["container", "ls", "-aq", "--no-trunc"])?;
    if !ids.lines().any(|candidate| candidate == id) {
        return Ok(None);
    }
    let value: Value = serde_json::from_str(&command(&["inspect", id])?)?;
    Ok(Some(value[0].clone()))
}

pub(super) fn validate(config: &Config, state: &Value) -> Result<()> {
    validate_settings(config, state)?;
    validate_volume(config)?;
    validate_ownership(config)
}

fn validate_ownership(config: &Config) -> Result<()> {
    let mut sources = vec![
        config.volume.clone(),
        config.profile.to_string_lossy().into_owned(),
    ];
    if config.docker_desktop && cfg!(target_os = "macos") {
        sources.push(format!("/host_mnt{}", config.profile.display()));
    }
    for source in sources {
        let peers = command(&[
            "ps",
            "-a",
            "--filter",
            &format!("volume={source}"),
            "--format",
            "{{.Names}}",
        ])?;
        if peers.lines().any(|peer| peer != config.container) {
            bail!("private workspace or account profile is attached to another container; refuse shared ownership");
        }
    }

    Ok(())
}

pub(super) fn validate_settings(config: &Config, state: &Value) -> Result<()> {
    let hc = &state["HostConfig"];
    let labels = &state["Config"]["Labels"];
    if labels["loom.workspace-mode"] != MODE
        || labels["loom.account"] != config.account
        || labels["loom.repository"] != config.repository
        || labels["loom.workspace"] != REPO
    {
        bail!("session workspace/account/repository identity differs; stop the idle old session before migrating (never reuse or reset it)");
    }
    if hc["Privileged"] != false
        || hc["ReadonlyRootfs"] != true
        || hc["PidMode"] == "host"
        || hc["IpcMode"] == "host"
        || hc["NetworkMode"] == "host"
        || hc["CapAdd"].as_array().is_some_and(|a| !a.is_empty())
        || hc["Devices"].as_array().is_some_and(|a| !a.is_empty())
        || hc["DeviceRequests"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        || !hc["CapDrop"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v == "ALL"))
        || !hc["SecurityOpt"].as_array().is_some_and(|a| {
            a.iter()
                .any(|v| v == "no-new-privileges" || v == "no-new-privileges:true")
        })
        || state["Config"]["User"] != "1000:1000"
        || state["Config"]["Entrypoint"] != json!(["/usr/bin/tini"])
        || state["Config"]["Cmd"] != json!(["--", "/bin/sleep", "infinity"])
    {
        bail!("private session containment settings differ; refuse reuse and recreate the idle container");
    }
    let mounts = state["Mounts"]
        .as_array()
        .context("missing actual mount inventory")?;
    // Workspace volume + profile bind + one read-only bind per profile control
    // file (+ the optional forge configuration). Because every destination is
    // required to be distinct AND to match one of those exact roles, an exact
    // count is also a proof that none of them is MISSING — which is what makes
    // this the host-side half of the control-file protection (issue #8839).
    let expected = 2
        + bundle::PROFILE_CONTROLS.len()
        + usize::from(config.gh_config.is_some())
        + config.extra_mounts.len();
    if mounts.len() != expected {
        bail!("private session has unexpected mounts; no host workspace, peer volume or runtime socket may be mounted, and every account profile control file must be bound read-only");
    }
    let mut seen = std::collections::HashSet::new();
    for mount in mounts {
        let destination = mount["Destination"]
            .as_str()
            .context("invalid mount destination")?;
        if !seen.insert(destination) {
            bail!("duplicate private session mount");
        }
        let valid = match destination {
            ROOT => {
                mount["Type"] == "volume" && mount["Name"] == config.volume && mount["RW"] == true
            }
            PROFILE => {
                mount["Type"] == "bind"
                    && bind_source_matches(config, mount, &config.profile)
                    && mount["RW"] == true
            }
            GH_CONFIG => config.gh_config.as_ref().is_some_and(|path| {
                mount["Type"] == "bind"
                    && bind_source_matches(config, mount, path)
                    && mount["RW"] == false
            }),
            // The hook registration, Codex's trust state and Loom's readiness
            // receipt, each bound READ-ONLY over its own canonical path. The
            // mount point is what makes them unremovable and unrenameable from
            // inside the session while `auth.json` beside them stays writable.
            other => {
                bundle::PROFILE_CONTROLS.iter().any(|name| {
                    other == bundle::control_destination(name)
                        && mount["Type"] == "bind"
                        && bind_source_matches(config, mount, &config.profile.join(name))
                        && mount["RW"] == false
                })
                // The recorded `accounts.session.extraMounts` (#14451): data
                // only, so each must be a read-only bind of exactly its
                // recorded host path. One that is not recorded, or recorded
                // and missing, breaks the exact count above.
                || config.extra_mounts.iter().any(|extra| {
                    Path::new(other) == extra.container
                        && mount["Type"] == "bind"
                        && bind_source_matches(config, mount, &extra.host)
                        && mount["RW"] == false
                })
            }
        };
        if !valid {
            bail!("private session mount does not match its account-owned configuration");
        }
    }

    Ok(())
}

/// The read-only control-file binds a private session is created with. One
/// source of truth with `validate_settings` above, so a session can never be
/// created in a shape its own validation would accept while unprotected.
fn control_mounts(config: &Config) -> Vec<String> {
    bundle::PROFILE_CONTROLS
        .iter()
        .map(|name| {
            format!(
                "type=bind,src={},dst={},readonly",
                config.profile.join(name).display(),
                bundle::control_destination(name)
            )
        })
        .collect()
}

/// Whether a live session is provably idle, alone. Every other caller wants
/// the refusal's own clause too, so it goes through [`idle_refusal`].
pub(super) fn idle(state: Option<&Value>, container: &str) -> Result<bool> {
    Ok(idle_refusal(state, container)?.is_none())
}

/// Why a live session is NOT provably idle, or `None` when it is. The clause
/// is one line of safe fields only — for a blocking container exec, its id
/// and its classified state, with an explicit unknown when inspection could
/// not answer — and it rides the stale-lease refusal on both the admission
/// path and `roles draw --explain` verbatim (#14943).
pub(super) fn idle_refusal(state: Option<&Value>, container: &str) -> Result<Option<String>> {
    let Some(state) = state else { return Ok(None) };
    if state["State"]["Running"] == false {
        return Ok(None);
    }
    if state["State"]["Paused"] == true {
        return Ok(Some("session container is paused".into()));
    }
    if state["State"]["Running"] != true {
        return Ok(Some("session container running state is unverified".into()));
    }
    // Docker never garbage-collects an exec record that was created but never
    // started, so a non-empty `ExecIDs` is NOT by itself a busy proof: each
    // record is inspected, and only a live, pending or unresolvable one
    // holds the seat (#14943). All leased exec creators (the snapshot
    // monitor, transport) hold the same account flock this proof runs under,
    // so an ignorable record has no live creator left; an unleased hand exec
    // that starts after this check is the same race `docker top` already has
    // against it, and its process still fails the baseline check below.
    if let Some(ids) = state["ExecIDs"].as_array() {
        let container_id = state["Id"].as_str().unwrap_or_default();
        for id in ids.iter().filter_map(Value::as_str) {
            if let Some(refusal) = exec_record(id, container_id).refusal(id) {
                return Ok(Some(refusal));
            }
        }
    }
    let output = command(&["top", container, "-o", "pid,args"])?;
    let processes: Vec<_> = output
        .lines()
        .skip(1)
        .map(str::trim)
        .map(|line| {
            line.split_once(char::is_whitespace)
                .map_or("", |(_, command)| command.trim())
        })
        .collect();
    // Private sessions deliberately have no tmux/baseline interactive shell.
    // Every extra process, including a detached child, is busy/ambiguous.
    Ok((processes.len() != 2
        || !processes.contains(&"/usr/bin/tini -- /bin/sleep infinity")
        || !processes.contains(&"/bin/sleep infinity"))
    .then(|| "session process list is not the idle baseline".to_owned()))
}

/// The Docker Engine API version the exec probe pins: old enough for every
/// engine this daemon runs against, and `/exec/{id}/json` has reported
/// `Running`, `ExitCode`, `Pid` and `ContainerID` since long before it.
const EXEC_API_VERSION: &str = "1.43";
/// Wall budget for one exec-inspect request: a read-only call against a
/// local daemon, bounded so a wedged engine cannot hold the account lease.
const EXEC_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// An exec-inspect body is a few hundred bytes; anything larger is not a
/// body this probe can trust.
const EXEC_PROBE_BODY_LIMIT: usize = 64 * 1024;

/// What one Docker exec record proves about session busyness.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ExecRecord {
    /// `Running: true` — a live exec worker.
    Running,
    /// Started and finished (`ExitCode` set). Docker prunes finished records
    /// from `ExecIDs` on its own; this covers the prune race.
    Exited,
    /// Created but never started (`ExitCode` null, `Pid` 0) — the record a
    /// snapshot exec leaves behind when its creating client died between
    /// create and start and the start never came (#14943).
    NeverStarted,
    /// Not running and not exited, with a pid attached: neither idle-proof
    /// nor dead, so it holds the seat.
    Pending,
    /// Inspect answered for a different container than the one being proven.
    Foreign,
    /// Inspect could not answer or did not classify: an explicit unknown,
    /// carrying the sanitized category for the refusal text.
    Unknown(&'static str),
}

impl ExecRecord {
    /// The blocking record's clause for [`idle_refusal`], or `None` when the
    /// record is ignorable. Safe fields only: the exec id and its classified
    /// state — never the exec's command, arguments or environment (#14943).
    fn refusal(&self, id: &str) -> Option<String> {
        match self {
            Self::Running => Some(format!("container exec {id} is running")),
            Self::Pending => {
                Some(format!("container exec {id} has not started and has not exited"))
            }
            Self::Foreign => Some(format!("container exec {id} belongs to another container")),
            Self::Unknown(why) => Some(format!("container exec {id} state unknown ({why})")),
            Self::Exited | Self::NeverStarted => None,
        }
    }
}

/// Classify one exec record of `container_id` from the engine that reports
/// it. Anything the probe cannot resolve classifies as unknown and keeps the
/// session busy — a stranded seat is recoverable by hand, a guessed-at idle
/// proof is not.
fn exec_record(id: &str, container_id: &str) -> ExecRecord {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return ExecRecord::Unknown("exec id is malformed");
    }
    match exec_inspect_body(id) {
        Ok(body) if body.len() <= EXEC_PROBE_BODY_LIMIT => classify_exec(&body, container_id),
        Ok(_) => ExecRecord::Unknown("exec inspect response was oversized"),
        Err(error) => {
            if error.downcast_ref::<NoExecProbeTransport>().is_some() {
                ExecRecord::Unknown("engine endpoint has no exec probe transport")
            } else {
                ExecRecord::Unknown("exec inspect failed")
            }
        }
    }
}

/// Marker: the configured engine endpoint uses a scheme this probe cannot
/// open (ssh, TLS) — distinct from a transport that tried and failed.
#[derive(Debug)]
struct NoExecProbeTransport;

impl std::fmt::Display for NoExecProbeTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("engine endpoint has no exec probe transport")
    }
}

impl std::error::Error for NoExecProbeTransport {}

/// The exec record's JSON body straight from the Engine API. Tests intercept
/// the pseudo-argv `exec-inspect <id>` at the shared fake seam, so no test
/// touches a real engine (#12772).
fn exec_inspect_body(id: &str) -> Result<String> {
    #[cfg(test)]
    if let Some(result) = fake::dispatch(&["exec-inspect", id], &[], EXEC_PROBE_TIMEOUT) {
        return result.map(|(_, body)| body);
    }
    let endpoint = engine_endpoint().ok_or_else(|| anyhow::anyhow!(NoExecProbeTransport))?;
    engine_api_get(&endpoint, &format!("/v{EXEC_API_VERSION}/exec/{id}/json"))
        .map_err(|error| error.context("engine exec inspect failed"))
}

/// Classify an `/exec/{id}/json` body against the container being proven.
fn classify_exec(body: &str, container_id: &str) -> ExecRecord {
    let Ok(value) = serde_json::from_str::<Value>(body) else {
        return ExecRecord::Unknown("exec state unrecognized");
    };
    let classified = match (value["Running"].as_bool(), value["ExitCode"].as_i64()) {
        (Some(true), _) => ExecRecord::Running,
        (_, Some(_)) => ExecRecord::Exited,
        (Some(false), _) if value["Pid"] == json!(0) => ExecRecord::NeverStarted,
        (Some(false), _) => ExecRecord::Pending,
        _ => return ExecRecord::Unknown("exec state unrecognized"),
    };
    match value["ContainerID"].as_str() {
        Some(recorded) if recorded == container_id => classified,
        // A record answered for another container is not this proof's to
        // use; one that names no container at all did not classify.
        Some(_) => ExecRecord::Foreign,
        None => ExecRecord::Unknown("exec state unrecognized"),
    }
}

/// The engine endpoint the `docker` CLI itself would use, with the CLI's own
/// precedence — `DOCKER_HOST` first, then the current context's endpoint,
/// then the platform default — so the probe speaks to the same engine that
/// supplied the container inventory. Schemes without an openable transport
/// here (ssh, TLS) answer `None`: the exec stays unverified and the seat is
/// preserved, never guessed at.
fn engine_endpoint() -> Option<EngineEndpoint> {
    if let Some(host) = std::env::var_os("DOCKER_HOST") {
        if !host.is_empty() {
            return parse_docker_host(&host.to_string_lossy());
        }
    }
    if let Ok(current) = command(&[
        "context",
        "inspect",
        "--format",
        "{{.Endpoints.docker.Host}}",
    ]) {
        let host = current.trim();
        if !host.is_empty() {
            return parse_docker_host(host);
        }
    }
    default_endpoint()
}

/// Where a raw engine probe can be opened.
#[derive(Debug, PartialEq, Eq)]
enum EngineEndpoint {
    Unix(PathBuf),
    Tcp(String),
}

/// Parse a `DOCKER_HOST`-shaped endpoint URI into an openable transport.
fn parse_docker_host(host: &str) -> Option<EngineEndpoint> {
    let (scheme, rest) = host.split_once("://")?;
    match scheme {
        "unix" => Some(EngineEndpoint::Unix(if rest.is_empty() {
            PathBuf::from(default_socket())
        } else {
            PathBuf::from(rest)
        })),
        "tcp" | "http"
            if std::env::var_os("DOCKER_TLS_VERIFY").is_none_or(|value| value.is_empty()) =>
        {
            (!rest.is_empty()).then(|| EngineEndpoint::Tcp(rest.to_owned()))
        }
        _ => None,
    }
}

/// A default-socket candidate: HOME-relative (resolved under `$HOME` when
/// it is set, skipped when it is not) or absolute (used verbatim).
#[derive(Debug, PartialEq, Eq)]
enum SocketCandidate {
    HomeRelative(&'static str),
    Absolute(&'static str),
}

/// The platform's default engine socket candidates, in probe order.
fn default_socket_candidates() -> &'static [SocketCandidate] {
    if cfg!(target_os = "macos") {
        &[
            // Docker Desktop's socket lives under the user's home.
            SocketCandidate::HomeRelative(".docker/run/docker.sock"),
            // Docker Desktop also keeps a symlink at the Unix path on macOS.
            SocketCandidate::Absolute("/var/run/docker.sock"),
        ]
    } else {
        &[SocketCandidate::Absolute("/var/run/docker.sock")]
    }
}

/// Map candidates plus a `$HOME` value to the paths probed, in order.
fn resolve_socket_candidates(candidates: &[SocketCandidate], home: Option<&Path>) -> Vec<PathBuf> {
    candidates
        .iter()
        .filter_map(|candidate| match candidate {
            SocketCandidate::HomeRelative(suffix) => home.map(|home| home.join(suffix)),
            SocketCandidate::Absolute(path) => Some(PathBuf::from(path)),
        })
        .collect()
}

/// The platform's default engine socket, in probe order.
fn default_endpoint() -> Option<EngineEndpoint> {
    let home = std::env::var_os("HOME");
    resolve_socket_candidates(default_socket_candidates(), home.as_deref().map(Path::new))
        .into_iter()
        .find(|path| std::fs::symlink_metadata(path).is_ok())
        .map(EngineEndpoint::Unix)
}

/// The first default socket path for this platform, without an existence
/// check (`unix://` with no path resolves to it).
fn default_socket() -> &'static str {
    // Docker Desktop keeps a symlink at the Unix path on macOS too.
    "/var/run/docker.sock"
}

/// One bounded read-only GET against the Docker Engine API, spoken directly
/// over the endpoint the `docker` CLI resolves. HTTP/1.1 with
/// `Connection: close` is the whole contract the daemon's JSON replies need.
fn engine_api_get(endpoint: &EngineEndpoint, path: &str) -> Result<String> {
    match endpoint {
        EngineEndpoint::Unix(socket) => {
            let mut stream = UnixStream::connect(socket)
                .with_context(|| format!("engine socket {} unreachable", socket.display()))?;
            stream.set_read_timeout(Some(EXEC_PROBE_TIMEOUT))?;
            stream.set_write_timeout(Some(EXEC_PROBE_TIMEOUT))?;
            engine_request(&mut stream, path)
        }
        EngineEndpoint::Tcp(host) => {
            let address = host
                .to_socket_addrs()
                .with_context(|| format!("engine endpoint {host} did not resolve"))?
                .next()
                .context("engine endpoint resolved to no address")?;
            let mut stream = TcpStream::connect_timeout(&address, EXEC_PROBE_TIMEOUT)
                .with_context(|| format!("engine endpoint {host} unreachable"))?;
            stream.set_read_timeout(Some(EXEC_PROBE_TIMEOUT))?;
            stream.set_write_timeout(Some(EXEC_PROBE_TIMEOUT))?;
            engine_request(&mut stream, path)
        }
    }
}

/// Write one GET and read the reply to the connection close, within the
/// probe's budgets.
fn engine_request(stream: &mut (impl Read + Write), path: &str) -> Result<String> {
    stream.write_all(
        format!("GET {path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\r\n").as_bytes(),
    )?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = stream.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if bytes.len() + read > EXEC_PROBE_BODY_LIMIT * 2 {
            bail!("engine response exceeds the probe's body budget");
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    parse_http_body(&bytes)
}

/// The body of an engine HTTP reply: 200 only, framed by `Content-Length`,
/// chunked encoding, or the connection close itself.
fn parse_http_body(response: &[u8]) -> Result<String> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("engine response has no header terminator")?;
    let head = std::str::from_utf8(&response[..header_end])
        .context("engine response header is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let status = lines.next().unwrap_or_default();
    if !status.starts_with("HTTP/1.0 200")
        && !status.starts_with("HTTP/1.1 200")
        && !status.starts_with("HTTP/2 200")
    {
        bail!("engine responded with {status}");
    }
    let mut chunked = false;
    let mut length = None;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                chunked = true;
            }
            "content-length" => length = value.trim().parse::<usize>().ok(),
            _ => {}
        }
    }
    let body = &response[header_end + 4..];
    if chunked {
        return Ok(dechunk(body));
    }
    let body = match length {
        Some(length) => body
            .get(..length)
            .context("engine body shorter than declared")?,
        None => body,
    };
    Ok(String::from_utf8_lossy(body).into_owned())
}

/// Decode a chunked engine reply body.
fn dechunk(body: &[u8]) -> String {
    let mut decoded = Vec::new();
    let mut cursor = 0;
    while let Some(line_end) = body[cursor.min(body.len())..]
        .iter()
        .position(|b| *b == b'\n')
        .map(|at| cursor + at)
    {
        let size_line = &body[cursor..line_end];
        let size_text = String::from_utf8_lossy(size_line);
        let size_text = size_text.trim().split(';').next().unwrap_or_default();
        let Ok(size) = usize::from_str_radix(size_text, 16) else {
            break;
        };
        cursor = line_end + 1;
        if size == 0 {
            break;
        }
        let Some(chunk) = body.get(cursor..cursor + size) else {
            break;
        };
        decoded.extend_from_slice(chunk);
        cursor += size + 2;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

pub(super) fn volume_exists(volume: &str) -> Result<bool> {
    let volumes = command(&["volume", "ls", "--format", "{{.Name}}"])?;
    Ok(volumes.lines().any(|name| name == volume))
}

/// The recorded image must already be local: recovery never pulls or swaps one.
pub(super) fn image_present(image: &str) -> Result<()> {
    image_present_with(image, docker_cli::DOCKER_CALL_TIMEOUT)
}

pub(super) fn image_present_with(image: &str, timeout: Duration) -> Result<()> {
    command_with(&["image", "inspect", "--format", "{{.Id}}", image], timeout)
        .map(drop)
        .with_context(|| format!("recorded session image {image} is not available locally"))
}

pub(super) fn create(config: &Config, image: &str) -> Result<()> {
    let volumes = command(&["volume", "ls", "--format", "{{.Name}}"])?;
    if volumes.lines().any(|name| name == config.volume) {
        validate_volume(config)?;
    } else {
        command(&[
            "volume",
            "create",
            "--label",
            &format!("loom.account={}", config.account),
            "--label",
            &format!("loom.repository={}", config.repository),
            "--label",
            &format!("loom.profile={}", config.profile.display()),
            &config.volume,
        ])?;
        // Initialize only a freshly created empty volume. Never recursively
        // chown or erase a reused volume that may contain unfinished work.
        command(&[
            "run",
            "--rm",
            "--network",
            "none",
            "--user",
            "0",
            "--cap-drop",
            "ALL",
            "--cap-add",
            "CHOWN",
            "--security-opt",
            "no-new-privileges",
            "--mount",
            &format!("type=volume,src={},dst={ROOT}", config.volume),
            "--entrypoint",
            "/bin/chown",
            image,
            "1000:1000",
            ROOT,
        ])?;
    }
    validate_ownership(config)?;
    // `--restart unless-stopped` (#14912): on a session-only host no daemon
    // runs the reconcile pass, so Docker's own policy is what returns the
    // seat after an engine restart or host reboot — the same policy the
    // host-mounted seat has carried since #10452. It never fights an
    // operator's stop: `stop` removes the container, and the `--rm` helper
    // runs above and below stay ephemeral with no policy.
    let mut args = vec![
        "run".to_owned(),
        "-d".into(),
        "--name".into(),
        config.container.clone(),
        "--restart".into(),
        "unless-stopped".into(),
        "--user".into(),
        "1000:1000".into(),
        "--read-only".into(),
        "--cap-drop".into(),
        "ALL".into(),
        "--security-opt".into(),
        "no-new-privileges".into(),
        "--tmpfs".into(),
        "/tmp:rw,nosuid,nodev,size=512m".into(),
        "--mount".into(),
        format!("type=volume,src={},dst={ROOT}", config.volume),
        "--mount".into(),
        format!("type=bind,src={},dst={PROFILE}", config.profile.display()),
        "--label".into(),
        format!("loom.workspace-mode={MODE}"),
        "--label".into(),
        format!("loom.workspace={REPO}"),
        "--label".into(),
        format!("loom.account={}", config.account),
        "--label".into(),
        format!("loom.repository={}", config.repository),
    ];
    // Read-only binds over the profile's own control files. They must already
    // exist on the host: `provision_controls` below is what guarantees that,
    // and it runs before this in a container that does NOT carry these binds
    // (writing them from inside a session is exactly what they prevent).
    for mount in control_mounts(config) {
        args.extend(["--mount".to_owned(), mount]);
    }
    for value in [
        format!("CODEX_HOME={PROFILE}"),
        "XDG_CACHE_HOME=/workspace/cache/xdg".into(),
        "CARGO_HOME=/workspace/cache/cargo".into(),
        "CARGO_TARGET_DIR=/workspace/cache/target".into(),
        "npm_config_cache=/workspace/cache/npm".into(),
        // `.loom/lease-renew/.lock` has been observed untracked in seat clones
        // (#14160); the writer is not traced (#14226). It is not this
        // container's `$HOME`, which is `/home/loom` here. Point the store at
        // the cache volume so Loom's own lock stays out of the work tree.
        "LOOM_LEASE_RENEW_STATE_DIR=/workspace/cache/lease-renew".into(),
        // The seat's 512 MB `/tmp` tmpfs filled with caches that live for the
        // container's whole multi-day life (#14929): uv's cache alone was
        // 395 MB after two days. Pin every long-lived cache and Loom state
        // dir to the persistent cache volume (each consumer creates its dir
        // on demand, so a seat created before this change needs no migration).
        // `UV_PROJECT_ENVIRONMENT` stays on /tmp deliberately: it is one
        // per-container venv uv recreates on repo switch, not a growing cache.
        "UV_CACHE_DIR=/workspace/cache/uv".into(),
        "GH_CACHE_DIR=/workspace/cache/gh".into(),
        "LOOM_LISTING_CACHE_DIR=/workspace/cache/forge-listing".into(),
        "LOOM_FORGE_CALL_STATS_DIR=/workspace/cache/forge-call-stats".into(),
        "LOOM_WRITE_SCOPE_CACHE_DIR=/workspace/cache/write-scope".into(),
        format!("LOOM_PRIVATE_REPOSITORY={}", config.repository),
    ] {
        args.extend(["--env".into(), value]);
    }
    if let Some(path) = &config.gh_config {
        args.extend([
            "--mount".into(),
            format!("type=bind,src={},dst={GH_CONFIG},readonly", path.display()),
            "--env".into(),
            format!("GH_CONFIG_DIR={GH_CONFIG}"),
        ]);
    }
    // The configured extra mounts, always read-only (#14451). Their JSON is
    // labelled only when there are any, so an unconfigured create is
    // byte-identical to what it has always been.
    for extra in &config.extra_mounts {
        args.extend([
            "--mount".into(),
            format!(
                "type=bind,src={},dst={},readonly",
                extra.host.display(),
                extra.container.display()
            ),
        ]);
    }
    if !config.extra_mounts.is_empty() {
        args.extend([
            "--label".into(),
            format!(
                "{}={}",
                super::super::session_lifecycle::EXTRA_MOUNTS_LABEL,
                super::super::session_image::extra_mounts_label_json(&config.extra_mounts)
            ),
        ]);
    }
    args.extend([
        "--entrypoint".into(),
        "/usr/bin/tini".into(),
        image.into(),
        "--".into(),
        "/bin/sleep".into(),
        "infinity".into(),
    ]);
    command(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    Ok(())
}

/// The sealed-bundle probe of [`super::super::session_image::verify_override_bundle`]
/// routed through [`command_with`], so it shares this module's engine seam (and its
/// test fake) rather than spawning its own `docker`.
struct ImageProbe(Duration);

impl super::super::session_image::BundleProbe for ImageProbe {
    fn run_image_script(&self, image: &str, script: &str) -> Result<String> {
        command_with(
            &[
                "run",
                "--rm",
                "--network",
                "none",
                "--read-only",
                "--cap-drop",
                "ALL",
                "--security-opt",
                "no-new-privileges",
                "--entrypoint",
                "/bin/sh",
                image,
                "-c",
                script,
            ],
            self.0,
        )
    }
}

/// Prove the configured override `image` carries the sealed control bundle
/// this daemon enforces, before any container is created from it (#14451).
pub(super) fn verify_image(image: &str) -> Result<()> {
    verify_image_with(image, docker_cli::DOCKER_RUN_TIMEOUT)
}

pub(super) fn verify_image_with(image: &str, timeout: Duration) -> Result<()> {
    super::super::session_image::verify_override_bundle(image, &ImageProbe(timeout))
        .context("refusing to create a private session from the configured override image")
}

pub(super) fn setup(config: &Config) -> Result<()> {
    let output = command_with(
        &[
            "exec",
            &config.container,
            "loom-daemon",
            "private-workspace",
            "setup",
        ],
        EXEC_TIMEOUT,
    )?;
    report(&output)
}

/// Materialize the account profile's control files — the managed hook
/// registration, Codex's own trust-state file and Loom's readiness receipt —
/// with the image-owned, sealed provisioner, **before** the session container
/// exists.
///
/// This cannot happen inside the session: the session binds those three files
/// read-only, which is the entire protection, so a write there is `EROFS` by
/// design. It therefore runs in a throwaway container from the same image,
/// carrying the account profile and nothing else — no workspace volume, no
/// forge configuration, no network — so the only thing it can touch is the
/// profile Loom already owns. `auth.json` is never read, written or copied by
/// any of it.
pub(super) fn provision_controls(config: &Config, image: &str) -> Result<()> {
    let output = command(&[
        "run",
        "--rm",
        "--network",
        "none",
        "--user",
        "1000:1000",
        "--read-only",
        "--cap-drop",
        "ALL",
        "--security-opt",
        "no-new-privileges",
        "--tmpfs",
        "/tmp:rw,nosuid,nodev,size=64m",
        "--mount",
        &format!("type=bind,src={},dst={PROFILE}", config.profile.display()),
        "--env",
        &format!("CODEX_HOME={PROFILE}"),
        "--entrypoint",
        "loom-daemon",
        image,
        "private-workspace",
        "provision-controls",
    ]).context("private control provisioning endpoint unavailable; use a session image that ships the versioned control bundle")?;
    report(&output)
}

fn report(output: &str) -> Result<()> {
    let report: worker_setup::SetupReport = serde_json::from_str(output)
        .context("private setup endpoint unavailable; update the session image")?;
    if report.protocol != PROTOCOL {
        bail!("private setup protocol mismatch; update the session image");
    }
    match report.status {
        worker_setup::SetupStatus::Ready => Ok(()),
        worker_setup::SetupStatus::ProfileInaccessible => bail!("private account profile is not readable and writable by session UID 1000; check external profile ownership and permissions (profile was preserved)"),
        worker_setup::SetupStatus::ControlMissing => bail!("private session image ships no {} control bundle, so the account profile's hook registration cannot be established from a sealed provisioner; guard code and effective policy would live in worker-writable paths", bundle::CONTROL_PROTOCOL),
        worker_setup::SetupStatus::Failed => bail!("private helper/configuration setup failed; inspect the owned account locally (configuration was preserved)"),
    }
}

/// Take the worker's own observation of the control boundary. Addressed by the
/// container's immutable ID, never by its mutable Docker name: the boundary is
/// bound to one container identity, and a rename between the identity check and
/// this probe must not let a different worker answer for it.
fn observe_control(id: &str) -> Result<bundle::Report> {
    let output = command(&["exec", id, "loom-daemon", "private-workspace", "control"])
        .context("private control endpoint unavailable; use a session image that ships the versioned control bundle")?;
    serde_json::from_str(&output)
        .context("private control endpoint is not the supported protocol; update the session image")
}

/// Verify the session's versioned control boundary and return the identity to
/// bind to the account lease. Every claim in the report is re-derived inside
/// the container from the image-owned bundle, and its view of the account
/// profile is cross-checked here against the host's own read of the canonical
/// profile — a container label or environment variable would prove nothing
/// about either.
pub(super) fn control(config: &Config, id: &str) -> Result<String> {
    bundle::accept(&observe_control(id)?, &config.profile)
}

/// Recheck a bound control identity immediately before mutable work, and hand
/// back the observation it was proven against. The report carries the one
/// boundary fact identity alone cannot be read back out of — whether this
/// clone has a managed registration at all (`Report::managed`) — which
/// containment admission (#8787) requires; it is part of the hashed identity,
/// so it cannot drift from what was bound.
pub(super) fn recheck_control(config: &Config, id: &str, bound: &str) -> Result<bundle::Report> {
    let report = observe_control(id)?;
    bundle::rebind(&report, &config.profile, bound)?;
    Ok(report)
}

pub(super) fn prepare(
    config: &Config,
    workspace: &Path,
    id: &str,
    branch: Option<&str>,
) -> Result<String> {
    prepare_target(config, workspace, &config.primary(), id, branch)
}

/// Prepare one repository of the account: the primary, or a lazily cloned
/// additional one. Credentials stay scoped to THAT repository's forge via the
/// exec's own `LOOM_PRIVATE_REPOSITORY`, never widened on the container.
pub(super) fn prepare_target(
    config: &Config,
    workspace: &Path,
    target: &Target,
    id: &str,
    branch: Option<&str>,
) -> Result<String> {
    // Resolved per exec (#13916): an App installation token outlives no job.
    let auth = forge_auth::ExecAuth::resolve(workspace, &target.repository);
    prepare_with(config, target, id, branch, &auth)
}

/// Drive one clone's `private-workspace recover` verb under the job's auth
/// context (#14030): the exec carries the same repository-scoped credential
/// env as preparation, so a publish pushes only through that clone's helper.
pub(super) fn recover_target(
    config: &Config,
    target: &Target,
    action: RecoverAction,
    issue: Option<u64>,
    branches: &[String],
    worktrees: &[PathBuf],
    auth: &forge_auth::ExecAuth,
) -> Result<RecoverOutcome> {
    let scope = format!("LOOM_PRIVATE_REPOSITORY={}", target.repository);
    let issue_flag = issue.map(|n| n.to_string());
    let mut args = vec!["exec", "--workdir", ROOT, "--env", &scope];
    for name in auth.names() {
        args.extend(["--env", name]);
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "recover",
        "--account",
        &config.account,
        "--repository",
        &target.repository,
        "--base",
        &target.base,
    ]);
    match action {
        RecoverAction::Publish => args.extend(["--publish"]),
        RecoverAction::Discard => args.push("--discard"),
        RecoverAction::ValidateWorktrees => args.extend(["--discard", "--validate-worktrees"]),
        RecoverAction::List => {}
    }
    if let Some(issue) = &issue_flag {
        args.extend(["--issue", issue]);
    }
    for branch in branches {
        args.extend(["--branch", branch]);
    }
    let worktree_flags: Vec<String> = worktrees.iter().map(|w| w.display().to_string()).collect();
    for worktree in &worktree_flags {
        args.extend(["--worktree", worktree]);
    }
    if let Some(slug) = &target.slug {
        args.extend(["--slug", slug]);
    }
    let output = command_env(&args, &auth.child_env(), EXEC_TIMEOUT).context(
        "private workspace endpoint failed; use a current session image supporting private-workspace protocol",
    )?;
    let result: Value = serde_json::from_str(&output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private clone recovery refused: {error}");
    }
    serde_json::from_value(result["outcome"].clone())
        .context("private workspace recover returned an invalid report")
}

/// Drive the in-volume `private-workspace seat-reclaim` verb (#14678) under
/// the credential scope of the account's primary repository — the same exec
/// shape as [`recover_target`] with **no preparation step**: the verb must
/// run in a dirty seat too, so nothing here calls [`prepare_target`].
pub(super) fn seat_reclaim(
    config: &Config,
    max_age_secs: i64,
    grace_secs: i64,
    auth: &forge_auth::ExecAuth,
) -> Result<super::seat_reclaim::SeatReclaimOutcome> {
    let scope = format!("LOOM_PRIVATE_REPOSITORY={}", config.repository);
    let max_age = max_age_secs.to_string();
    let grace = grace_secs.to_string();
    let mut args = vec!["exec", "--workdir", ROOT, "--env", &scope];
    for name in auth.names() {
        args.extend(["--env", name]);
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "seat-reclaim",
        "--account",
        &config.account,
        "--max-age-secs",
        &max_age,
        "--grace-secs",
        &grace,
    ]);
    let output = command_env(&args, &auth.child_env(), EXEC_TIMEOUT).context(
        "private workspace endpoint failed; use a current session image supporting private-workspace protocol",
    )?;
    let result: Value = serde_json::from_str(&output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private clone seat reclaim refused: {error}");
    }
    serde_json::from_value(result["outcome"].clone())
        .context("private workspace seat-reclaim returned an invalid report")
}

/// Drive the in-volume `private-workspace rebind` verb (#14357) under the
/// credential scope of the CANONICAL repository.
pub(super) fn rebind_target(
    config: &Config,
    from: &Registered,
    to: &RepoId,
    dry_run: bool,
    auth: &forge_auth::ExecAuth,
) -> Result<RebindOutcome> {
    let scope = format!("LOOM_PRIVATE_REPOSITORY={}", to.url);
    let mut args = vec!["exec", "--workdir", ROOT, "--env", &scope];
    for name in auth.names() {
        args.extend(["--env", name]);
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "rebind",
        "--account",
        &config.account,
        "--from",
        &from.repository,
        "--from-slug",
        &from.slug,
        "--to",
        &to.url,
        "--to-slug",
        &to.slug,
        "--base",
        &from.base,
    ]);
    if dry_run {
        args.push("--dry-run");
    }
    let output = command_env(&args, &auth.child_env(), EXEC_TIMEOUT).context(
        "private workspace endpoint failed; use a current session image supporting private-workspace protocol",
    )?;
    let result: Value = serde_json::from_str(&output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private clone rebind refused: {error}");
    }
    serde_json::from_value(result["outcome"].clone())
        .context("private workspace rebind returned an invalid report")
}

pub(super) fn prepare_with(
    config: &Config,
    target: &Target,
    id: &str,
    branch: Option<&str>,
    auth: &forge_auth::ExecAuth,
) -> Result<String> {
    let scope = format!("LOOM_PRIVATE_REPOSITORY={}", target.repository);
    let mut args = vec!["exec", "--workdir", ROOT, "--env", &scope];
    // `--env NAME` copies the `docker` client's value directly: the host
    // process's own, or one this host resolved and set on that child alone
    // (#13916). Secrets never appear in argv, config files, remote URLs or
    // our captured output.
    for name in auth.names() {
        args.extend(["--env", name]);
    }
    args.extend([
        &config.container,
        "loom-daemon",
        "private-workspace",
        "prepare",
        "--account",
        &config.account,
        "--repository",
        &target.repository,
        "--base",
        &target.base,
        "--container-id",
        id,
    ]);
    if let Some(branch) = branch {
        args.extend(["--branch", branch]);
    }
    if let Some(slug) = &target.slug {
        args.extend(["--slug", slug]);
    }
    let output = command_env(&args, &auth.child_env(), EXEC_TIMEOUT).context("private workspace endpoint failed; use a current session image supporting private-workspace protocol")?;
    prepare_result(&output, |path| {
        log::info!(
            "private session {}: reaped finished-PR worktree {path} in {}",
            config.account,
            target.repository
        );
    })
}

fn prepare_result(output: &str, mut report_reaped: impl FnMut(&str)) -> Result<String> {
    let result: Value = serde_json::from_str(output)
        .context("private workspace endpoint unavailable; update the session image before reuse")?;
    if result["protocol"] != PROTOCOL {
        bail!("private workspace endpoint protocol mismatch; update session image");
    }
    // Optional since the reap-reporting field: an older session image omits
    // it, and a non-array or non-string entry is skipped, never fatal.
    for path in result["reaped_worktrees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        report_reaped(path);
    }
    if let Some(error) = result["error"].as_str() {
        bail!("private workspace preparation refused: {error}");
    }
    result["revision"]
        .as_str()
        .map(str::to_owned)
        .context("private clone did not resolve a base revision")
}

#[cfg(test)]
mod prepare_tests {
    use super::*;

    #[test]
    fn prepare_refusal_reports_paths_before_returning_the_same_error() {
        for paths in [
            Some(vec!["/workspace/finished-a", "/workspace/finished-b"]),
            None,
        ] {
            let mut reply = serde_json::json!({"protocol": PROTOCOL, "error": "seat_dirty"});
            if let Some(paths) = &paths {
                reply["reaped_worktrees"] = serde_json::json!(paths);
            }
            let mut reported = Vec::new();
            let error = prepare_result(&reply.to_string(), |path| reported.push(path.to_owned()))
                .unwrap_err();
            assert_eq!(error.to_string(), "private workspace preparation refused: seat_dirty");
            assert_eq!(reported, paths.unwrap_or_default());
        }
    }
}

#[cfg(test)]
mod exec_probe_tests {
    use super::*;

    #[test]
    fn endpoint_uris_parse_to_openable_transports_only() {
        let _tls = crate::env_test_support::EnvGuard::clear(&["DOCKER_TLS_VERIFY"]);
        assert_eq!(
            parse_docker_host("unix:///var/run/docker.sock"),
            Some(EngineEndpoint::Unix(PathBuf::from("/var/run/docker.sock")))
        );
        assert_eq!(
            parse_docker_host("tcp://127.0.0.1:2375"),
            Some(EngineEndpoint::Tcp("127.0.0.1:2375".into()))
        );
        for unsupported in [
            "ssh://worker.example",
            "https://engine.example:2376",
            "npipe:////./pipe/docker_engine",
            "fd://",
            "garbage",
            "tcp://",
        ] {
            assert_eq!(parse_docker_host(unsupported), None, "{unsupported}");
        }
        // A TLS-verified endpoint is not probeable over plain HTTP either.
        let _tls_on = crate::env_test_support::EnvGuard::set("DOCKER_TLS_VERIFY", "1");
        assert_eq!(parse_docker_host("tcp://127.0.0.1:2375"), None);
    }

    #[test]
    fn default_socket_candidates_resolve_by_home_not_by_stripping() {
        let home = Some(Path::new("/home/probe"));
        // The macOS list: the Docker Desktop socket is HOME-relative, the
        // Unix path stays absolute — never `$HOME/.var/run/docker.sock`
        // (#14958).
        let macos = [
            SocketCandidate::HomeRelative(".docker/run/docker.sock"),
            SocketCandidate::Absolute("/var/run/docker.sock"),
        ];
        assert_eq!(
            resolve_socket_candidates(&macos, home),
            vec![
                PathBuf::from("/home/probe/.docker/run/docker.sock"),
                PathBuf::from("/var/run/docker.sock"),
            ]
        );
        // HOME unset: the HOME-relative candidate cannot resolve and is
        // skipped; the absolute one still probes.
        assert_eq!(
            resolve_socket_candidates(&macos, None),
            vec![PathBuf::from("/var/run/docker.sock")]
        );
        // The Linux list never depends on HOME.
        let linux = [SocketCandidate::Absolute("/var/run/docker.sock")];
        assert_eq!(
            resolve_socket_candidates(&linux, home),
            vec![PathBuf::from("/var/run/docker.sock")]
        );
        assert_eq!(
            resolve_socket_candidates(&linux, None),
            vec![PathBuf::from("/var/run/docker.sock")]
        );
        // The shipped platform list keeps the shape probed above.
        #[cfg(target_os = "macos")]
        assert_eq!(default_socket_candidates(), macos.as_slice());
        #[cfg(not(target_os = "macos"))]
        assert_eq!(default_socket_candidates(), linux.as_slice());
    }

    #[test]
    fn http_bodies_parse_by_length_chunk_or_close() {
        let by_length =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 6\r\nConnection: close\r\n\r\n{\"a\":1";
        assert_eq!(parse_http_body(by_length).unwrap(), "{\"a\":1");
        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\n{\"a\"\r\n3\r\n:1}\r\n0\r\n\r\n";
        assert_eq!(parse_http_body(chunked).unwrap(), "{\"a\":1}");
        let by_close = b"HTTP/1.0 200 OK\r\n\r\n{}";
        assert_eq!(parse_http_body(by_close).unwrap(), "{}");
        for refused in [
            &b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"[..],
            b"HTTP/1.1 500 Internal Server Error\r\n\r\n",
            b"not http at all",
        ] {
            assert!(parse_http_body(refused).is_err(), "{refused:?}");
        }
    }

    #[test]
    fn exec_bodies_classify_into_the_four_states_plus_unknown() {
        let container = "a".repeat(64);
        let id = "b".repeat(64);
        let record = |running: Value, exit: Value, pid: Value| {
            json!({"ID": id, "Running": running, "ExitCode": exit, "Pid": pid,
                   "ContainerID": container, "ProcessConfig": {"entrypoint": "loom-daemon",
                   "arguments": ["private-workspace", "snapshot"]}})
            .to_string()
        };
        // The incident's stranded record: created, never started (#14943).
        assert_eq!(
            classify_exec(&record(json!(false), json!(null), json!(0)), &container),
            ExecRecord::NeverStarted
        );
        assert_eq!(
            classify_exec(&record(json!(false), json!(0), json!(4242)), &container),
            ExecRecord::Exited
        );
        assert_eq!(
            classify_exec(&record(json!(true), json!(null), json!(99)), &container),
            ExecRecord::Running
        );
        // Not running, no exit, a pid attached: neither proof nor dead.
        assert_eq!(
            classify_exec(&record(json!(false), json!(null), json!(99)), &container),
            ExecRecord::Pending
        );
        // A record answered for another container is not this proof's to use.
        assert_eq!(
            classify_exec(&record(json!(true), json!(null), json!(1)), &"c".repeat(64)),
            ExecRecord::Foreign
        );
        for unrecognized in ["{\"Running\": \"yes\"}".to_owned(), "not json".to_owned()] {
            assert_eq!(
                classify_exec(&unrecognized, &container),
                ExecRecord::Unknown("exec state unrecognized"),
                "{unrecognized}"
            );
        }
        // Only the blocking states produce a refusal clause, and the clause
        // never carries the record's arguments (#14943).
        for (record, clause) in [
            (ExecRecord::Running, format!("container exec {id} is running")),
            (
                ExecRecord::Pending,
                format!("container exec {id} has not started and has not exited"),
            ),
            (ExecRecord::Foreign, format!("container exec {id} belongs to another container")),
            (
                ExecRecord::Unknown("exec inspect failed"),
                format!("container exec {id} state unknown (exec inspect failed)"),
            ),
        ] {
            assert_eq!(record.refusal(&id).as_deref(), Some(clause.as_str()));
        }
        assert_eq!(ExecRecord::Exited.refusal(&id), None);
        assert_eq!(ExecRecord::NeverStarted.refusal(&id), None);
    }
}

fn validate_volume(config: &Config) -> Result<()> {
    let existing: Value = serde_json::from_str(&command(&["volume", "inspect", &config.volume])?)?;
    validate_volume_settings(config, &existing[0])
}

pub(super) fn validate_volume_settings(config: &Config, volume: &Value) -> Result<()> {
    if volume["Driver"] != "local"
        || volume["Options"]
            .as_object()
            .is_some_and(|options| !options.is_empty())
        || (!volume["Options"].is_null() && !volume["Options"].is_object())
        || volume["Labels"]["loom.account"] != config.account
        || volume["Labels"]["loom.repository"] != config.repository
        || volume["Labels"]["loom.profile"] != config.profile.to_string_lossy().as_ref()
    {
        bail!("workspace volume must be an ordinary local Docker-managed volume owned by this account; driver/bind options and unknown ownership are refused");
    }
    Ok(())
}

fn bind_source_matches(config: &Config, mount: &Value, expected: &Path) -> bool {
    let expected = expected.to_string_lossy();
    // Desktop rewrites both effective and requested mount sources into its VM
    // namespace. The persisted engine identity was verified at resolution;
    // only that detected engine/platform gets this exact prefix equivalence.
    mount["Source"] == expected.as_ref()
        || (cfg!(target_os = "macos")
            && config.docker_desktop
            && mount["Source"] == format!("/host_mnt{expected}"))
}

/// Test seam: every Docker invocation of this module funnels through
/// [`command`], so a fake installed on the current thread stands in for the
/// whole engine (containers, volumes, images, execs) and no test touches a
/// real Docker (#12772).
#[cfg(test)]
pub(super) mod fake {
    use std::cell::RefCell;
    use std::time::Duration;

    pub(in super::super) type Handler = Box<dyn FnMut(&[&str]) -> anyhow::Result<String>>;
    type TimedHandler = Box<dyn FnMut(&[&str], Duration) -> anyhow::Result<(bool, String)>>;

    thread_local! {
        static HANDLER: RefCell<Option<TimedHandler>> = const { RefCell::new(None) };
        /// The child environment each faked call would have carried (#13916).
        static ENVS: RefCell<Vec<Vec<(String, String)>>> = const { RefCell::new(Vec::new()) };
    }

    /// Every faked call's `docker` child environment, in call order.
    pub(in super::super) fn seen_env() -> Vec<Vec<(String, String)>> {
        ENVS.with(|e| e.borrow().clone())
    }

    /// Route this thread's Docker calls to `handler` until the guard drops.
    pub(in super::super) fn install(mut handler: Handler) -> Guard {
        install_timed(Box::new(move |args, _| handler(args).map(|stdout| (true, stdout))))
    }

    pub(in super::super) fn install_timed(handler: TimedHandler) -> Guard {
        HANDLER.with(|h| *h.borrow_mut() = Some(handler));
        ENVS.with(|e| e.borrow_mut().clear());
        Guard
    }

    pub(in super::super) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            HANDLER.with(|h| *h.borrow_mut() = None);
            ENVS.with(|e| e.borrow_mut().clear());
        }
    }

    pub(super) fn dispatch(
        args: &[&str],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Option<anyhow::Result<(bool, String)>> {
        HANDLER.with(|h| {
            h.borrow_mut().as_mut().map(|handler| {
                ENVS.with(|e| e.borrow_mut().push(env.to_vec()));
                handler(args, timeout)
            })
        })
    }
}
