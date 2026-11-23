//! Draw-time reservation tests (#14983).

use super::*;
use crate::api_keys_pool::registry::add;
use crate::api_keys_pool::select::{health_in, select_api_key};
use crate::api_keys_pool::{limits, paths};

/// A PID above every platform's `pid_max`: never a live process.
const DEAD_PID: u32 = u32::MAX;

/// A workspace whose per-repo pool holds `accounts` as `(name, cap)`.
fn workspace_with(accounts: &[(&str, Option<u32>)]) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let root = paths::per_repo_api_keys_dir(tmp.path());
    for (name, cap) in accounts {
        add(&root, "zai", name, "ZAI_API_KEY", "fake-key", false).unwrap();
        if cap.is_some() {
            limits::set_max_concurrent(&root, "zai", name, *cap).unwrap();
        }
    }
    (tmp, root)
}

fn held(reserve: Reserve) -> Reservation {
    match reserve {
        Reserve::Held(slot) => slot,
        other => panic!("expected a held slot, got {other:?}"),
    }
}

#[test]
fn an_uncapped_pool_holds_nothing_and_writes_nothing() {
    let (tmp, root) = workspace_with(&[("alpha", None)]);
    assert!(matches!(try_reserve(tmp.path(), "zai"), Reserve::Unbounded));
    assert!(!reserved_dir(&root, "zai").exists());
    // One uncapped selectable account makes the whole pool unbounded.
    let (tmp, root) = workspace_with(&[("alpha", Some(1)), ("beta", None)]);
    assert!(matches!(try_reserve(tmp.path(), "zai"), Reserve::Unbounded));
    assert_eq!(live_count(&root, "zai"), 0);
}

#[test]
fn the_last_free_slot_is_held_once_and_released_explicitly() {
    let (tmp, root) = workspace_with(&[("alpha", Some(1))]);
    let slot = held(try_reserve(tmp.path(), "zai"));
    assert_eq!(live_count(&root, "zai"), 1);
    let Reserve::AtCap {
        slots,
        in_flight,
        reserved,
    } = try_reserve(tmp.path(), "zai")
    else {
        panic!("the only slot is reserved");
    };
    assert_eq!((slots, in_flight, reserved), (1, 0, 1));
    // The pick will not launch after all: the slot is free at once.
    slot.release();
    assert_eq!(live_count(&root, "zai"), 0);
    held(try_reserve(tmp.path(), "zai"));
}

/// The property #14983 asks for: N concurrent holds against K free slots
/// yield exactly K.
#[test]
fn concurrent_reserves_never_exceed_the_free_slots() {
    for (accounts, free) in [
        (vec![("alpha", Some(1))], 1usize),
        (vec![("alpha", Some(1)), ("beta", Some(2))], 3),
    ] {
        let (tmp, root) = workspace_with(&accounts);
        let barrier = std::sync::Barrier::new(12);
        let won = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..12)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        matches!(try_reserve(tmp.path(), "zai"), Reserve::Held(_))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .filter(|won| *won)
                .count()
        });
        assert_eq!(won, free, "{accounts:?}");
        assert_eq!(live_count(&root, "zai"), free);
    }
}

/// A live spawn's slot is not reservable, and an account over a lowered cap
/// holds only its own slots.
#[test]
fn live_spawns_count_against_the_slots() {
    let (tmp, root) = workspace_with(&[("alpha", Some(1)), ("beta", Some(1))]);
    let _a = inflight::register(&root, "zai", "alpha").unwrap();
    let _a2 = inflight::register(&root, "zai", "alpha").unwrap();
    held(try_reserve(tmp.path(), "zai"));
    assert!(matches!(
        try_reserve(tmp.path(), "zai"),
        Reserve::AtCap {
            slots: 2,
            in_flight: 1,
            reserved: 1
        }
    ));
}

/// An unused reservation is released: its owner died.
#[cfg(unix)]
#[test]
fn a_dead_owners_reservation_is_not_counted_and_is_reaped() {
    let (tmp, root) = workspace_with(&[("alpha", Some(1))]);
    let slot = held(try_reserve_in(&root, "zai", DEAD_PID, &limits::ConfiguredLimits::EMPTY));
    assert!(slot.path().exists());
    assert_eq!(live_count(&root, "zai"), 0);
    assert!(!slot.path().exists(), "dead reservation was not reaped");
    held(try_reserve(tmp.path(), "zai"));
}

/// An unused reservation is released: nothing acquired within the window.
#[test]
fn an_unconsumed_reservation_expires() {
    let (tmp, root) = workspace_with(&[("alpha", Some(1))]);
    let slot = held(try_reserve(tmp.path(), "zai"));
    assert!(matches!(try_reserve(tmp.path(), "zai"), Reserve::AtCap { .. }));
    let body = std::fs::read_to_string(slot.path()).unwrap();
    let mut holder: Holder = serde_json::from_str(&body).unwrap();
    holder.started_at -= DEFAULT_STALE_SECS + 1;
    std::fs::write(slot.path(), serde_json::to_string(&holder).unwrap()).unwrap();
    assert_eq!(live_count(&root, "zai"), 0);
    assert!(!slot.path().exists(), "expired reservation was not reaped");
    held(try_reserve(tmp.path(), "zai"));
}

/// A torn file proves nothing about a launch on its way: reaped, not counted.
#[test]
fn an_unparsable_reservation_is_reaped() {
    let (tmp, root) = workspace_with(&[("alpha", Some(1))]);
    let slot = held(try_reserve(tmp.path(), "zai"));
    std::fs::write(slot.path(), "{torn").unwrap();
    assert_eq!(live_count(&root, "zai"), 0);
    assert!(!slot.path().exists());
}

/// The handover: the ladder does not count the reservation against the launch
/// it was taken for, and the acquire retires it, so the slot is the lease's.
#[test]
fn the_launchs_acquire_ignores_and_then_consumes_the_reservation() {
    let (tmp, root) = workspace_with(&[("alpha", Some(1))]);
    let slot = held(try_reserve(tmp.path(), "zai"));
    let selected = select_api_key(tmp.path(), "zai", None).expect("the reserved slot is ours");
    assert_eq!(selected.name, "alpha");
    assert!(!slot.path().exists(), "the acquire did not retire the reservation");
    assert_eq!(live_count(&root, "zai"), 0);
    assert_eq!(inflight::live_count(&root, "zai", "alpha"), 1);
    assert!(matches!(
        try_reserve(tmp.path(), "zai"),
        Reserve::AtCap {
            slots: 1,
            in_flight: 1,
            reserved: 0
        }
    ));
    // The run ends: the slot is reservable again.
    selected.lease.unwrap().release();
    held(try_reserve(tmp.path(), "zai"));
}

/// Two reservations, one acquire: the older one is retired, the newer stays.
#[test]
fn an_acquire_retires_the_oldest_reservation_only() {
    let (tmp, root) = workspace_with(&[("alpha", Some(2))]);
    let first = held(try_reserve(tmp.path(), "zai"));
    let second = held(try_reserve(tmp.path(), "zai"));
    let body = std::fs::read_to_string(first.path()).unwrap();
    let mut holder: Holder = serde_json::from_str(&body).unwrap();
    holder.started_at -= 5;
    std::fs::write(first.path(), serde_json::to_string(&holder).unwrap()).unwrap();
    select_api_key(tmp.path(), "zai", None).unwrap();
    assert!(!first.path().exists());
    assert!(second.path().exists());
    assert_eq!(live_count(&root, "zai"), 1);
}

#[test]
fn health_reports_the_slots_a_draw_could_still_take() {
    let slots = |root: &Path| {
        health_in(&[root.to_path_buf()], Some("zai"), &limits::ConfiguredLimits::EMPTY).unwrap()[0]
            .unreserved_slots
    };
    let (tmp, root) = workspace_with(&[("alpha", Some(2))]);
    assert_eq!(slots(&root), Some(2));
    let slot = held(try_reserve(tmp.path(), "zai"));
    assert_eq!(slots(&root), Some(1));
    let _lease = inflight::register(&root, "zai", "alpha").unwrap();
    assert_eq!(slots(&root), Some(0));
    slot.release();
    assert_eq!(slots(&root), Some(1));
    // No finite count once an uncapped account is selectable.
    add(&root, "zai", "beta", "ZAI_API_KEY", "fake-key", false).unwrap();
    assert_eq!(slots(&root), None);
}

/// #14932: a cap declared only in the workspace's config reserves like a
/// file-declared one, so a role draw holds the slot the launched child's
/// selection will then enforce.
#[test]
fn a_config_declared_cap_is_reserved_against() {
    let (tmp, root) = workspace_with(&[("alpha", None)]);
    assert!(matches!(try_reserve(tmp.path(), "zai"), Reserve::Unbounded));
    std::fs::write(
        tmp.path().join(".loom").join("config.json"),
        serde_json::json!({
            "runtimes": { "apiKeyLimits": { "zai": { "alpha": { "maxConcurrent": 1 } } } }
        })
        .to_string(),
    )
    .unwrap();
    assert!(matches!(try_reserve(tmp.path(), "zai"), Reserve::Held(_)));
    assert!(matches!(
        try_reserve(tmp.path(), "zai"),
        Reserve::AtCap {
            slots: 1,
            in_flight: 0,
            reserved: 1
        }
    ));
    assert!(!root.join("zai").join(limits::LIMITS_FILE).exists());
}

#[test]
fn an_unusable_store_degrades_open() {
    let (tmp, root) = workspace_with(&[("alpha", Some(1))]);
    std::fs::write(reserved_dir(&root, "zai"), "not a directory").unwrap();
    assert!(matches!(try_reserve(tmp.path(), "zai"), Reserve::Unbounded));
    assert_eq!(live_count(&root, "zai"), 0);
}
