//! Draw-time **reservations** on a capped API-key pool (#14983).
//!
//! # The gap this closes
//!
//! #14944 made the role draw treat a pool with no account under its
//! `maxConcurrent` as unavailable. That check is a read. The slot itself is
//! only taken later, by the launched child, when
//! [`super::select::select_api_key_for`] registers its [`super::inflight`]
//! lease. A role-runner tick admits several runs of one role within
//! milliseconds, so every draw ran before any child had acquired, every draw
//! saw the slot free, and several picked the capped tap. One launch got the
//! account; the rest were refused at 78 (`stage=credential`) and lost the tick.
//!
//! A reservation is the draw's own hold on one slot of the pool, taken under
//! the same `.inflight.lock` the selection ladder counts and registers under,
//! so N concurrent draws against K free slots yield at most K picks.
//!
//! # Lifecycle
//!
//! 1. [`try_reserve`] (role-tick draw, dispatch intent): under the lock, count
//!    the pool's free slots (declared caps minus live leases minus live
//!    reservations). Room: write one file under `<provider>/.reserved/`.
//!    None: the draw passes the tap over and continues down its row.
//! 2. The launched child acquires through the ladder, which **does not count
//!    reservations** (it would otherwise refuse the very launch the slot is
//!    held for) and, once its lease is registered, calls [`consume_one`] to
//!    retire the oldest reservation. Lease and reservation are both written
//!    and removed under the lock, so a slot is never counted twice or zero
//!    times across the handover.
//! 3. A reservation nothing consumes (the tick bailed out before spawn, the
//!    child died before the credential stage) stops counting when its owner
//!    PID dies or after [`DEFAULT_STALE_SECS`]. A caller that knows at once
//!    the launch is off calls [`Reservation::release`].
//!
//! # Pool-level, and consumed by any acquire
//!
//! The draw picks a pool, not an account (the ladder's rotation picks the
//! account at launch), so a reservation is held against the pool's summed
//! free slots. It is not matched to one child either: any acquire retires the
//! oldest one. That keeps the child side free of new plumbing, and it is exact
//! as long as every acquire was preceded by a reservation, which holds for
//! role ticks. An acquire that drew without reserving (a sweep phase, a manual
//! launch) can retire a role tick's reservation; the ladder still enforces the
//! cap, so the cost is one refused launch, never a breach.
//!
//! # Degrades open
//!
//! Like [`super::inflight`], this is Loom's own bookkeeping: an unusable
//! store (no lock, unwritable directory) yields [`Reserve::Unbounded`] and the
//! draw proceeds exactly as it did before this module. A pool in which any
//! selectable account declares no cap has no slot count to hold and is
//! `Unbounded` too, with nothing written.

use std::path::{Path, PathBuf};

use crate::tokens_pool::locking::MkdirLock;

use super::inflight;
use super::paths::{provider_dir, resolve_provider_root};
use super::registry::{list_provider, ApiKeyAccount};

/// Directory under a provider's pool dir holding one file per reservation.
/// Dot-prefixed, so it can never collide with an account file.
pub const RESERVED_DIR: &str = ".reserved";

/// Env override (whole seconds, must parse `> 0`) for [`DEFAULT_STALE_SECS`].
pub const RESERVATION_STALE_SECS_ENV: &str = "LOOM_API_KEY_RESERVATION_STALE_SECS";

/// How long an unconsumed reservation counts: 2 minutes. The window it covers
/// is draw to the child's credential stage, which is seconds. Longer would
/// hold a slot for a launch that never happened; shorter than the slowest
/// real launch would reopen the race this module closes.
pub const DEFAULT_STALE_SECS: u64 = 120;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Holder {
    pid: u32,
    started_at: u64,
}

/// A held slot. **Not released on drop**: the draw that took it returns long
/// before the child that will consume it acquires.
#[derive(Debug)]
pub struct Reservation {
    path: PathBuf,
}

impl Reservation {
    /// The reservation file, for a caller that wants to log or assert on it.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Give the slot back now: the pick this was taken for will not launch.
    /// Best-effort; a failure means it ages out instead.
    pub fn release(self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What [`try_reserve`] decided.
#[derive(Debug)]
pub enum Reserve {
    /// A slot was free and is now held.
    Held(Reservation),
    /// Every slot is held by a live spawn or by another draw's reservation.
    AtCap {
        /// Declared slots across the selectable accounts.
        slots: usize,
        /// Of those, held by live spawns.
        in_flight: usize,
        /// Of those, held by other draws' reservations.
        reserved: usize,
    },
    /// Nothing to hold: the pool is uncapped, has no selectable account (the
    /// availability read reports that), or the store is unusable.
    Unbounded,
}

#[must_use]
fn resolve_stale() -> u64 {
    std::env::var(RESERVATION_STALE_SECS_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|secs| *secs > 0)
        .unwrap_or(DEFAULT_STALE_SECS)
}

fn reserved_dir(root: &Path, provider: &str) -> PathBuf {
    provider_dir(root, provider).join(RESERVED_DIR)
}

/// Live reservation files, oldest first, reaping dead-owner, over-age and
/// unparsable ones as it reads. Never fails: an unreadable store is empty.
fn live(root: &Path, provider: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(reserved_dir(root, provider)) else {
        return Vec::new();
    };
    let now = super::bad_marks::epoch_now();
    let stale = resolve_stale();
    let mut held: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let holder = std::fs::read_to_string(&path)
            .ok()
            .and_then(|body| serde_json::from_str::<Holder>(&body).ok())
            .filter(|holder| {
                crate::live_claim::pid_is_live_process(holder.pid)
                    && now.saturating_sub(holder.started_at) < stale
            });
        match holder {
            Some(holder) => held.push((holder.started_at, path)),
            None => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    held.sort();
    held.into_iter().map(|(_, path)| path).collect()
}

/// How many live reservations `provider`'s pool under `root` holds.
#[must_use]
pub fn live_count(root: &Path, provider: &str) -> usize {
    live(root, provider).len()
}

/// `(declared slots, live spawns holding them)` summed over the selectable
/// accounts, or `None` when any of them declares no cap (the pool then has no
/// finite slot count) or none is selectable.
#[must_use]
pub fn capped_slots(
    root: &Path,
    provider: &str,
    accounts: &[ApiKeyAccount],
) -> Option<(usize, usize)> {
    let mut slots = 0usize;
    let mut in_flight = 0usize;
    let mut any = false;
    for account in accounts.iter().filter(|a| a.selectable()) {
        let cap = usize::try_from(account.max_concurrent?).unwrap_or(usize::MAX);
        let live = usize::try_from(inflight::live_count(root, provider, &account.name))
            .unwrap_or(usize::MAX);
        slots = slots.saturating_add(cap);
        // An account over its cap (the cap was lowered under live spawns)
        // holds its own slots, never a neighbour's.
        in_flight = in_flight.saturating_add(live.min(cap));
        any = true;
    }
    any.then_some((slots, in_flight))
}

/// Hold one slot of `provider`'s pool for a pick that is about to launch.
///
/// `workspace` is the workspace root; the pool root is resolved from it the
/// way the selection ladder resolves it, so the draw and the launched child
/// count the same directory.
#[must_use]
pub fn try_reserve(workspace: &Path, provider: &str) -> Reserve {
    let Ok(root) = resolve_provider_root(workspace, provider) else {
        return Reserve::Unbounded;
    };
    try_reserve_in(&root, provider, std::process::id())
}

/// [`try_reserve`] against an explicit pool root and owner PID: the seam
/// tests use to fabricate a dead owner.
#[must_use]
pub fn try_reserve_in(root: &Path, provider: &str, pid: u32) -> Reserve {
    let Ok(accounts) = list_provider(root, provider) else {
        return Reserve::Unbounded;
    };
    if !accounts.iter().any(|a| a.max_concurrent.is_some()) {
        return Reserve::Unbounded;
    }
    // The ladder's own lock: count-then-write here is atomic against a peer
    // draw and against a child's count-then-register.
    let Ok(_lock) = MkdirLock::acquire(&provider_dir(root, provider).join(".inflight.lock")) else {
        return Reserve::Unbounded;
    };
    let Some((slots, in_flight)) = capped_slots(root, provider, &accounts) else {
        return Reserve::Unbounded;
    };
    let reserved = live_count(root, provider);
    if in_flight.saturating_add(reserved) >= slots {
        return Reserve::AtCap {
            slots,
            in_flight,
            reserved,
        };
    }
    let dir = reserved_dir(root, provider);
    if std::fs::create_dir_all(&dir).is_err() {
        return Reserve::Unbounded;
    }
    super::registry::restrict_dir(&dir);
    let path = dir.join(format!("{pid}-{}.json", uuid::Uuid::new_v4()));
    let holder = Holder {
        pid,
        started_at: super::bad_marks::epoch_now(),
    };
    let written = serde_json::to_string(&holder)
        .ok()
        .and_then(|body| super::registry::write_secret(&path, &body).ok());
    match written {
        Some(()) => Reserve::Held(Reservation { path }),
        None => Reserve::Unbounded,
    }
}

/// Retire the oldest live reservation: a spawn has just registered the lease
/// that slot was held for. The caller holds the pool's `.inflight.lock`. A
/// no-op when nothing is reserved.
pub(super) fn consume_one(root: &Path, provider: &str) {
    if let Some(oldest) = live(root, provider).first() {
        let _ = std::fs::remove_file(oldest);
    }
}

#[cfg(test)]
#[path = "reservation_tests.rs"]
mod tests;
