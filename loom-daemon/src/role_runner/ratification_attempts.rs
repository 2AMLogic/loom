//! The ratification lane's attempt bound (#15315).
//!
//! # The loop this bounds
//!
//! A withheld key posts no marker, so the key is still missing on the next
//! tick and is dispatched again at the same head — on every host, every
//! [`super::ratification_lane::STEP_TICK_INTERVAL`] and after every Champion
//! role run. When the session can never produce a parseable review for that
//! PR (or a host fault refuses every launch) the same head is reviewed
//! without end. Selection must not change (#15315), so the bound sits
//! between selection and dispatch.
//!
//! # Counted attempts: the host-local ledger
//!
//! A session that ran and left no usable review — [`WithheldClass::OutputMissing`],
//! [`WithheldClass::VerdictInvalid`], [`WithheldClass::VerdictPlaceholder`] —
//! is one **counted attempt** for its `(repo, pr, head, key)`. At most
//! [`MAX_COUNTED_ATTEMPTS_PER_WINDOW`] counted attempts are dispatched per
//! `(repo, pr, head, key)` in any trailing [`ATTEMPT_WINDOW_HOURS`]: once that
//! many are on record the key is **capped** and not dispatched until the
//! oldest of them ages out, or at once on a new head (records name the head
//! they were counted at). The ledger is a small JSON file per workspace,
//! [`ledger_path`], written atomically. Nothing is posted to the PR.
//!
//! It is an efficiency guard, not a safety gate: an unreadable or corrupt
//! ledger is treated as empty (logged once) and never blocks dispatch. It is
//! per host by construction, like the rest of `.loom/state/`; deleting the
//! file is the manual reset. A ledger written under other parse rules
//! ([`LEDGER_RULES`]) counts nothing, so a parser fix releases every cap.
//!
//! # Not counted: backoffs in process memory
//!
//! A failure the PR did not cause is never a counted attempt. It instead
//! delays the next try by [`BACKOFF_MINUTES`] ([`Backoff`]):
//!
//! - a pinned-instruction read that failed (the `reader budget low` shed,
//!   say): that pin, process-wide — every root on the host shares the reader
//!   budget and the pin (critique A9);
//! - a launch refused before its CLI ran ([`WithheldClass::PreLaunchFailed`])
//!   or a launched session that failed with an account, pool, credential or
//!   host class ([`is_host_failure_class`]): the key, for that repo;
//! - a forge post that failed, or a launched session that failed for any
//!   other reason: that `(repo, pr, head, key)` only.
//!
//! A version roll stopping a session is neither counted nor backed off.
//! Backoffs reset with the daemon.

use super::ratification_dispatch::{AttemptFailure, InstructionSource};
use super::ratification_lane::{Key, Pin, WithheldClass};
use chrono::{DateTime, TimeDelta, Utc};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The most counted attempts dispatched per `(repo, pr, head, key)` in any
/// trailing [`ATTEMPT_WINDOW_HOURS`] — the Judge backstop's
/// `MAX_PER_WINDOW` (#11855) on the same window.
pub const MAX_COUNTED_ATTEMPTS_PER_WINDOW: usize = 3;

/// The trailing window counted attempts are read in.
pub const ATTEMPT_WINDOW_HOURS: i64 = 24;

/// How long a failure the PR did not cause delays the next try — the Judge
/// backstop's refused cooldown.
pub const BACKOFF_MINUTES: i64 = 15;

/// The parse rules ([`super::ratification_lane::parse_review`]) the ledger's
/// records were counted under. Bump it whenever those rules change: a ledger
/// written under other rules counts nothing.
pub const LEDGER_RULES: u32 = 1;

/// [`ATTEMPT_WINDOW_HOURS`] as a duration.
#[must_use]
pub fn attempt_window() -> TimeDelta {
    TimeDelta::hours(ATTEMPT_WINDOW_HOURS)
}

/// [`BACKOFF_MINUTES`] as a duration.
#[must_use]
pub fn backoff() -> TimeDelta {
    TimeDelta::minutes(BACKOFF_MINUTES)
}

/// `<root>/.loom/state/ratification-lane/attempts.json` — per host, never
/// git-tracked (`.loom/state/*` is ignored, `init/post_init.rs`).
#[must_use]
pub fn ledger_path(root: &Path) -> PathBuf {
    root.join(".loom")
        .join("state")
        .join("ratification-lane")
        .join("attempts.json")
}

/// Whether a withheld class is a counted attempt: the session ran and left
/// no usable review. Launch refusals, pre-launch and launched host failures,
/// roll stops and post failures are never counted.
#[must_use]
pub fn counts_toward_cap(class: WithheldClass) -> bool {
    matches!(
        class,
        WithheldClass::OutputMissing
            | WithheldClass::VerdictInvalid
            | WithheldClass::VerdictPlaceholder
    )
}

/// Whether a launched session's `loom.failure_class` names an account, pool,
/// credential or host fault rather than anything about the PR (critique A1):
/// `account-pool-exhausted` (claude-wrapper exits 78 after its CLI started),
/// `account-exhausted:*`, `no-usable-account`, `session-limit`,
/// `credential-expired`, `runtime-timeout`, `sandbox-unavailable`,
/// `cwd-deleted`, and every launch-refusal class.
#[must_use]
pub fn is_host_failure_class(class: &str) -> bool {
    class.starts_with("account-exhausted:")
        || matches!(
            class,
            "account-pool-exhausted"
                | "no-usable-account"
                | "session-limit"
                | "credential-expired"
                | "runtime-timeout"
                | "sandbox-unavailable"
                | "cwd-deleted"
        )
        || super::ratification_dispatch::LAUNCH_REFUSAL_CLASSES.contains(&class)
}

/// What an uncounted withheld key delays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backoff {
    /// Nothing: a counted attempt, or a version roll.
    None,
    /// The key's pinned instructions, process-wide.
    Pin,
    /// The key, for the target repo.
    Key,
    /// That `(repo, pr, head, key)` only.
    Pr,
}

/// The backoff a withheld `class` earns, given why the attempt failed.
#[must_use]
pub fn backoff_for(class: WithheldClass, failure: Option<&AttemptFailure>) -> Backoff {
    match class {
        WithheldClass::PreLaunchFailed => match failure {
            Some(AttemptFailure::Pin) => Backoff::Pin,
            _ => Backoff::Key,
        },
        WithheldClass::InvocationFailed => match failure {
            Some(AttemptFailure::Rolled) => Backoff::None,
            Some(AttemptFailure::Launched(Some(c))) if is_host_failure_class(c) => Backoff::Key,
            _ => Backoff::Pr,
        },
        WithheldClass::PostFailed => Backoff::Pr,
        WithheldClass::OutputMissing
        | WithheldClass::VerdictInvalid
        | WithheldClass::VerdictPlaceholder => Backoff::None,
    }
}

/// One counted attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// The target repo (`owner/name`).
    pub repo: String,
    pub pr: u64,
    /// The full 40-hex head the attempt reviewed.
    pub head_sha: String,
    /// `ee` | `market`.
    pub key: String,
    /// The withheld class's wire name ([`WithheldClass::wire`]); never the
    /// reason text.
    pub class: String,
    pub at: DateTime<Utc>,
}

impl AttemptRecord {
    fn is(&self, repo: &str, pr: u64, head_sha: &str, key: Key) -> bool {
        self.pr == pr
            && self.key == key.wire()
            && self.repo == repo
            && self.head_sha.eq_ignore_ascii_case(head_sha)
    }

    fn in_window(&self, now: DateTime<Utc>) -> bool {
        self.at + attempt_window() > now
    }
}

/// The ledger file's contents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// [`LEDGER_RULES`] when the records were counted.
    pub rules: u32,
    pub attempts: Vec<AttemptRecord>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            rules: LEDGER_RULES,
            attempts: Vec::new(),
        }
    }
}

impl Ledger {
    /// The counted attempts for `(repo, pr, head, key)` inside the window
    /// ending at `now`, oldest first.
    #[must_use]
    pub fn counted(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
        key: Key,
        now: DateTime<Utc>,
    ) -> Vec<&AttemptRecord> {
        let mut out: Vec<&AttemptRecord> = self
            .attempts
            .iter()
            .filter(|r| r.is(repo, pr, head_sha, key) && r.in_window(now))
            .collect();
        out.sort_by_key(|r| r.at);
        out
    }

    /// When a capped key becomes dispatchable again — the moment enough of
    /// its counted attempts age out of the window — or `None` while it is
    /// below [`MAX_COUNTED_ATTEMPTS_PER_WINDOW`].
    #[must_use]
    pub fn capped_until(
        &self,
        repo: &str,
        pr: u64,
        head_sha: &str,
        key: Key,
        now: DateTime<Utc>,
    ) -> Option<DateTime<Utc>> {
        let counted = self.counted(repo, pr, head_sha, key, now);
        let n = counted.len();
        (n >= MAX_COUNTED_ATTEMPTS_PER_WINDOW)
            .then(|| counted[n - MAX_COUNTED_ATTEMPTS_PER_WINDOW].at + attempt_window())
    }

    /// Record one counted attempt.
    pub fn record(&mut self, record: AttemptRecord) {
        self.attempts.push(record);
    }

    /// Add `records` not already present (exact match).
    pub fn merge(&mut self, records: &[AttemptRecord]) {
        for r in records {
            if !self.attempts.contains(r) {
                self.attempts.push(r.clone());
            }
        }
    }

    /// Drop records that have aged out of the window and, when `current` is
    /// given, records whose `(pr, head)` (head lowercase) is not in it.
    /// Returns whether anything was dropped.
    pub fn prune(&mut self, now: DateTime<Utc>, current: Option<&HashSet<(u64, String)>>) -> bool {
        let before = self.attempts.len();
        self.attempts.retain(|r| {
            r.in_window(now)
                && current.is_none_or(|c| c.contains(&(r.pr, r.head_sha.to_ascii_lowercase())))
        });
        self.attempts.len() != before
    }
}

/// Read the ledger at `path`. A missing file is an empty ledger; a ledger
/// written under other [`LEDGER_RULES`] counts nothing (empty).
///
/// # Errors
///
/// The file exists but cannot be read or does not parse; the caller treats
/// that as empty (fail open) and logs it.
pub fn load(path: &Path) -> Result<Ledger, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Ledger::default()),
        Err(e) => return Err(format!("{} could not be read: {e}", path.display())),
    };
    let ledger: Ledger = serde_json::from_str(&text)
        .map_err(|e| format!("{} did not parse: {e}", path.display()))?;
    Ok(if ledger.rules == LEDGER_RULES {
        ledger
    } else {
        Ledger::default()
    })
}

/// Write the ledger to `path` atomically (temp file, fsync, rename).
///
/// # Errors
///
/// Any I/O failure; the caller keeps the records in process memory.
pub fn save(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let text = serde_json::to_vec_pretty(ledger).map_err(|e| e.to_string())?;
    crate::fs_util::write_atomic(path, &text, crate::fs_util::AtomicOpts::default())
        .map_err(|e| format!("{} could not be written: {e}", path.display()))
}

/// The pinned-instruction identity a cache entry and a pin backoff key on.
type PinId = (String, String, String);

fn pin_id(pin: &Pin) -> PinId {
    (pin.repo.clone(), pin.path.clone(), pin.commit.clone())
}

/// What the lane remembers between passes, in process memory: the
/// pinned-instruction cache, the backoffs and ledger records whose save
/// failed. One per process ([`process_memory`]), shared by every root and
/// locked only briefly, never across a session (critique A9).
#[derive(Debug, Default)]
pub struct LaneMemory {
    /// Pinned instruction bodies: content at a full 40-hex commit never
    /// changes, so each is read once per process.
    pins: HashMap<PinId, String>,
    /// Pins whose read failed, until when.
    pin_backoff: HashMap<PinId, DateTime<Utc>>,
    /// `(repo, key)` after a launch refusal or a host failure, until when.
    key_backoff: HashMap<(String, Key), DateTime<Utc>>,
    /// `(repo, pr, head, key)` after a post failure or an uncounted session
    /// failure, until when.
    pr_backoff: HashMap<(String, u64, String, Key), DateTime<Utc>>,
    /// Ledger records whose save failed, by ledger path: merged into the
    /// next load so a host still stops at the cap when the disk refuses.
    unsaved: HashMap<PathBuf, Vec<AttemptRecord>>,
}

/// One key of one candidate PR, as the backoffs key it.
#[derive(Debug, Clone, Copy)]
pub struct KeySlot<'a> {
    /// The target repo (`owner/name`).
    pub repo: &'a str,
    pub pr: u64,
    pub head_sha: &'a str,
    pub key: Key,
    /// The key's pinned instructions.
    pub pin: &'a Pin,
}

impl KeySlot<'_> {
    fn pr_id(&self) -> (String, u64, String, Key) {
        (self.repo.to_string(), self.pr, self.head_sha.to_ascii_lowercase(), self.key)
    }
}

impl LaneMemory {
    /// Whether `slot` is held back by a backoff at `now`.
    #[must_use]
    pub fn deferred(&self, slot: &KeySlot<'_>, now: DateTime<Utc>) -> bool {
        let live = |until: Option<&DateTime<Utc>>| until.is_some_and(|u| *u > now);
        live(self.pin_backoff.get(&pin_id(slot.pin)))
            || live(self.key_backoff.get(&(slot.repo.to_string(), slot.key)))
            || live(self.pr_backoff.get(&slot.pr_id()))
    }

    /// Start `kind` of backoff for `slot` until `now +` [`backoff`].
    pub fn back_off(&mut self, kind: Backoff, slot: &KeySlot<'_>, now: DateTime<Utc>) {
        let until = now + backoff();
        match kind {
            Backoff::None => {}
            Backoff::Pin => {
                self.pin_backoff.insert(pin_id(slot.pin), until);
            }
            Backoff::Key => {
                self.key_backoff
                    .insert((slot.repo.to_string(), slot.key), until);
            }
            Backoff::Pr => {
                self.pr_backoff.insert(slot.pr_id(), until);
            }
        }
    }

    /// Drop expired backoffs and, when `current` is given, `repo`'s per-PR
    /// backoffs whose `(pr, head)` is not in it (critique A10).
    pub fn prune(
        &mut self,
        repo: &str,
        now: DateTime<Utc>,
        current: Option<&HashSet<(u64, String)>>,
    ) {
        self.pin_backoff.retain(|_, until| *until > now);
        self.key_backoff.retain(|_, until| *until > now);
        self.pr_backoff.retain(|(r, pr, head, _), until| {
            *until > now && (r != repo || current.is_none_or(|c| c.contains(&(*pr, head.clone()))))
        });
    }

    /// Records kept for `path` because their save failed.
    #[must_use]
    pub fn unsaved(&self, path: &Path) -> &[AttemptRecord] {
        self.unsaved.get(path).map_or(&[], Vec::as_slice)
    }

    /// Remember `records` for `path` after a failed save, or forget them
    /// after a successful one (`records` empty).
    pub fn set_unsaved(&mut self, path: &Path, records: Vec<AttemptRecord>) {
        if records.is_empty() {
            self.unsaved.remove(path);
        } else {
            self.unsaved.insert(path.to_path_buf(), records);
        }
    }
}

/// The process-wide [`LaneMemory`] the real lane uses.
pub fn process_memory() -> &'static Mutex<LaneMemory> {
    static MEMORY: std::sync::OnceLock<Mutex<LaneMemory>> = std::sync::OnceLock::new();
    MEMORY.get_or_init(Default::default)
}

/// Lock `memory`, recovering a poisoned lock (the memory is a cache).
pub fn lock(memory: &Mutex<LaneMemory>) -> std::sync::MutexGuard<'_, LaneMemory> {
    memory
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An [`InstructionSource`] that reads each pinned file once per
/// [`LaneMemory`] (content at a full 40-hex commit is immutable) and each
/// repo's visibility once per pass (critique A9). Only successful reads are
/// cached: a failed read is retried after its backoff.
pub struct CachedInstructions<'a> {
    inner: &'a dyn InstructionSource,
    memory: &'a Mutex<LaneMemory>,
    visibility: RefCell<HashMap<String, Option<bool>>>,
}

impl<'a> CachedInstructions<'a> {
    #[must_use]
    pub fn new(inner: &'a dyn InstructionSource, memory: &'a Mutex<LaneMemory>) -> Self {
        Self {
            inner,
            memory,
            visibility: RefCell::default(),
        }
    }
}

impl InstructionSource for CachedInstructions<'_> {
    fn fetch(&self, pin: &Pin) -> Result<String, String> {
        let id = pin_id(pin);
        if let Some(body) = lock(self.memory).pins.get(&id) {
            return Ok(body.clone());
        }
        let body = self.inner.fetch(pin)?;
        let immutable = pin.commit.len() == 40 && pin.commit.bytes().all(|b| b.is_ascii_hexdigit());
        if immutable {
            lock(self.memory).pins.insert(id, body.clone());
        }
        Ok(body)
    }

    fn repo_is_private(&self, repo: &str) -> Option<bool> {
        if let Some(v) = self.visibility.borrow().get(repo) {
            return *v;
        }
        let v = self.inner.repo_is_private(repo);
        self.visibility.borrow_mut().insert(repo.to_string(), v);
        v
    }
}

/// One key's attempt state for the status report: its counted attempts in
/// the window at `now`, oldest first, and when it is released if capped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyAttempts {
    pub counted: Vec<AttemptRecord>,
    pub capped_until: Option<DateTime<Utc>>,
}

/// [`KeyAttempts`] for `(repo, pr, head, key)` from `ledger` — the same
/// [`Ledger::counted`] / [`Ledger::capped_until`] the lane gates on.
#[must_use]
pub fn key_attempts(
    ledger: &Ledger,
    repo: &str,
    pr: u64,
    head_sha: &str,
    key: Key,
    now: DateTime<Utc>,
) -> KeyAttempts {
    KeyAttempts {
        counted: ledger
            .counted(repo, pr, head_sha, key, now)
            .into_iter()
            .cloned()
            .collect(),
        capped_until: ledger.capped_until(repo, pr, head_sha, key, now),
    }
}
