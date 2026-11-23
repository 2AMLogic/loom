//! The weighted role draw (#11726) through the real resolver: config on disk,
//! shipped role/runtime manifests, real availability.
//!
//! The regression the issue names (2026-10-12): Judge, Curator and Champion drew Codex, every tool
//! call was hook-denied, the tick exited 0 and the Claude entry never fired.
//! Here a launched attempt that fails is recorded exactly as
//! `role_runner::launch::hold_feed` records it, and the next role tick must
//! draw from the attempt-2 table and land on Claude.
use super::role_draw::{self, attempts};
use super::tests::{fixture, provision_claude_pool, write_config, ClearedRuntimeEnv};
use super::{
    resolve_for_dispatch, resolve_runtime_for, Decision, DispatchContext, Intent, PreferenceSource,
};
use serde_json::json;
use std::path::Path;

/// A bare-metal Codex account under the guard's isolated profile root.
fn provision_codex_account(name: &str) {
    let root = std::env::var_os("LOOM_CODEX_PROFILE_ROOT").expect("ClearedRuntimeEnv sets it");
    std::fs::create_dir_all(Path::new(&root).join(name)).unwrap();
}

struct DrawEnv {
    _cleared: ClearedRuntimeEnv,
}

impl DrawEnv {
    fn new() -> Self {
        let cleared = ClearedRuntimeEnv::new();
        attempts::reset_for_tests();
        Self { _cleared: cleared }
    }
}

impl Drop for DrawEnv {
    fn drop(&mut self) {
        attempts::reset_for_tests();
    }
}

fn role_tick() -> DispatchContext {
    DispatchContext {
        intent: Intent::Probe,
        role_tick: true,
    }
}

/// `(runtime, draw record)` of one curator role-tick resolution.
fn curator_tick(root: &Path) -> (Option<String>, role_draw::RoleDrawRecord) {
    let Decision::Preference {
        source,
        resolution,
        draw,
        ..
    } = resolve_runtime_for(root, "curator", None, 1_800_000_000, role_tick()).unwrap()
    else {
        panic!("expected the draw to decide");
    };
    assert_eq!(source, PreferenceSource::RoleDraw);
    let runtime = resolution.chosen.map(|c| c.admitted.runtime);
    (runtime, *draw.expect("a draw record"))
}

fn workspace(runtimes: &serde_json::Value) -> tempfile::TempDir {
    let dir = fixture();
    provision_claude_pool(dir.path(), 2);
    provision_codex_account("bare-metal");
    write_config(dir.path(), &json!({ "runtimes": runtimes }));
    dir
}

#[test]
#[serial_test::serial]
fn a_failed_codex_attempt_is_followed_by_the_attempt_two_table() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({
        // The ordered list the incident ran on; the draw must replace it.
        "rolePreference": {"curator": ["codex", "claude"]},
        "roleDraw": {"curator": [
            {"attempt": 1, "draw": {"codex": 100}},
            {"attempt": 2, "draw": {"claude/fable": 100}},
        ]},
    }));
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("codex"), "{draw:?}");
    assert_eq!((draw.attempt, draw.table_attempt, draw.pick.as_deref()), (1, 1, Some("codex")));
    // The tick launched, every tool call was denied: a failed attempt.
    attempts::record_failure(dir.path(), "curator");
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("claude"));
    assert_eq!((draw.attempt, draw.table_attempt), (2, 2));
    assert_eq!(
        (draw.pick.as_deref(), draw.model.as_deref()),
        (Some("claude/fable"), Some("fable"))
    );
    // A success puts the next tick back on attempt 1.
    attempts::record_success(dir.path(), "curator");
    assert_eq!(curator_tick(dir.path()).1.attempt, 1);
}

#[test]
#[serial_test::serial]
fn a_sweep_dispatch_always_draws_attempt_one_and_carries_the_drawn_model() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"curator": {
        "1": {"claude/opus": 100}, "2": {"claude/fable": 100}}}}));
    attempts::record_failure(dir.path(), "curator");
    let admitted = resolve_for_dispatch(dir.path(), "curator", None)
        .unwrap()
        .admitted
        .unwrap();
    let stamp = admitted.preference.unwrap();
    assert_eq!(stamp.model.as_deref(), Some("opus"));
    let draw = stamp.draw.unwrap();
    assert_eq!(draw.attempt, 1);
    assert!(stamp.marker.contains("source=role-draw"), "{}", stamp.marker);
    assert!(stamp.marker.contains("pick:claude/opus"), "{}", stamp.marker);
}

#[test]
#[serial_test::serial]
fn an_unavailable_pick_is_passed_over_within_the_same_table() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"curator": [
        {"attempt": 1, "draw": {"codex": 1000, "claude": 1}}]}}));
    // No Claude pool: whatever the roll, only Codex can serve.
    provision_claude_pool(dir.path(), 0);
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("codex"));
    assert_eq!(draw.pick.as_deref(), Some("codex"));
    // And the other way round: Codex excluded on this host, Claude wins.
    let local = dir.path().join(".loom-local");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("local.json"),
        json!({"runtimes": {"excludeProviders": ["codex"]}}).to_string(),
    )
    .unwrap();
    provision_claude_pool(dir.path(), 2);
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime.as_deref(), Some("claude"));
    assert_eq!(draw.table.len(), 1);
    assert!(draw.off[0].starts_with("codex: excluded"), "{:?}", draw.off);
}

#[test]
#[serial_test::serial]
fn a_table_empty_after_exclusions_fails_closed_with_a_diagnostic() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({
        "excludeProviders": ["codex"],
        "roleDraw": {"curator": [{"attempt": 1, "draw": {"codex": 100}}]},
    }));
    let (runtime, draw) = curator_tick(dir.path());
    assert_eq!(runtime, None);
    assert!(draw.table.is_empty());
    let error = resolve_for_dispatch(dir.path(), "curator", None).unwrap_err();
    assert!(error.reason.contains("EMPTY after exclusions"), "{}", error.reason);
}

#[test]
#[serial_test::serial]
fn without_a_role_draw_table_role_preference_decides_unchanged() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({
        "rolePreference": {"curator": ["codex", "claude"]},
        "roleDraw": {"guide": [{"attempt": 1, "draw": {"claude": 1}}]},
    }));
    let Decision::Preference {
        source,
        resolution,
        draw,
        ..
    } = resolve_runtime_for(dir.path(), "curator", None, 0, role_tick()).unwrap()
    else {
        panic!("expected the preference list");
    };
    assert_eq!(source, PreferenceSource::RolePreference);
    assert!(draw.is_none());
    assert_eq!(resolution.chosen.unwrap().admitted.runtime, "codex");
    // A role whose only table starts at attempt 2 keeps its list on attempt 1.
    write_config(
        dir.path(),
        &json!({"runtimes": {"rolePreference": {"curator": ["claude"]},
            "roleDraw": {"curator": {"2": {"codex": 1}}}}}),
    );
    let decision = resolve_runtime_for(dir.path(), "curator", None, 0, role_tick()).unwrap();
    assert!(matches!(
        decision,
        Decision::Preference {
            source: PreferenceSource::RolePreference,
            ..
        }
    ));
}

#[test]
#[serial_test::serial]
fn an_operator_pin_still_wins_over_a_draw() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"curator": {"1": {"codex": 1}}}}));
    std::env::set_var("LOOM_RUNTIME_CURATOR", "claude");
    let decision = resolve_runtime_for(dir.path(), "curator", None, 0, role_tick());
    std::env::remove_var("LOOM_RUNTIME_CURATOR");
    assert!(matches!(decision.unwrap(), Decision::Static { .. }));
}

/// One draw of `role` on a role tick: `(runtime, model profile, record)`.
fn tick(root: &Path, role: &str) -> (Option<String>, role_draw::RoleDrawRecord) {
    let Decision::Preference {
        resolution, draw, ..
    } = resolve_runtime_for(root, role, None, 1_800_000_000, role_tick()).unwrap()
    else {
        panic!("expected the draw to decide");
    };
    (resolution.chosen.map(|c| c.admitted.runtime), *draw.expect("a draw record"))
}

/// #14944: an API-key pool whose every account holds its declared
/// `maxConcurrent` is unavailable **at draw time**, the way a Codex tap with
/// no idle seat is. Before, `availability` counted the account as selectable,
/// a table renormalised onto the capped pick (98% on 2026-11-22) drew it on
/// nearly every tick, and each launch was refused at 78 (`stage=credential`)
/// with the tick lost.
#[test]
#[serial_test::serial]
fn a_pick_whose_api_key_pool_is_at_its_concurrency_cap_is_passed_over() {
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"builder": [
        {"attempt": 1, "draw": {"opencode": 1000, "claude": 1}}]}}));
    super::unprovisioned_tests::provision_zai_pool(dir.path(), true);
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::limits::set_max_concurrent(&pool, "zai", "alpha", Some(1)).unwrap();

    // Under the cap: the heavy pick launches.
    let (runtime, draw) = role_draw::with_seed(7, || tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("opencode"), "{draw:?}");
    assert!(draw.passed_over.is_empty(), "{draw:?}");

    // One live spawn holds the only slot: same seed, same first roll, but the
    // pick is passed over and the row renormalises onto Claude.
    let lease = crate::api_keys_pool::inflight::register(&pool, "zai", "alpha").unwrap();
    let (runtime, draw) = role_draw::with_seed(7, || tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("claude"), "{draw:?}");
    assert_eq!(draw.pick.as_deref(), Some("claude"));
    assert_eq!(draw.steps.len(), 2, "one roll for the capped pick, one for claude: {draw:?}");
    let [skip] = draw.passed_over.as_slice() else {
        panic!("exactly the capped pick is passed over: {draw:?}");
    };
    assert!(
        skip.starts_with("opencode: unavailable(api_keys:zai: pool zai at cap"),
        "{skip}"
    );
    // A skip before spawn is not a failed attempt.
    assert_eq!(attempts::current(dir.path(), "builder"), 1);

    // The slot frees: the pick is drawable again.
    lease.release();
    let (runtime, _) = role_draw::with_seed(7, || tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("opencode"));
}

/// #14944: with nothing else in the row, a capped pool is a pre-spawn skip
/// (no pick), never a launch that is then refused.
#[test]
#[serial_test::serial]
fn a_row_holding_only_a_capped_pool_draws_nothing() {
    let _env = DrawEnv::new();
    let dir =
        workspace(&json!({"roleDraw": {"builder": [{"attempt": 1, "draw": {"opencode": 1}}]}}));
    super::unprovisioned_tests::provision_zai_pool(dir.path(), true);
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::limits::set_max_concurrent(&pool, "zai", "alpha", Some(1)).unwrap();
    let _lease = crate::api_keys_pool::inflight::register(&pool, "zai", "alpha").unwrap();
    let (runtime, draw) = tick(dir.path(), "builder");
    assert_eq!((runtime, draw.pick.as_deref()), (None, None), "{draw:?}");
}

/// One **dispatching** role tick, the way `role_runner::runtime_preflight`
/// resolves: the only draw that holds the pool slot it picks (#14983).
fn dispatch_tick(root: &Path, role: &str) -> (Option<String>, role_draw::RoleDrawRecord) {
    let context = DispatchContext {
        intent: Intent::Dispatch,
        role_tick: true,
    };
    let Decision::Preference {
        resolution, draw, ..
    } = resolve_runtime_for(root, role, None, 1_800_000_000, context).unwrap()
    else {
        panic!("expected the draw to decide");
    };
    (resolution.chosen.map(|c| c.admitted.runtime), *draw.expect("a draw record"))
}

/// #14983: a role-runner tick admits several runs of one role before any of
/// them has acquired its pool account (9 Champion runs in ~200 ms on
/// 2026-11-22). The #14944 cap check is a read, so every draw saw the one slot
/// free and picked the capped tap; one launch got the account and the rest
/// were refused at 78 with the tick lost. Each dispatching draw now holds the
/// slot it picks: N concurrent draws against one free slot yield exactly one
/// pick of the pool, and the others draw the rest of their row.
#[test]
#[serial_test::serial]
fn concurrent_draws_against_one_free_slot_pick_the_pool_exactly_once() {
    use crate::api_keys_pool::{inflight, reservation};
    const DRAWS: usize = 9;
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"builder": [
        {"attempt": 1, "draw": {"opencode": 1000, "claude": 1}}]}}));
    super::unprovisioned_tests::provision_zai_pool(dir.path(), true);
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::limits::set_max_concurrent(&pool, "zai", "alpha", Some(1)).unwrap();

    let barrier = std::sync::Barrier::new(DRAWS);
    let ticks: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..DRAWS)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    // Seed 7 rolls the heavy pick first on every thread.
                    role_draw::with_seed(7, || dispatch_tick(dir.path(), "builder"))
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let picks = |runtime: &str| {
        ticks
            .iter()
            .filter(|(chosen, _)| chosen.as_deref() == Some(runtime))
            .count()
    };
    assert_eq!(picks("opencode"), 1, "{ticks:?}");
    assert_eq!(picks("claude"), DRAWS - 1, "the rest draw the remaining candidate: {ticks:?}");
    for (_, draw) in ticks
        .iter()
        .filter(|(chosen, _)| chosen.as_deref() == Some("claude"))
    {
        let [skip] = draw.passed_over.as_slice() else {
            panic!("exactly the reserved pick is passed over: {draw:?}");
        };
        assert!(
            skip.starts_with("opencode: unavailable(api_keys:zai: pool zai at cap"),
            "{skip}"
        );
    }
    // A skip before spawn is not a failed attempt.
    assert_eq!(attempts::current(dir.path(), "builder"), 1);
    assert_eq!(reservation::live_count(&pool, "zai"), 1);

    // The one launch acquires: the reservation becomes its lease, and the
    // pool stays at cap for the next draw.
    let selected = crate::api_keys_pool::select_api_key(dir.path(), "zai", None).unwrap();
    assert_eq!(reservation::live_count(&pool, "zai"), 0);
    assert_eq!(inflight::live_count(&pool, "zai", "alpha"), 1);
    let (runtime, _) = role_draw::with_seed(7, || dispatch_tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("claude"));
    assert_eq!(reservation::live_count(&pool, "zai"), 0, "a passed-over pick holds nothing");

    // The run ends: the pick is drawable again.
    selected.lease.unwrap().release();
    let (runtime, _) = role_draw::with_seed(7, || dispatch_tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("opencode"));
}

/// #14983: a pick that never launches does not keep its slot. A probe (the
/// explain view, pool pre-flight) holds none to begin with, and a dispatching
/// tick's unconsumed reservation stops counting when it expires.
#[test]
#[serial_test::serial]
fn a_pick_that_never_launches_gives_its_slot_back() {
    use crate::api_keys_pool::reservation;
    let _env = DrawEnv::new();
    let dir = workspace(&json!({"roleDraw": {"builder": [
        {"attempt": 1, "draw": {"opencode": 1000, "claude": 1}}]}}));
    super::unprovisioned_tests::provision_zai_pool(dir.path(), true);
    let pool = crate::api_keys_pool::paths::per_repo_api_keys_dir(dir.path());
    crate::api_keys_pool::limits::set_max_concurrent(&pool, "zai", "alpha", Some(1)).unwrap();

    let (runtime, _) = role_draw::with_seed(7, || tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("opencode"));
    assert_eq!(reservation::live_count(&pool, "zai"), 0, "a probe holds nothing");

    let (runtime, _) = role_draw::with_seed(7, || dispatch_tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("opencode"));
    assert_eq!(reservation::live_count(&pool, "zai"), 1);
    // Held: a second draw, and a read-only probe, both see the pool at cap.
    let (runtime, _) = role_draw::with_seed(7, || dispatch_tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("claude"));
    let (runtime, draw) = role_draw::with_seed(7, || tick(dir.path(), "builder"));
    assert_eq!(runtime.as_deref(), Some("claude"), "{draw:?}");

    // No launch ever acquires. Past the window the slot is free again.
    std::env::set_var(reservation::RESERVATION_STALE_SECS_ENV, "1");
    std::thread::sleep(std::time::Duration::from_millis(2100));
    let (runtime, _) = role_draw::with_seed(7, || dispatch_tick(dir.path(), "builder"));
    std::env::remove_var(reservation::RESERVATION_STALE_SECS_ENV);
    assert_eq!(runtime.as_deref(), Some("opencode"));
}
