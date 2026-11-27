//! Issue #14112: the reconcile pass moves a running private-clone seat onto
//! the daemon's release image, through a caller-supplied roll (the shared
//! fake stands in for the pass's own reads). Fake stale seats: "after a host
//! roll, no private seat keeps a stock image the release did not ship for
//! longer than one idle window".

use super::*;
use crate::tokens_pool::private_workspace::{AccountBusy, ImageUnavailable, Rolled};
use crate::tokens_pool::session_hold::OperatorHeld;
use crate::tokens_pool::session_lifecycle::release_session_image;
use skew::DaemonVersion;
use std::cell::{Cell, RefCell};

const LATEST: &str = "ghcr.io/2amlogic/loom-worker-session:latest";
const LEGACY_LATEST: &str = "ghcr.io/rjwalters/loom-worker-session:latest";

fn release(host: &str) -> String {
    release_session_image(host)
}

fn v(text: &str) -> DaemonVersion {
    DaemonVersion::parse(text).unwrap()
}

/// Alice's seat: running, private-clone, created from `image`, on a host
/// daemon of `host`.
fn seed_stale(lifecycle: &SessionLifecycle<Fake>, host: &str, image: &str) {
    let fake = lifecycle.runner();
    fake.seed(&container_name("alice"), true, false, Some(Path::new(private_workspace::REPO)));
    fake.mounts
        .lock()
        .unwrap()
        .insert(container_name("alice"), Vec::new());
    fake.containers
        .lock()
        .unwrap()
        .get_mut(&container_name("alice"))
        .unwrap()
        .image = Some(image.into());
    *fake.host_version.lock().unwrap() = Some(v(host));
}

/// What the pass next reads as the seat's image.
fn set_image(lifecycle: &SessionLifecycle<Fake>, image: impl Into<String>) {
    lifecycle
        .runner()
        .containers
        .lock()
        .unwrap()
        .get_mut(&container_name("alice"))
        .unwrap()
        .image = Some(image.into());
}

fn running_image(lifecycle: &SessionLifecycle<Fake>) -> String {
    lifecycle.runner().containers.lock().unwrap()[&container_name("alice")]
        .image
        .clone()
        .unwrap()
}

/// One pass over alice, positively configured private, with caller-supplied
/// private roll and recovery.
fn pass_with_roll(
    lifecycle: &mut SessionLifecycle<Fake>,
    env: &Env,
    private: &dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    recover: &PrivateRecovery<'_>,
    roll: &PrivateRoll<'_>,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<Outcome> {
    let index = AccountIndex::from_inventories(&[&env.accounts]);
    let fake = lifecycle.runner().clone();
    let registered = fake.registered.lock().unwrap().clone();
    let mut take = || fake.snapshot(&registered);
    let mut observe = PassSnapshot::new(&mut take);
    let inputs = PassInputs {
        recover_private: recover,
        roll_private: roll,
        host_version: fake.host_version.lock().unwrap().clone(),
        ..fake_inputs(&fake, &index, private, Path::new("/srv/checkouts"), &registered)
    };
    reconcile_accounts(lifecycle, &env.accounts, &inputs, &mut observe, state, now)
        .into_iter()
        .map(|o| o.outcome)
        .collect()
}

fn roll_pass(
    lifecycle: &mut SessionLifecycle<Fake>,
    env: &Env,
    private: &dyn Fn(&AccountDescriptor) -> anyhow::Result<bool>,
    roll: &PrivateRoll<'_>,
    state: &mut ReconcileState,
    now: u64,
) -> Vec<Outcome> {
    pass_with_roll(lifecycle, env, private, &|_, _, _| Ok(()), roll, state, now)
}

fn private() -> impl Fn(&AccountDescriptor) -> anyhow::Result<bool> {
    |_: &AccountDescriptor| Ok(true)
}

/// A roll that answers like the real one: a fresh container from `image`.
fn rolling<'a>(
    fake: Fake,
    asked: &'a Cell<usize>,
) -> impl Fn(&AccountDescriptor, &str, &dyn Fn() -> bool) -> Result<Rolled> + 'a {
    let fake = fake.clone();
    move |_: &AccountDescriptor, image: &str, _: &dyn Fn() -> bool| {
        asked.set(asked.get() + 1);
        fake.containers
            .lock()
            .unwrap()
            .get_mut(&container_name("alice"))
            .unwrap()
            .image = Some(image.into());
        Ok(Rolled::Recreated)
    }
}

#[test]
#[serial]
fn an_idle_private_seat_on_a_stale_stock_image_is_rolled_once() {
    for stale in [LATEST, "ghcr.io/2amlogic/loom-worker-session:0.20.37"] {
        let env = setup(&["alice"], &["alice"]);
        let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
        seed_stale(&lifecycle, "0.20.41", stale);
        let expected = release("0.20.41");
        let asked = Cell::new(0);
        let roll = rolling(lifecycle.runner().clone(), &asked);
        let mut state = ReconcileState::default();
        assert_eq!(
            roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 0),
            vec![Outcome::PrivateImageRecreated {
                from: stale.into(),
                to: expected.clone(),
            }]
        );
        assert_eq!(asked.get(), 1);
        assert_eq!(running_image(&lifecycle), expected);
        // On the release image now: confirmed, then left alone.
        assert_eq!(
            roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 60),
            vec![Outcome::Running]
        );
        assert_eq!(
            roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 120),
            vec![Outcome::Running]
        );
        assert_eq!(asked.get(), 1);
    }
}

#[test]
#[serial]
fn a_seat_on_the_legacy_repository_moves_to_the_release_repository() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LEGACY_LATEST);
    let asked = Cell::new(0);
    let roll = rolling(lifecycle.runner().clone(), &asked);
    let mut state = ReconcileState::default();
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 0),
        vec![Outcome::PrivateImageRecreated {
            from: LEGACY_LATEST.into(),
            to: "ghcr.io/2amlogic/loom-worker-session:0.20.41".into(),
        }]
    );
}

#[test]
#[serial]
fn a_seat_that_does_not_stay_up_after_its_roll_is_a_failed_start() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let asked = Cell::new(0);
    let roll = rolling(lifecycle.runner().clone(), &asked);
    let mut state = ReconcileState::default();
    let out = roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 0);
    assert!(matches!(&out[..], [Outcome::PrivateImageRecreated { .. }]), "{out:?}");
    lifecycle
        .runner()
        .containers
        .lock()
        .unwrap()
        .get_mut(&container_name("alice"))
        .unwrap()
        .running = false;
    let out = roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 60);
    assert!(
        matches!(&out[..], [Outcome::Failed { error, .. }] if error.contains("did not stay running")),
        "{out:?}"
    );
}

#[test]
#[serial]
fn a_busy_private_seat_is_deferred_never_interrupted_and_never_counted_a_failure() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let asked = Cell::new(0);
    let busy = |_: &AccountDescriptor, image: &str, _: &dyn Fn() -> bool| {
        asked.set(asked.get() + 1);
        assert_eq!(image, release("0.20.41"));
        Err(AccountBusy("private session is busy").into())
    };
    let mut state = ReconcileState::default();
    let deferred = vec![Outcome::PrivateImageDeferred {
        reason: DeferReason::Busy,
    }];
    for now in [0, 60, 120, 180] {
        assert_eq!(roll_pass(&mut lifecycle, &env, &private(), &busy, &mut state, now), deferred);
    }
    assert_eq!(asked.get(), 4, "re-checked every pass, never backed off");
    assert!(lifecycle.runner().mutations().is_empty(), "never touched");
    assert_eq!(running_image(&lifecycle), LATEST, "still on its image");
    // Idle at last: rolled.
    let roll = rolling(lifecycle.runner().clone(), &asked);
    let out = roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 240);
    assert!(matches!(&out[..], [Outcome::PrivateImageRecreated { .. }]), "{out:?}");
}

#[test]
#[serial]
fn an_unavailable_release_image_leaves_the_seat_running_and_backs_the_pull_off() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let expected = release("0.20.41");
    let asked = Cell::new(0);
    let missing = |_: &AccountDescriptor, _: &str, _: &dyn Fn() -> bool| {
        asked.set(asked.get() + 1);
        Err(ImageUnavailable(expected.clone()).into())
    };
    let mut state = ReconcileState::default();
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &private(), &missing, &mut state, 0),
        vec![Outcome::Running]
    );
    assert_eq!(asked.get(), 1);
    // Nothing was torn down, so the pull is retried: one pass skipped after
    // the first failure, two after the second, four after the third (#13493).
    let mut pulls = Vec::new();
    for pass_no in 1..=12u64 {
        let before = asked.get();
        let out = roll_pass(&mut lifecycle, &env, &private(), &missing, &mut state, pass_no * 60);
        assert_eq!(out, vec![Outcome::Running], "pass {pass_no}: no failure is counted");
        if asked.get() > before {
            pulls.push(pass_no);
        }
    }
    assert_eq!(pulls, [2, 5, 10], "pulls at passes 2, 5 and 10: {pulls:?}");
    assert_eq!(running_image(&lifecycle), LATEST, "the seat is still running");
    // The image is published: the next due pull succeeds and the seat is
    // rolled, with no daemon roll in between.
    let roll = rolling(lifecycle.runner().clone(), &asked);
    let mut rolled = None;
    for pass_no in 13..=40u64 {
        let out = roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, pass_no * 60);
        if matches!(&out[..], [Outcome::PrivateImageRecreated { .. }]) {
            rolled = Some(pass_no);
            break;
        }
    }
    assert_eq!(rolled, Some(19), "fourth pull after 8 skipped passes");
    assert_eq!(running_image(&lifecycle), expected);
}

#[test]
#[serial]
fn a_failed_replacement_is_attempted_once_per_pair_and_recovery_keeps_the_last_seen_image() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let rolls = Cell::new(0);
    let failing = |_: &AccountDescriptor, image: &str, _: &dyn Fn() -> bool| {
        rolls.set(rolls.get() + 1);
        Err(anyhow::anyhow!("docker run {image} failed").context(private_workspace::RollAttempted))
    };
    let hints = RefCell::new(Vec::new());
    let recover = |_: &AccountDescriptor, image: Option<&str>, _: &dyn Fn() -> bool| {
        hints.borrow_mut().push(image.map(str::to_owned));
        Ok(())
    };
    let mut state = ReconcileState::default();
    let mut run = |lifecycle: &mut SessionLifecycle<Fake>, now: u64| {
        pass_with_roll(lifecycle, &env, &private(), &recover, &failing, &mut state, now)
    };
    let out = run(&mut lifecycle, 0);
    assert!(
        matches!(&out[..], [Outcome::Failed { error, retry_at: 120 }] if error.contains("docker run")),
        "{out:?}"
    );
    assert!(matches!(run(&mut lifecycle, 60)[..], [Outcome::BackingOff { .. }]));
    assert_eq!(rolls.get(), 1);
    // If the seat disappears independently, the failed replacement never
    // became its last observed image.
    lifecycle
        .runner()
        .containers
        .lock()
        .unwrap()
        .remove(&container_name("alice"));
    assert_eq!(run(&mut lifecycle, 120), vec![Outcome::PrivateRecovered]);
    assert_eq!(*hints.borrow(), [Some(LATEST.to_owned())], "{:?}", hints.borrow());
    // The same (running, expected) pair is not attempted again while the seat
    // shows that stale image: a broken recreate must not churn it every pass.
    seed_stale(&lifecycle, "0.20.41", LATEST);
    assert_eq!(run(&mut lifecycle, 180), vec![Outcome::Running]);
    assert_eq!(run(&mut lifecycle, 240), vec![Outcome::Running]);
    assert_eq!(rolls.get(), 1);
    // A different stale image is another pair: attempted.
    seed_stale(&lifecycle, "0.20.41", LEGACY_LATEST);
    let out = run(&mut lifecycle, 300);
    assert!(matches!(&out[..], [Outcome::Failed { .. }]), "{out:?}");
    assert_eq!(rolls.get(), 2);
}

#[test]
#[serial]
fn a_private_seat_is_asked_to_roll_only_when_configured_private_and_off_the_release_image() {
    let never = |_: &AccountDescriptor, image: &str, _: &dyn Fn() -> bool| -> Result<Rolled> {
        bail!("the roll must not be asked for {image}")
    };
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let mut state = ReconcileState::default();

    // A container that says private-clone, but the account is not configured
    // for it, or its mode cannot be ruled out: never touched.
    let host_mode = |_: &AccountDescriptor| -> anyhow::Result<bool> { Ok(false) };
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &host_mode, &never, &mut state, 0),
        vec![Outcome::Running]
    );
    let unknown = |_: &AccountDescriptor| -> anyhow::Result<bool> { bail!("bad state") };
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &unknown, &never, &mut state, 60),
        vec![Outcome::Running]
    );

    // The host daemon's version could not be read: no image decision.
    let private = private();
    *lifecycle.runner().host_version.lock().unwrap() = None;
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &private, &never, &mut state, 120),
        vec![Outcome::Running]
    );

    // On the release image — as Docker records it, with the digest of the
    // pull (#13479): nothing to do.
    *lifecycle.runner().host_version.lock().unwrap() = Some(v("0.20.41"));
    set_image(&lifecycle, format!("{}@sha256:{}", release("0.20.41"), "a".repeat(64)));
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &private, &never, &mut state, 180),
        vec![Outcome::Running]
    );
}

#[test]
#[serial]
fn a_seat_on_a_newer_release_than_the_daemon_is_not_downgraded() {
    let never = |_: &AccountDescriptor, image: &str, _: &dyn Fn() -> bool| -> Result<Rolled> {
        bail!("the roll must not be asked for {image}")
    };
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let newer = "ghcr.io/2amlogic/loom-worker-session:0.20.42";
    seed_stale(&lifecycle, "0.20.41", newer);
    let mut state = ReconcileState::default();
    for now in [0, 60] {
        assert_eq!(
            roll_pass(&mut lifecycle, &env, &private(), &never, &mut state, now),
            vec![Outcome::Running]
        );
    }
    assert_eq!(running_image(&lifecycle), newer);
}

#[test]
#[serial]
fn a_seat_the_roll_keeps_is_asked_once_per_image_pair() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let asked = Cell::new(0);
    let kept = |_: &AccountDescriptor, image: &str, _: &dyn Fn() -> bool| {
        asked.set(asked.get() + 1);
        assert_eq!(image, release("0.20.41"));
        Ok(Rolled::Kept)
    };
    let mut state = ReconcileState::default();
    for now in [0, 60, 120] {
        assert_eq!(
            roll_pass(&mut lifecycle, &env, &private(), &kept, &mut state, now),
            vec![Outcome::Running]
        );
    }
    assert_eq!(asked.get(), 1, "not re-asked every pass");
    set_image(&lifecycle, LEGACY_LATEST);
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &private(), &kept, &mut state, 180),
        vec![Outcome::Running]
    );
    assert_eq!(asked.get(), 2, "another image is another pair");
}

#[test]
#[serial]
fn a_seat_that_stopped_before_its_roll_is_left_to_the_recovery() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let asked = Cell::new(0);
    let gone = |_: &AccountDescriptor, _: &str, _: &dyn Fn() -> bool| {
        asked.set(asked.get() + 1);
        Ok(Rolled::Unchanged)
    };
    let mut state = ReconcileState::default();
    for now in [0, 60] {
        assert_eq!(
            roll_pass(&mut lifecycle, &env, &private(), &gone, &mut state, now),
            vec![Outcome::Running]
        );
    }
    // Not remembered as attempted: the next pass asks again, and a stopped
    // seat is recovered in place from then on.
    assert_eq!(asked.get(), 2);
    lifecycle
        .runner()
        .containers
        .lock()
        .unwrap()
        .get_mut(&container_name("alice"))
        .unwrap()
        .running = false;
    let recovered = Cell::new(0);
    let recover = |_: &AccountDescriptor, image: Option<&str>, _: &dyn Fn() -> bool| {
        recovered.set(recovered.get() + 1);
        assert_eq!(image, Some(LATEST));
        Ok(())
    };
    assert_eq!(
        pass_with_roll(&mut lifecycle, &env, &private(), &recover, &gone, &mut state, 120),
        vec![Outcome::PrivateRecovered]
    );
    assert_eq!(asked.get(), 2, "a stopped seat is not the roll's to touch");
}

#[test]
#[serial]
fn a_hold_at_the_roll_boundary_is_reported_held() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let held = |_: &AccountDescriptor, _: &str, _: &dyn Fn() -> bool| Err(OperatorHeld.into());
    let mut state = ReconcileState::default();
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &private(), &held, &mut state, 0),
        vec![Outcome::Held]
    );
}

#[test]
#[serial]
fn a_timed_out_roll_ends_the_pass_as_docker_unavailable() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", LATEST);
    let slow = |_: &AccountDescriptor, _: &str, _: &dyn Fn() -> bool| {
        Err(DockerTimedOut {
            subcommand: "pull".into(),
            secs: 600,
        }
        .into())
    };
    let mut state = ReconcileState::default();
    let out = roll_pass(&mut lifecycle, &env, &private(), &slow, &mut state, 0);
    assert!(matches!(&out[..], [Outcome::DockerUnavailable { .. }]), "{out:?}");
    assert!(matches!(
        roll_pass(&mut lifecycle, &env, &private(), &slow, &mut state, 60)[..],
        [Outcome::BackingOff { .. }]
    ));
    let asked = Cell::new(0);
    let roll = rolling(lifecycle.runner().clone(), &asked);
    let out = roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 120);
    assert!(matches!(out.as_slice(), [Outcome::PrivateImageRecreated { .. }]), "{out:?}");
    assert_eq!(asked.get(), 1);
}

/// A roll that settles the interrupted roll (restores the original under the
/// account lease) and counts how often the pass asked.
fn settling<'a>(
    fake: Fake,
    asked: &'a Cell<usize>,
) -> impl Fn(&AccountDescriptor, &str, &dyn Fn() -> bool) -> Result<Rolled> + 'a {
    move |_: &AccountDescriptor, _: &str, _: &dyn Fn() -> bool| {
        asked.set(asked.get() + 1);
        let mut containers = fake.containers.lock().unwrap();
        let seat = containers.get_mut(&container_name("alice")).unwrap();
        seat.restarting = false;
        seat.running = true;
        fake.replaces.lock().unwrap().clear();
        Ok(Rolled::Unchanged)
    }
}

#[test]
#[serial]
fn a_restarting_roll_replacement_is_settled_before_the_pass_reads_a_crash_loop() {
    for passes in [1, 6] {
        let env = setup(&["alice"], &["alice"]);
        let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
        seed_stale(&lifecycle, "0.20.41", &release("0.20.41"));
        {
            let fake = lifecycle.runner();
            fake.containers
                .lock()
                .unwrap()
                .get_mut(&container_name("alice"))
                .unwrap()
                .restarting = true;
            fake.replaces
                .lock()
                .unwrap()
                .insert(container_name("alice"), "a".repeat(64));
        }
        let asked = Cell::new(0);
        let roll = settling(lifecycle.runner().clone(), &asked);
        let mut state = ReconcileState::default();
        for now in 0..passes {
            roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, now * 60);
        }
        // The recovery ran on the first pass, before the restart streak could
        // reach a crash-loop verdict, and left one running owner.
        assert_eq!(asked.get(), 1, "settled once, then the label is gone");
        let seat = lifecycle.runner().containers.lock().unwrap()[&container_name("alice")].clone();
        assert!(seat.running && !seat.restarting);
    }
}

#[test]
#[serial]
fn a_replacement_on_a_newer_image_still_settles_the_interrupted_roll() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    let newer = "ghcr.io/2amlogic/loom-worker-session:0.20.44";
    seed_stale(&lifecycle, "0.20.43", newer);
    lifecycle
        .runner()
        .replaces
        .lock()
        .unwrap()
        .insert(container_name("alice"), "b".repeat(64));
    let asked = Cell::new(0);
    let roll = settling(lifecycle.runner().clone(), &asked);
    let mut state = ReconcileState::default();
    assert_eq!(
        roll_pass(&mut lifecycle, &env, &private(), &roll, &mut state, 0),
        vec![Outcome::Running]
    );
    assert_eq!(asked.get(), 1, "settled before the no-downgrade decision");
    assert_eq!(running_image(&lifecycle), newer, "and the newer seat is not downgraded");
}

#[test]
#[serial]
fn a_failed_recovery_is_a_failed_pass_not_a_running_one() {
    let env = setup(&["alice"], &["alice"]);
    let mut lifecycle = SessionLifecycle::new(env.workspace.path(), Fake::default(), None);
    seed_stale(&lifecycle, "0.20.41", &release("0.20.41"));
    lifecycle
        .runner()
        .replaces
        .lock()
        .unwrap()
        .insert(container_name("alice"), "c".repeat(64));
    let refused = |_: &AccountDescriptor, _: &str, _: &dyn Fn() -> bool| -> Result<Rolled> {
        bail!("docker refused rollback removal")
    };
    let mut state = ReconcileState::default();
    let out = roll_pass(&mut lifecycle, &env, &private(), &refused, &mut state, 0);
    assert!(
        matches!(&out[..], [Outcome::Failed { error, .. }] if error.contains("refused rollback")),
        "{out:?}"
    );
    assert!(
        matches!(
            roll_pass(&mut lifecycle, &env, &private(), &refused, &mut state, 1)[..],
            [Outcome::BackingOff { .. }]
        ),
        "the failure backs off instead of clearing the account's failure state"
    );
}
