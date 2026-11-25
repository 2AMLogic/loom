//! Tests for the ratification lane's attempt ledger and backoff policy
//! (#15315). Lane-level tests of the bound live beside the other lane tests
//! in `ratification_markers.rs`.

use crate::role_runner::ratification_attempts::{
    backoff_for, counts_toward_cap, is_host_failure_class, key_attempts, ledger_path, load, save,
    AttemptRecord, Backoff, CachedInstructions, KeySlot, LaneMemory, Ledger, ATTEMPT_WINDOW_HOURS,
    LEDGER_RULES, MAX_COUNTED_ATTEMPTS_PER_WINDOW,
};
use crate::role_runner::ratification_dispatch::{AttemptFailure, InstructionSource};
use crate::role_runner::ratification_lane::{Key, Pin, WithheldClass};
use chrono::{DateTime, TimeDelta, Utc};
use std::collections::HashSet;

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

fn t0() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-11-22T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}

fn rec(pr: u64, head: &str, key: Key, minutes: i64) -> AttemptRecord {
    AttemptRecord {
        repo: "o/r".into(),
        pr,
        head_sha: head.into(),
        key: key.wire().into(),
        class: "verdict_invalid".into(),
        at: t0() + TimeDelta::minutes(minutes),
    }
}

fn ledger(records: Vec<AttemptRecord>) -> Ledger {
    Ledger {
        rules: LEDGER_RULES,
        attempts: records,
    }
}

#[test]
fn ledger_round_trips_and_a_missing_file_is_empty() {
    let dir = tempfile::tempdir().unwrap();
    let path = ledger_path(dir.path());
    assert!(path.ends_with(".loom/state/ratification-lane/attempts.json"));
    assert_eq!(load(&path).unwrap(), Ledger::default());
    let l = ledger(vec![rec(7, HEAD, Key::Ee, 0), rec(7, HEAD, Key::Market, 1)]);
    save(&path, &l).unwrap();
    assert_eq!(load(&path).unwrap(), l);
    // Only the fixed class and identity are recorded, never reason text.
    let text = std::fs::read_to_string(&path).unwrap();
    for field in [
        "\"repo\"",
        "\"pr\"",
        "\"head_sha\"",
        "\"key\"",
        "\"class\"",
        "\"at\"",
    ] {
        assert!(text.contains(field), "{field}: {text}");
    }
}

#[test]
fn a_ledger_under_other_rules_counts_nothing_and_a_corrupt_one_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let path = ledger_path(dir.path());
    let mut l = ledger(vec![rec(7, HEAD, Key::Ee, 0)]);
    l.rules = LEDGER_RULES + 1;
    save(&path, &l).unwrap();
    assert_eq!(load(&path).unwrap(), Ledger::default());
    std::fs::write(&path, "{not json").unwrap();
    assert!(load(&path).unwrap_err().contains("did not parse"));
}

#[test]
fn count_ignores_other_head_key_pr_repo_and_aged_records() {
    let other_head = "f".repeat(40);
    let mut foreign = rec(7, HEAD, Key::Ee, 0);
    foreign.repo = "o/other".into();
    let l = ledger(vec![
        rec(7, HEAD, Key::Ee, 0),
        rec(7, &HEAD.to_ascii_uppercase(), Key::Ee, 1),
        rec(7, &other_head, Key::Ee, 2),
        rec(7, HEAD, Key::Market, 3),
        rec(8, HEAD, Key::Ee, 4),
        foreign,
        rec(7, HEAD, Key::Ee, -ATTEMPT_WINDOW_HOURS * 60),
    ]);
    let counted = l.counted("o/r", 7, HEAD, Key::Ee, t0() + TimeDelta::minutes(5));
    assert_eq!(counted.len(), 2, "{counted:?}");
    assert!(l
        .capped_until("o/r", 7, HEAD, Key::Ee, t0() + TimeDelta::minutes(5))
        .is_none());
}

#[test]
fn a_cap_is_released_when_enough_attempts_age_out() {
    // Three counted attempts: capped until the oldest is a full window old.
    let l = ledger(vec![
        rec(7, HEAD, Key::Ee, 30),
        rec(7, HEAD, Key::Ee, 0),
        rec(7, HEAD, Key::Ee, 60),
    ]);
    let now = t0() + TimeDelta::minutes(61);
    let until = t0() + TimeDelta::hours(ATTEMPT_WINDOW_HOURS);
    assert_eq!(l.capped_until("o/r", 7, HEAD, Key::Ee, now), Some(until));
    assert_eq!(l.capped_until("o/r", 7, HEAD, Key::Ee, until), None);
    // A fourth attempt after the release caps again until the next oldest
    // of the last three ages out: at most three per trailing window.
    let mut l = l;
    l.record(rec(7, HEAD, Key::Ee, ATTEMPT_WINDOW_HOURS * 60 + 1));
    let later = until + TimeDelta::minutes(2);
    assert_eq!(
        l.capped_until("o/r", 7, HEAD, Key::Ee, later),
        Some(t0() + TimeDelta::minutes(30) + TimeDelta::hours(ATTEMPT_WINDOW_HOURS))
    );
    assert_eq!(MAX_COUNTED_ATTEMPTS_PER_WINDOW, 3);
    // The status view reads the same gate.
    let k = key_attempts(&l, "o/r", 7, HEAD, Key::Ee, later);
    assert_eq!(k.counted.len(), 3);
    assert_eq!(k.capped_until, l.capped_until("o/r", 7, HEAD, Key::Ee, later));
}

#[test]
fn prune_drops_aged_records_and_records_for_closed_prs_or_moved_heads() {
    let other_head = "f".repeat(40);
    let mut l = ledger(vec![
        rec(7, HEAD, Key::Ee, 0),
        rec(7, &other_head, Key::Ee, 0),
        rec(8, HEAD, Key::Ee, 0),
        rec(7, HEAD, Key::Market, -ATTEMPT_WINDOW_HOURS * 60 - 1),
    ]);
    let now = t0() + TimeDelta::minutes(1);
    assert!(l.prune(now, None), "the aged record goes");
    assert_eq!(l.attempts.len(), 3);
    let current: HashSet<(u64, String)> = [(7, HEAD.to_string())].into_iter().collect();
    assert!(l.prune(now, Some(&current)));
    assert_eq!(l.attempts, vec![rec(7, HEAD, Key::Ee, 0)]);
    assert!(!l.prune(now, Some(&current)), "nothing more to drop");
    l.merge(&[rec(7, HEAD, Key::Ee, 0), rec(7, HEAD, Key::Ee, 1)]);
    assert_eq!(l.attempts.len(), 2, "merge adds only new records");
}

#[test]
fn counts_toward_cap_table() {
    use WithheldClass::*;
    let launched = |c: &str| Some(AttemptFailure::Launched(Some(c.to_string())));
    let cases: Vec<(WithheldClass, Option<AttemptFailure>, bool, Backoff)> = vec![
        (OutputMissing, None, true, Backoff::None),
        (VerdictInvalid, None, true, Backoff::None),
        (VerdictPlaceholder, None, true, Backoff::None),
        (PreLaunchFailed, Some(AttemptFailure::Pin), false, Backoff::Pin),
        (PreLaunchFailed, Some(AttemptFailure::PreLaunch), false, Backoff::Key),
        (InvocationFailed, Some(AttemptFailure::Rolled), false, Backoff::None),
        (InvocationFailed, launched("account-pool-exhausted"), false, Backoff::Key),
        (
            InvocationFailed,
            launched("account-exhausted:token-exhausted"),
            false,
            Backoff::Key,
        ),
        (InvocationFailed, launched("session-limit"), false, Backoff::Key),
        (InvocationFailed, launched("credential-expired"), false, Backoff::Key),
        (InvocationFailed, launched("runtime-timeout"), false, Backoff::Key),
        (InvocationFailed, launched("sandbox-unavailable"), false, Backoff::Key),
        (InvocationFailed, launched("cwd-deleted"), false, Backoff::Key),
        (InvocationFailed, launched("preflight-no-cli-start"), false, Backoff::Key),
        (InvocationFailed, launched("exit-1"), false, Backoff::Pr),
        (InvocationFailed, launched("exit-78"), false, Backoff::Pr),
        (InvocationFailed, launched("timeout-ceiling"), false, Backoff::Pr),
        (InvocationFailed, Some(AttemptFailure::Launched(None)), false, Backoff::Pr),
        (PostFailed, None, false, Backoff::Pr),
    ];
    for (class, failure, counted, backoff) in cases {
        assert_eq!(counts_toward_cap(class), counted, "{class:?}");
        assert_eq!(backoff_for(class, failure.as_ref()), backoff, "{class:?} {failure:?}");
    }
    for class in [
        "exit-1",
        "timeout-ceiling",
        "killed-by-signal",
        "model-refusal",
        "",
    ] {
        assert!(!is_host_failure_class(class), "{class}");
    }
}

fn pin(path: &str) -> Pin {
    Pin {
        repo: "k/i".into(),
        path: path.into(),
        commit: "a".repeat(40),
    }
}

#[test]
fn backoffs_are_scoped_to_the_pin_the_repo_key_or_the_pr() {
    let mut m = LaneMemory::default();
    let (ee_pin, mk_pin) = (pin("ee.md"), pin("market.md"));
    let slot = |repo: &'static str, pr: u64, key: Key, pin: &'static Pin| KeySlot {
        repo,
        pr,
        head_sha: HEAD,
        key,
        pin,
    };
    let ee_pin: &'static Pin = Box::leak(Box::new(ee_pin));
    let mk_pin: &'static Pin = Box::leak(Box::new(mk_pin));
    let now = t0();
    let soon = now + TimeDelta::minutes(1);
    let after = now + TimeDelta::minutes(16);

    m.back_off(Backoff::Pin, &slot("o/r", 7, Key::Ee, ee_pin), now);
    assert!(m.deferred(&slot("o/other", 9, Key::Ee, ee_pin), soon), "a pin is process-wide");
    assert!(!m.deferred(&slot("o/r", 7, Key::Market, mk_pin), soon));
    assert!(!m.deferred(&slot("o/r", 7, Key::Ee, ee_pin), after), "it expires");

    let mut m = LaneMemory::default();
    m.back_off(Backoff::Key, &slot("o/r", 7, Key::Ee, ee_pin), now);
    assert!(m.deferred(&slot("o/r", 8, Key::Ee, ee_pin), soon), "a key is repo-wide");
    assert!(!m.deferred(&slot("o/other", 8, Key::Ee, ee_pin), soon), "not across repos");
    assert!(!m.deferred(&slot("o/r", 8, Key::Market, mk_pin), soon));

    let mut m = LaneMemory::default();
    m.back_off(Backoff::Pr, &slot("o/r", 7, Key::Ee, ee_pin), now);
    assert!(m.deferred(&slot("o/r", 7, Key::Ee, ee_pin), soon));
    assert!(!m.deferred(&slot("o/r", 8, Key::Ee, ee_pin), soon), "only that PR");
    m.back_off(Backoff::None, &slot("o/r", 8, Key::Ee, ee_pin), now);
    assert!(!m.deferred(&slot("o/r", 8, Key::Ee, ee_pin), soon));

    // Prune: expired entries always go; per-PR entries for PRs no longer
    // current go only when the caller passes the current set.
    m.prune("o/r", soon, None);
    assert!(m.deferred(&slot("o/r", 7, Key::Ee, ee_pin), soon));
    let current: HashSet<(u64, String)> = [(8, HEAD.to_string())].into_iter().collect();
    m.prune("o/r", soon, Some(&current));
    assert!(!m.deferred(&slot("o/r", 7, Key::Ee, ee_pin), soon));
}

#[test]
fn cached_instructions_read_each_immutable_pin_once_and_visibility_once_per_pass() {
    #[derive(Default)]
    struct Counting {
        fetches: std::cell::RefCell<Vec<String>>,
        visibility: std::cell::Cell<u32>,
        fail: bool,
    }
    impl InstructionSource for Counting {
        fn fetch(&self, pin: &Pin) -> Result<String, String> {
            self.fetches.borrow_mut().push(pin.path.clone());
            if self.fail {
                Err("reader budget low".into())
            } else {
                Ok(format!("body of {}", pin.path))
            }
        }
        fn repo_is_private(&self, _repo: &str) -> Option<bool> {
            self.visibility.set(self.visibility.get() + 1);
            Some(false)
        }
    }
    let memory = std::sync::Mutex::new(LaneMemory::default());
    let inner = Counting::default();
    for _pass in 0..2 {
        let c = CachedInstructions::new(&inner, &memory);
        assert_eq!(c.fetch(&pin("ee.md")).unwrap(), "body of ee.md");
        assert_eq!(c.fetch(&pin("ee.md")).unwrap(), "body of ee.md");
        assert_eq!(c.repo_is_private("o/r"), Some(false));
        assert_eq!(c.repo_is_private("o/r"), Some(false));
    }
    assert_eq!(*inner.fetches.borrow(), ["ee.md"]);
    assert_eq!(inner.visibility.get(), 2, "visibility is read once per pass");

    // A failed read is never cached; a non-SHA pin is never cached either.
    let failing = Counting {
        fail: true,
        ..Counting::default()
    };
    let c = CachedInstructions::new(&failing, &memory);
    let fresh = pin("market.md");
    assert!(c.fetch(&fresh).is_err());
    assert!(c.fetch(&fresh).is_err());
    assert_eq!(failing.fetches.borrow().len(), 2);
    let branch = Pin {
        commit: "main".into(),
        ..pin("other.md")
    };
    let c = CachedInstructions::new(&inner, &memory);
    c.fetch(&branch).unwrap();
    c.fetch(&branch).unwrap();
    assert_eq!(*inner.fetches.borrow(), ["ee.md", "other.md", "other.md"]);
}
