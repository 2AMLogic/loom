//! Safehouse: the one merged-PR report in the fleet room (#13946, part of #13920).
//!
//! Safehouse (`rjwalters/safehouse`) is an end-to-end-encrypted Matrix room a
//! human watches in Element. A per-host `safehoused` owns the Matrix device and
//! exposes a keyless `AF_UNIX` RPC. Loom uses it for exactly **one** thing: a
//! slim line in the fleet room each time a PR merges. The dashboard and
//! telemetry are the record of merges; the line is a cosmetic heartbeat, so
//! delivery is **best effort** (operator ruling 2026-11-25), not exactly-once.
//!
//! # What this module does
//!
//! - [`spawn_merge_reporter`] starts the reporter ([`reporter`]): the host that
//!   ran a merge posts it, and the fleet captain posts the merges no host
//!   reported inside a bounded catch-up window.
//! - [`SafehouseClient`] is the socket client: `hello`, `send`, and `read`.
//!   After a lost `send` reply the reporter reads the room back and re-sends
//!   only when that PR's line is absent ([`merge_report`]).
//! - The config ([`resolve_config`]) and connection-state cell
//!   ([`SafehouseState`], shown by `loom-daemon status`) and the
//!   `Envelope`/`build_send_request` wire builder that `loom-daemon fleet-send`
//!   shares.
//!
//! Loom sends no per-step narration, no status reactions, no completion ads and
//! no `loom_claim` messages. **Loom never blocks a sweep on safehouse**: with
//! `safehouse.enabled` false or absent nothing here touches a socket.
//!
//! # Wire protocol (envelope v1)
//!
//! - `AF_UNIX`, **newline-delimited JSON**, one object per line, bidirectional.
//! - Mandatory first request `{"id":0,"op":"hello","persona":"<name>"}`; any op
//!   before `hello` is rejected. `persona` must be in safehoused's allowlist.
//! - `send` carries `to`/`type`/`body` and an optional `room`. The daemon
//!   **stamps `from`**, so this client never sends one. The reply carries the
//!   Matrix `event_id`.
//! - `read` carries an optional `room` and a `limit` (default 20, max 100) and
//!   replies `{"ok":true,"messages":[{"event_id","sender","own","ts",
//!   "envelope":{"body",…}}],"undecryptable":N}`, oldest first.
//! - Replies echo the request `id`. **Async push lines are interleaved on the
//!   same connection and carry an `event` key with no `id`**; the client skips
//!   any line with an `event` key.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

mod forge_gate;
mod merge_report;
mod merge_source;
mod reporter;
pub use reporter::spawn_merge_reporter;

// ============================================================================
// Constants
// ============================================================================

/// Config-block env overrides (precedence **env > config > default**).
const ENABLED_ENV: &str = "LOOM_SAFEHOUSE_ENABLED";
const SOCKET_ENV: &str = "LOOM_SAFEHOUSE_SOCKET";
const ROOM_ENV: &str = "LOOM_SAFEHOUSE_ROOM";
const PERSONA_ENV: &str = "LOOM_SAFEHOUSE_PERSONA";
/// Overrides `rooms.signal`, the fleet room the merged-PR line is posted to.
const ROOM_SIGNAL_ENV: &str = "LOOM_SAFEHOUSE_ROOM_SIGNAL";

/// The unprefixed socket variable safehoused's own clients read.
const SAFEHOUSED_SOCKET_ENV: &str = "SAFEHOUSED_SOCKET";

/// Test/operator override for the `gh` binary the forge lookups run.
const GH_BIN_ENV: &str = "LOOM_SAFEHOUSE_GH_BIN";

/// Default persona; must be in safehoused's boot-time allowlist.
const DEFAULT_PERSONA: &str = "loom_daemon";

/// The envelope protocol version this client speaks.
pub const ENVELOPE_VERSION: u32 = 1;

/// The closed set of envelope `type`s safehoused accepts from this client.
/// `completion` is not sent by Loom (the merged-PR line is a `chat`), so it is
/// not in the set.
pub const KNOWN_TYPES: [&str; 5] = ["chat", "task", "handoff", "ack", "digest"];

// ============================================================================
// Config
// ============================================================================

/// The room map. Only `signal` is read now: the fleet room the merged-PR line
/// goes to. Attention-class routing, per-repo rooms and the alerts room were
/// narration-only and are retired (#13946).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RoomMap {
    /// The everyone/signal room (`loom-fleet`). `None` ⇒ fall back to the
    /// scalar [`SafehouseConfig::room`].
    pub signal: Option<String>,
}

/// Resolved `safehouse` config block. `enabled: false` is the default and a
/// byte-for-byte no-op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafehouseConfig {
    pub enabled: bool,
    /// Socket path; `None` ⇒ resolve `$SAFEHOUSED_SOCKET` at connect time.
    pub socket: Option<PathBuf>,
    /// Room name/id; `None` is valid only when safehoused joined exactly one
    /// room (it then resolves the sole room server-side).
    pub room: Option<String>,
    /// The room map; `signal` takes precedence over [`room`](Self::room).
    pub rooms: Option<RoomMap>,
    /// Persona to authenticate as; must be in safehoused's allowlist.
    pub persona: String,
}

impl Default for SafehouseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            socket: None,
            room: None,
            rooms: None,
            persona: DEFAULT_PERSONA.to_owned(),
        }
    }
}

impl SafehouseConfig {
    /// The room the merged-PR line is posted to and read back from:
    /// `rooms.signal` when configured, else the scalar [`room`](Self::room).
    #[must_use]
    pub fn signal_room(&self) -> Option<&str> {
        self.rooms
            .as_ref()
            .and_then(|rooms| rooms.signal.as_deref())
            .or(self.room.as_deref())
    }
}

/// Resolve the effective `safehouse` config for `repo_root` with precedence
/// **env > config > default(disabled)**. Never panics: a missing/malformed
/// config tree resolves to [`SafehouseConfig::default`] (disabled).
#[must_use]
pub fn resolve_config(repo_root: &Path) -> SafehouseConfig {
    let effective = crate::config_resolver::resolve_effective_config(repo_root);
    let block = crate::config_resolver::get_path(&effective, "safehouse");
    let retired = retired_keys(block, |name| env_nonempty(name).is_some());
    warn_retired_keys_once(&RETIRED_WARNED, &retired);
    apply_env_overrides(config_from_value(block))
}

/// `safehouse` config keys that only the retired peer-claim channel (#13945) or
/// the retired per-step narration (#13946) read. Still *accepted* — a leftover
/// one must not stop a daemon starting — but ignored, and named once in a
/// warning.
const RETIRED_CONFIG_KEYS: &[&str] = &["peerClaimTtlSecs", "operatorMention"];

/// Retired `safehouse.rooms.*` keys, warned about the same way.
const RETIRED_ROOMS_KEYS: &[&str] = &["claims", "byRepo", "alerts"];

/// Retired environment variables, warned about the same way.
const RETIRED_ENV_KEYS: &[&str] = &[
    "LOOM_SAFEHOUSE_ROOM_CLAIMS",
    "LOOM_PEER_CLAIM_TTL_SECS",
    "LOOM_SAFEHOUSE_ROOMS_BY_REPO",
    "LOOM_SAFEHOUSE_ROOM_ALERTS",
    "LOOM_SAFEHOUSE_OPERATOR_MENTION",
];

/// Guards the one-per-process leftover-key warning.
static RETIRED_WARNED: std::sync::Once = std::sync::Once::new();

/// Which retired keys are still set (`env_set` answers whether an env var is
/// exported, so tests need not mutate the process environment).
#[must_use]
fn retired_keys(block: Option<&Value>, env_set: impl Fn(&str) -> bool) -> Vec<String> {
    let mut keys = Vec::new();
    if let Some(block) = block.and_then(Value::as_object) {
        if let Some(rooms) = block.get("rooms").and_then(Value::as_object) {
            for key in RETIRED_ROOMS_KEYS {
                if rooms.contains_key(*key) {
                    keys.push(format!("safehouse.rooms.{key}"));
                }
            }
        }
        for key in RETIRED_CONFIG_KEYS {
            if block.contains_key(*key) {
                keys.push(format!("safehouse.{key}"));
            }
        }
    }
    keys.extend(
        RETIRED_ENV_KEYS
            .iter()
            .filter(|name| env_set(name))
            .map(|name| (*name).to_owned()),
    );
    keys
}

/// Log the leftover-key warning at most once per `once`, however many times the
/// config is re-resolved. Returns whether this call emitted it.
fn warn_retired_keys_once(once: &std::sync::Once, keys: &[String]) -> bool {
    if keys.is_empty() {
        return false;
    }
    let mut emitted = false;
    once.call_once(|| {
        emitted = true;
        log::warn!(
            "safehouse: ignoring retired settings ({}); safehouse now carries only the \
             merged-PR report in the fleet room (#13920, #13946). Delete these keys.",
            keys.join(", ")
        );
    });
    emitted
}

/// Read the config layer only (no env), so unit tests can assert
/// config-over-default without mutating process env.
#[must_use]
fn config_from_value(block: Option<&Value>) -> SafehouseConfig {
    let mut cfg = SafehouseConfig::default();
    let Some(block) = block.and_then(Value::as_object) else {
        return cfg;
    };
    if let Some(enabled) = block.get("enabled").and_then(Value::as_bool) {
        cfg.enabled = enabled;
    }
    if let Some(socket) = block.get("socket").and_then(Value::as_str) {
        if !socket.trim().is_empty() {
            cfg.socket = Some(PathBuf::from(socket));
        }
    }
    if let Some(room) = block.get("room").and_then(Value::as_str) {
        if !room.trim().is_empty() {
            cfg.room = Some(room.to_owned());
        }
    }
    if let Some(persona) = block.get("persona").and_then(Value::as_str) {
        if !persona.trim().is_empty() {
            cfg.persona = persona.to_owned();
        }
    }
    cfg.rooms = rooms_from_value(block.get("rooms"));
    cfg
}

/// Parse the `safehouse.rooms` sub-block. Every malformed shape degrades to
/// "that key was not configured", and a map with nothing usable normalizes to
/// `None`.
#[must_use]
fn rooms_from_value(block: Option<&Value>) -> Option<RoomMap> {
    let signal = block?
        .as_object()?
        .get("signal")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    Some(RoomMap {
        signal: Some(signal.to_owned()),
    })
}

/// Apply the env layer on top of a config-resolved [`SafehouseConfig`]. Env
/// wins over config for every key.
#[must_use]
fn apply_env_overrides(mut cfg: SafehouseConfig) -> SafehouseConfig {
    if let Some(enabled) = env_bool(ENABLED_ENV) {
        cfg.enabled = enabled;
    }
    if let Some(socket) = env_nonempty(SOCKET_ENV) {
        cfg.socket = Some(PathBuf::from(socket));
    }
    if let Some(room) = env_nonempty(ROOM_ENV) {
        cfg.room = Some(room);
    }
    if let Some(persona) = env_nonempty(PERSONA_ENV) {
        cfg.persona = persona;
    }
    if let Some(signal) = env_nonempty(ROOM_SIGNAL_ENV) {
        cfg.rooms = Some(RoomMap {
            signal: Some(signal),
        });
    }
    cfg
}

/// Resolve the socket path at connect time. **Precedence: env > config >
/// None** — `$LOOM_SAFEHOUSE_SOCKET` (already folded into `cfg.socket` by
/// [`apply_env_overrides`]) or the unprefixed `$SAFEHOUSED_SOCKET` convention
/// safehoused clients read both win over `cfg.socket`, which by this point
/// only reflects the committed/local-override config value.
///
/// Checking env directly here (not just relying on the already-merged
/// `cfg.socket`) is load-bearing: before this fix `cfg.socket` was checked
/// *first*, so a committed `safehouse.socket` path (opt-in, per-host, and
/// often copied verbatim into shared `.loom/config.json`) permanently shadowed
/// any `$SAFEHOUSED_SOCKET` override — the same defect class #5354/#5336
/// fixed for `observability.ingestKeyFile`, now mirrored here for
/// `safehouse.socket` (#5457). No built-in `$HOME`-relative default: unlike
/// `ingestKeyFile`, safehouse is opt-in per-host, so an unconfigured socket
/// degrades to `None` (warn + skip) rather than guessing a path.
#[must_use]
pub fn resolve_socket(cfg: &SafehouseConfig) -> Option<PathBuf> {
    env_nonempty(SOCKET_ENV)
        .or_else(|| env_nonempty(SAFEHOUSED_SOCKET_ENV))
        .map(PathBuf::from)
        .or_else(|| cfg.socket.clone())
}

fn env_bool(key: &str) -> Option<bool> {
    let raw = std::env::var(key).ok()?;
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" | "" => Some(false),
        _ => None,
    }
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

// ============================================================================
// Connection state (issue #4345 — new-host onboarding visibility)
// ============================================================================
//
// Before #4345, `safehouse.enabled` false/absent, enabled-but-unreachable, and
// enabled-and-connected all looked identical to an operator: silence. The
// merge reporter knows the outcome of its own last connect; this cell is how
// that knowledge reaches `loom-daemon status` without a second, status-time
// connection attempt.
//
// The reporter connects per report (a merge is rare), so `Connected` means "the
// most recent report reached safehoused", and a freshly started daemon reads
// `Unreachable` ("configured, not yet connected") until its first report.

/// Live safehouse connection state, updated by the merge reporter
/// ([`spawn_merge_reporter`]) through a [`SharedSafehouseState`] cell.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SafehouseState {
    /// `safehouse.enabled` is false/absent (the byte-for-byte no-op path), or
    /// enabled with no socket resolving at all. No connection has ever been
    /// attempted for this transition.
    #[default]
    NotConfigured,
    /// Enabled with a socket resolved, but the most recent connect attempt
    /// failed, refused, or dropped. `socket` carries the path that was tried.
    Unreachable { socket: PathBuf },
    /// The most recent connect attempt completed the `hello` handshake
    /// successfully. `room` is the configured room name — `None` when
    /// [`SafehouseConfig::room`] is unset, which is only valid when safehoused
    /// joined exactly one room (resolved server-side; this client is never
    /// told the resolved name in that case).
    Connected {
        socket: PathBuf,
        room: Option<String>,
    },
    /// The `hello` handshake succeeded, but the most recent `send` was rejected
    /// at the protocol layer (`ok:false`) — the socket is reachable and the
    /// connection is healthy, only the send was refused (#4464). The canonical
    /// case is a multi-room safehoused with [`SafehouseConfig::room`] unset,
    /// whose `send` returns `'room' required: N rooms joined`. Distinct from
    /// [`Unreachable`](Self::Unreachable) so `loom-daemon status` points the
    /// operator at `safehouse.room` rather than at the socket/persona. `reason`
    /// carries the raw safehoused `error` string. **Sticky**: a reconnect whose
    /// `hello` succeeds does not clear it; only a `send` that is accepted
    /// returns the state to [`Connected`](Self::Connected).
    SendRejected { socket: PathBuf, reason: String },
}

impl SafehouseState {
    /// Render into the wire [`crate::types::SafehouseStatus`] shape consumed by
    /// `DaemonStatusReport` (#4345).
    #[must_use]
    pub fn to_status(&self) -> crate::types::SafehouseStatus {
        match self {
            Self::NotConfigured => crate::types::SafehouseStatus {
                state: "not_configured".to_owned(),
                socket: None,
                room: None,
                reason: None,
            },
            Self::Unreachable { socket } => crate::types::SafehouseStatus {
                state: "unreachable".to_owned(),
                socket: Some(socket.clone()),
                room: None,
                reason: None,
            },
            Self::Connected { socket, room } => crate::types::SafehouseStatus {
                state: "connected".to_owned(),
                socket: Some(socket.clone()),
                room: room.clone(),
                reason: None,
            },
            Self::SendRejected { socket, reason } => crate::types::SafehouseStatus {
                state: "send_rejected".to_owned(),
                socket: Some(socket.clone()),
                room: None,
                reason: Some(reason.clone()),
            },
        }
    }
}

/// A shared, `Arc`-wrapped connection-state cell, injected into
/// [`spawn_merge_reporter`] so [`WorkspacePool`](crate::workspace_pool::WorkspacePool)
/// can hold one cell per daemon and read it back for `loom-daemon status`
/// without a second connection.
pub type SharedSafehouseState = Arc<Mutex<SafehouseState>>;

/// Construct a fresh cell defaulted to [`SafehouseState::NotConfigured`] — the
/// correct starting value for a daemon that has not yet called
/// [`spawn_merge_reporter`] for this cell.
#[must_use]
pub fn new_shared_state() -> SharedSafehouseState {
    Arc::new(Mutex::new(SafehouseState::default()))
}

/// Overwrite `cell` with `state`. Recovers a poisoned mutex (a panic on some
/// other thread while holding the lock must never permanently blind `status`
/// to connection state) rather than propagating the poison.
fn set_state(cell: &SharedSafehouseState, state: SafehouseState) {
    match cell.lock() {
        Ok(mut guard) => *guard = state,
        Err(poisoned) => *poisoned.into_inner() = state,
    }
}

/// Snapshot the current connection state out of `cell`.
#[must_use]
pub fn snapshot_state(cell: &SharedSafehouseState) -> SafehouseState {
    match cell.lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Visible room lines and whether the read can establish that a report is absent.
#[derive(Debug, PartialEq, Eq)]
pub struct RecentMessages {
    /// Message bodies returned by safehoused, oldest first.
    pub bodies: Vec<String>,
    /// Every possible report in the returned window was readable.
    pub complete: bool,
    /// Matrix origin time of the first returned message, in milliseconds.
    oldest_ts: Option<i64>,
}

impl RecentMessages {
    /// Safehoused filters non-message events after limiting the timeline, so
    /// even an empty reply can be a truncated slice. Only a readable row
    /// strictly before the instant in question establishes coverage (timestamps can tie).
    fn covers(&self, since: chrono::DateTime<chrono::Utc>) -> bool {
        self.complete
            && self
                .oldest_ts
                .is_some_and(|ts| ts < since.timestamp_millis())
    }
}

/// Set `cell` to [`SafehouseState::NotConfigured`] (#4345).
#[cfg(test)]
pub fn set_not_configured(cell: &SharedSafehouseState) {
    set_state(cell, SafehouseState::NotConfigured);
}

// ============================================================================
// Envelope
// ============================================================================

/// A logical outbound message. Deliberately omits `from` — safehoused stamps it
/// from the socket identity (§6), and this client never sends it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    /// Recipient persona, `"*"` (everyone), or a `@matrix:id`.
    pub to: String,
    /// One of [`KNOWN_TYPES`].
    pub kind: String,
    /// Task thread key; must be `[A-Za-z0-9_]` (a bare issue number is fine).
    pub task_id: Option<String>,
    pub body: String,
}

/// `[a-z0-9_]`, 1..=64 chars (persona charset, `envelope.rs` `valid_persona`).
fn valid_persona(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Normalize a `to` value: `"*"` and `@matrix:id` pass through; a persona is
/// lowercased and hyphens folded to underscores (hyphens are a render-time
/// cosmetic — the wire form is underscored, and safehoused does **not**
/// normalize `to`, so a hyphenated `to` would route nowhere). Rejects anything
/// that is still not a valid persona after normalization.
fn normalize_to(to: &str) -> Result<String> {
    if to == "*" || to.starts_with('@') {
        return Ok(to.to_owned());
    }
    let normalized = to.to_ascii_lowercase().replace('-', "_");
    if valid_persona(&normalized) {
        Ok(normalized)
    } else {
        bail!("invalid `to` {to:?}: not \"*\", a @matrix-id, or a [a-z0-9_] persona")
    }
}

/// Build the `send` RPC request for `env`, validating **before** sending:
/// `type` must be a known type, `task_id` must be `[A-Za-z0-9_]`, and `to` is
/// normalized. Emits `v: 1`, never a `from`, and omits `task_id`/`room` when
/// absent. safehoused ignores the extra `v` and re-stamps it — carrying it here
/// makes the request self-describe as envelope-v1.
pub fn build_send_request(env: &Envelope, id: u64, room: Option<&str>) -> Result<Value> {
    if !KNOWN_TYPES.contains(&env.kind.as_str()) {
        bail!("invalid envelope type {:?} (v1 types: {:?})", env.kind, KNOWN_TYPES);
    }
    if let Some(task_id) = &env.task_id {
        if task_id.is_empty()
            || !task_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            bail!("invalid task_id {task_id:?}: must be [A-Za-z0-9_]");
        }
    }
    let to = normalize_to(&env.to)?;

    let mut req = json!({
        "id": id,
        "op": "send",
        "v": ENVELOPE_VERSION,
        "to": to,
        "type": env.kind,
        "body": env.body,
    });
    let obj = req.as_object_mut().expect("json object literal");
    if let Some(task_id) = &env.task_id {
        obj.insert("task_id".into(), json!(task_id));
    }
    if let Some(room) = room {
        obj.insert("room".into(), json!(room));
    }
    Ok(req)
}

// ============================================================================
// Client
// ============================================================================

/// Why a [`SafehouseClient`] call failed — split so the reporter can tell a
/// **protocol rejection** (the connection is healthy; retrying identically will
/// be rejected identically) apart from a **transport failure** (the connection
/// is gone, or the reply was lost: the outcome is unknown) without
/// string-matching an untyped `anyhow` chain (#4464).
#[derive(Debug)]
pub enum SendError {
    /// safehoused accepted the request over the wire but refused it at the
    /// protocol layer (`ok:false`). `reason` is the raw `error` string — the
    /// canonical case is `'room' required: N rooms joined` on a multi-room host
    /// with no room configured. The connection stays usable.
    Rejected { reason: String },
    /// A transport-level failure (write/read I/O, closed connection, timeout,
    /// or a reply `id` desync). For a `send` this is **ambiguous**: the message
    /// may or may not have reached the room, which is why the reporter reads the
    /// room back before retrying.
    Transport(anyhow::Error),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { reason } => write!(f, "safehoused rejected request: {reason}"),
            Self::Transport(err) => write!(f, "{err:#}"),
        }
    }
}

impl std::error::Error for SendError {}

/// A blocking-free async envelope-v1 client over one `AF_UNIX` connection.
pub struct SafehouseClient {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next_id: u64,
}

impl SafehouseClient {
    /// Connect, perform the mandatory `hello` handshake, and verify the persona
    /// was accepted. Errors (socket absent/refused, persona rejected) are
    /// returned for the caller to degrade to a `warn`.
    pub async fn connect(socket: &Path, persona: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to safehoused at {}", socket.display()))?;
        let (read_half, write_half) = stream.into_split();
        let mut client = Self {
            reader: BufReader::new(read_half),
            writer: write_half,
            next_id: 0,
        };

        let hello = json!({"id": 0, "op": "hello", "persona": persona});
        client.write_line(&hello).await?;
        let reply = client.read_reply().await?;
        if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let err = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            bail!("safehoused rejected persona {persona:?}: {err}");
        }
        client.next_id = 1;
        Ok(client)
    }

    /// Serialize and send one envelope to `room` (`None` sends no `room` key —
    /// valid only when safehoused joined exactly one room), then read and
    /// `id`-match the reply (skipping any interleaved push line). Returns the
    /// Matrix `event_id` safehoused reports for the accepted message (`None`
    /// when an older daemon's reply omits it).
    pub async fn send_to(
        &mut self,
        env: &Envelope,
        room: Option<&str>,
    ) -> std::result::Result<Option<String>, SendError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        // A malformed envelope is a caller bug, not a server rejection and not
        // a lost reply; surface it as Transport so it is logged, not retried
        // as an ambiguous send.
        let req = build_send_request(env, id, room).map_err(SendError::Transport)?;
        self.write_line(&req).await.map_err(SendError::Transport)?;
        let reply = self.read_reply().await.map_err(SendError::Transport)?;
        Self::check_reply(&reply, id)?;
        Ok(reply
            .get("event_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned))
    }

    /// The text bodies of the most recent `limit` (at most 100) messages in
    /// `room`, oldest first, via the socket `read` op — the room read-back the
    /// merged-PR report runs before any retry. A row with no readable envelope
    /// body contributes no visible line. Unreadable events make absence inconclusive.
    pub async fn read_recent(
        &mut self,
        room: Option<&str>,
        limit: u32,
    ) -> std::result::Result<RecentMessages, SendError> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let mut req = json!({"id": id, "op": "read", "limit": limit.min(100)});
        if let Some(room) = room {
            req["room"] = json!(room);
        }
        self.write_line(&req).await.map_err(SendError::Transport)?;
        let reply = self.read_reply().await.map_err(SendError::Transport)?;
        Self::check_reply(&reply, id)?;
        let rows = reply["messages"]
            .as_array()
            .context("read reply has no messages array")
            .map_err(SendError::Transport)?;
        let mut complete = reply["undecryptable"].as_u64() == Some(0);
        let mut bodies = Vec::new();
        for row in rows {
            if let Some(body) = row["envelope"]["body"].as_str() {
                bodies.push(body.to_owned());
            } else {
                // A decryption gap, unknown envelope version or malformed row
                // can hide the text even when attachment metadata is present.
                complete = false;
            }
        }
        let oldest_ts = rows
            .first()
            .and_then(|row| row["ts"].as_i64())
            .filter(|ts| *ts >= 0);
        complete &= rows.is_empty() || oldest_ts.is_some();
        Ok(RecentMessages {
            bodies,
            complete,
            oldest_ts,
        })
    }

    /// Shared reply gate: `ok:false` is a [`SendError::Rejected`]; a reply `id`
    /// that does not echo the request is a stream desync (Transport), so the
    /// caller reconnects.
    fn check_reply(reply: &Value, id: u64) -> std::result::Result<(), SendError> {
        if !reply.get("ok").and_then(Value::as_bool).unwrap_or(false) {
            let reason = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned();
            return Err(SendError::Rejected { reason });
        }
        if let Some(reply_id) = reply.get("id").and_then(Value::as_u64) {
            if reply_id != id {
                return Err(SendError::Transport(anyhow::anyhow!(
                    "safehoused reply id {reply_id} != request id {id} (stream desync)"
                )));
            }
        }
        Ok(())
    }

    async fn write_line(&mut self, value: &Value) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;
        Ok(())
    }

    /// Read the next reply line, **skipping push lines** — any line carrying an
    /// `event` key (and no `id`) is an async inbound room event, not a reply.
    async fn read_reply(&mut self) -> Result<Value> {
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).await?;
            if n == 0 {
                bail!("safehoused closed the connection");
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value =
                serde_json::from_str(trimmed).context("bad reply from safehoused")?;
            if value.get("event").is_some() {
                // Interleaved async push (inbound room event): not a reply.
                continue;
            }
            return Ok(value);
        }
    }
}

#[cfg(test)]
mod tests;
