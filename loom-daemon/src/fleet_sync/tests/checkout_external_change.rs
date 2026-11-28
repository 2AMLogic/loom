//! Tests for out-of-band checkout changes (#11566), and for the record the
//! pass emits for its own fast-forward (#15729). Real git in temp dirs; the
//! 2am reflog is a faithful synthetic fixture (see `FIXTURE`).

use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use chrono::{TimeZone, Utc};
use tempfile::TempDir;

use super::super::{watch_reflog, CheckoutPass, CheckoutState, Env, Memory, MoveHold};
use super::*;
use crate::observability::ops::capture::{capture, without_global_sink, Captured};
use crate::telemetry::kinds::checkout_fast_forward::CheckoutFastForwardRecord;
use crate::telemetry::provenance::Provenance;
use crate::telemetry::TelemetryRecord;
use crate::test_git_helpers::run as git;
use crate::workspace_hold::{HeldCopy, HoldKind, WorkspaceHold};

/// A synthetic stand-in for the reflog of the checkout reset at 00:44:39,
/// 00:44:57 and 00:45:16 on 2026-10-12. The real reflog was not available to
/// the builder, so the lines are faithful in shape (git's on-disk format, the
/// three reset times, unpushed commits before them, Loom's own tagged
/// fast-forwards around them, and a no-op `reset: moving to HEAD`) but the
/// shas and messages are invented.
const FIXTURE: &str = include_str!("../../../tests/fixtures/reflog-2am-2026-10-12.txt");

fn commit(dir: &Path, name: &str) {
    fs::write(dir.join(name), name).unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "--quiet", "-m", &format!("add {name}")]);
}

/// A repo on `main` with one commit.
fn repo() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let dir = fs::canonicalize(tmp.path()).unwrap().join("host");
    fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "--quiet", "--initial-branch=main"]);
    for (k, v) in [("user.name", "T"), ("user.email", "t@example.invalid")] {
        git(&dir, &["config", k, v]);
    }
    git(&dir, &["config", "commit.gpgsign", "false"]);
    commit(&dir, "first");
    (tmp, dir)
}

fn watch(root: &Path, memory: &mut Memory) -> CheckoutPass {
    let mut pass = CheckoutPass::default();
    watch_reflog(root, Some("main"), Utc::now(), memory, &mut pass);
    pass
}

// ---------------------------------------------------------------- parsing

#[test]
fn the_2am_fixture_yields_the_three_resets() {
    let entries: Vec<Entry> = FIXTURE.lines().filter_map(parse_line).collect();
    assert_eq!(entries.len(), FIXTURE.lines().count(), "every line parses");
    let kinds: Vec<_> = entries
        .iter()
        .filter_map(|e| classify(e).map(|k| (e.at, k)))
        .collect();
    let at = |h, m, s| Utc.with_ymd_and_hms(2026, 10, 12, h, m, s).unwrap();
    assert_eq!(
        kinds,
        vec![
            (at(0, 44, 39), Kind::Reset),
            (at(0, 44, 57), Kind::Reset),
            (at(0, 45, 16), Kind::Reset),
        ],
        "Loom's tagged ff, plain commits and the no-op reset are not external changes"
    );
}

#[test]
fn replaying_the_fixture_past_a_seeded_mark_finds_each_reset_once() {
    let head = String::from("HEAD");
    let lines: Vec<&str> = FIXTURE.lines().collect();
    let seeded = lines[..4].join("\n");
    let mut marks = Marks::default();
    assert!(marks.scan(&[(head.clone(), seeded)]).found.is_empty(), "first sight seeds");
    let scan = marks.scan(&[(head.clone(), FIXTURE.to_string())]);
    let times: Vec<_> = scan
        .found
        .iter()
        .map(|(_, e, _)| e.at.format("%H:%M:%S").to_string())
        .collect();
    assert_eq!(times, ["00:44:39", "00:44:57", "00:45:16"]);
    assert!(marks.scan(&[(head, FIXTURE.to_string())]).found.is_empty(), "second look: none");
}

#[test]
fn a_reset_in_both_logs_is_one_entry() {
    let line = "0000000000000000000000000000000000000002 0000000000000000000000000000000000000001 \
                T <t@x> 1791765879 +0000\treset: moving to HEAD~1\n";
    let old = "0000000000000000000000000000000000000001 0000000000000000000000000000000000000002 \
               T <t@x> 1791765800 +0000\tcommit: x\n";
    let mut marks = Marks::default();
    let logs = |text: String| {
        vec![
            ("HEAD".to_string(), text.clone()),
            ("refs/heads/main".into(), text),
        ]
    };
    marks.scan(&logs(old.to_string()));
    assert_eq!(marks.scan(&logs(format!("{old}{line}"))).found.len(), 1);
}

#[test]
fn a_shrunk_or_rewritten_log_is_unknown_not_a_change() {
    let l = |n: u8, msg: &str| {
        format!("{:040x} {:040x} T <t@x> 17917658{n:02} +0000\t{msg}\n", n, n + 1)
    };
    let mut marks = Marks::default();
    let log = format!("{}{}", l(1, "commit: a"), l(2, "commit: b"));
    marks.scan(&[("HEAD".into(), log)]);
    let rewritten = format!("{}{}", l(1, "commit: a"), l(9, "reset: moving to x"));
    assert!(marks.scan(&[("HEAD".into(), rewritten)]).found.is_empty());
    assert!(marks
        .scan(&[("HEAD".into(), l(3, "reset: moving to y"))])
        .found
        .is_empty());
}

// ---------------------------------------------------------------- real git

#[test]
fn first_sight_seeds_silently() {
    let (_t, dir) = repo();
    commit(&dir, "second");
    let mut memory = Memory::default();
    assert!(watch(&dir, &mut memory).external_changes.is_empty());
}

#[test]
fn an_external_reset_emits_exactly_one_fact_with_the_discarded_commits() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    commit(&dir, "unpushed-a");
    commit(&dir, "unpushed-b");
    let tip = git(&dir, &["rev-parse", "HEAD"]);
    let first = git(&dir, &["rev-parse", "HEAD~2"]);
    assert!(watch(&dir, &mut memory).external_changes.is_empty(), "commits are additive");

    git(&dir, &["reset", "--quiet", "--hard", "HEAD~2"]);
    let pass = watch(&dir, &mut memory);
    assert_eq!(pass.external_changes.len(), 1, "{:?}", pass.external_changes);
    let fact = &pass.external_changes[0];
    assert_eq!(
        (fact.move_kind.as_str(), fact.from_sha.as_str(), fact.to_sha.as_str()),
        ("reset", tip.as_str(), first.as_str())
    );
    assert!(fact.reflog_line.starts_with("reset:"), "{}", fact.reflog_line);
    assert_eq!(fact.root, dir.display().to_string());
    assert!(!fact.host.is_empty());
    assert!(!fact.hold_active);
    let discarded = fact.discarded_commits.as_ref().unwrap();
    assert_eq!(discarded.count, 2);
    assert_eq!(discarded.subjects, ["add unpushed-b", "add unpushed-a"]);

    assert!(watch(&dir, &mut memory).external_changes.is_empty(), "seen once, never again");
}

#[test]
fn a_commit_still_on_another_branch_is_not_discarded() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    commit(&dir, "kept");
    git(&dir, &["branch", "rescue"]);
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let pass = watch(&dir, &mut memory);
    assert_eq!(
        pass.external_changes[0]
            .discarded_commits
            .as_ref()
            .unwrap()
            .count,
        0
    );
}

#[test]
fn an_external_checkout_is_reported() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    git(&dir, &["checkout", "--quiet", "-b", "elsewhere"]);
    commit(&dir, "x");
    git(&dir, &["checkout", "--quiet", "main"]);
    let pass = watch(&dir, &mut memory);
    let kinds: Vec<_> = pass
        .external_changes
        .iter()
        .map(|f| f.move_kind.as_str())
        .collect();
    // Creating `elsewhere` at the same commit moved nothing; leaving it did.
    assert_eq!(kinds, ["checkout"], "{:?}", pass.external_changes);
}

/// A bare origin and a clone of it, plus a second clone that pushes.
fn with_origin() -> (TempDir, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let base = fs::canonicalize(tmp.path()).unwrap();
    let origin = base.join("origin.git");
    fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "--quiet", "--bare", "--initial-branch=main"]);
    let clone = |name: &str| {
        git(&base, &["clone", "--quiet", origin.to_str().unwrap(), name]);
        let dir = base.join(name);
        for (k, v) in [("user.name", "T"), ("user.email", "t@example.invalid")] {
            git(&dir, &["config", k, v]);
        }
        git(&dir, &["config", "commit.gpgsign", "false"]);
        dir
    };
    let seed = clone("seed");
    commit(&seed, "first");
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    let host = clone("host");
    commit(&seed, "second");
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    (tmp, host, seed)
}

#[test]
fn loom_own_fast_forward_emits_nothing_and_logs_the_loom_tag() {
    let (_t, host, _seed) = with_origin();
    let mut memory = Memory::default();
    watch(&host, &mut memory);
    git(&host, &["fetch", "--quiet", "origin"]);
    super::super::git::fast_forward(&host, "main").unwrap();
    let log = fs::read_to_string(host.join(".git/logs/HEAD")).unwrap();
    assert!(log.lines().last().unwrap().contains("\tloom:ff:"), "{log}");
    assert!(watch(&host, &mut memory).external_changes.is_empty());
    // The same checkout, reset by hand afterwards, is reported.
    git(&host, &["reset", "--quiet", "--hard", "HEAD~1"]);
    assert_eq!(watch(&host, &mut memory).external_changes.len(), 1);
}

// ------------------------------------------------ the pass's own fast-forward

/// A host clone one commit behind its origin, which it has already fetched.
fn behind() -> (TempDir, PathBuf, PathBuf) {
    let (tmp, host, seed) = with_origin();
    git(&host, &["fetch", "--quiet", "origin"]);
    (tmp, host, seed)
}

/// One pass over `root` on a host with `fleet.autoApply` on and no network,
/// so the checkout is compared with the clone's own `origin/main`.
fn apply_pass(
    root: &PathBuf,
    memory: &mut Memory,
    held: &dyn Fn() -> Option<&'static str>,
) -> CheckoutPass {
    let env = Env {
        write: true,
        held,
        gate_in_flight: &|_| false,
        hold: &|_| Some(MoveHold::free()),
        network: false,
        backing_off: &|_| false,
        confirmed: &|_, _| None,
        breaker_open: &|| false,
        budget: None,
        elapsed: &|| Duration::ZERO,
        clock: &Utc::now,
        pr_head: &|_, _| None,
    };
    super::super::run(&env, std::slice::from_ref(root), memory)
}

fn fast_forwards(captured: &Captured) -> Vec<CheckoutFastForwardRecord> {
    captured
        .records
        .iter()
        .filter_map(|r| match r {
            TelemetryRecord::CheckoutFastForward(f) => Some(f.clone()),
            _ => None,
        })
        .collect()
}

/// The last line of `root`'s HEAD reflog, split by hand rather than with the
/// parser under test: the commit before, the commit after, when git wrote it
/// (epoch seconds), and its message.
fn last_reflog_line(root: &Path) -> (String, String, i64, String) {
    let log = fs::read_to_string(root.join(".git/logs/HEAD")).unwrap();
    let line = log.lines().last().unwrap();
    let (head, message) = line.split_once('\t').unwrap();
    let words: Vec<&str> = head.split_whitespace().collect();
    (
        words[0].to_string(),
        words[1].to_string(),
        words[words.len() - 2].parse().unwrap(),
        message.trim().to_string(),
    )
}

fn own_tag() -> String {
    format!("{}:", tag(FF_OP))
}

#[test]
fn a_fast_forward_on_the_first_pass_emits_one_record_that_matches_the_reflog() {
    let (_t, host, _seed) = behind();
    let old = git(&host, &["rev-parse", "HEAD"]);
    let tip = git(&host, &["rev-parse", "refs/remotes/origin/main"]);
    // A GitHub-shaped origin with a credential the record must not carry. The
    // pass has no network, so it is never asked.
    git(
        &host,
        &[
            "remote",
            "set-url",
            "origin",
            "https://user:ghp_FAKE@github.com/o/r.git",
        ],
    );

    let mut memory = Memory::default();
    let began = Utc::now();
    let (pass, captured) = capture(|| apply_pass(&host, &mut memory, &|| None));
    let ended = Utc::now();
    assert_eq!(pass.checkouts[0].state, CheckoutState::FastForwarded, "{pass:?}");
    assert_eq!(git(&host, &["rev-parse", "HEAD"]), tip, "the checkout moved");
    let records = fast_forwards(&captured);
    assert_eq!(records.len(), 1, "{records:?}");
    let r = &records[0];
    let (from, to, epoch, message) = last_reflog_line(&host);
    assert_eq!((from.as_str(), to.as_str()), (old.as_str(), tip.as_str()));
    assert_eq!(
        (r.from_sha.as_str(), r.to_sha.as_str()),
        (from.as_str(), to.as_str()),
        "the full shas are the reflog entry's"
    );
    assert_eq!(r.at.timestamp(), epoch, "the record time is the reflog entry's");
    assert!(began <= r.observed_at && r.observed_at <= ended, "{}", r.observed_at);
    assert!(message.starts_with(&own_tag()), "{message}");
    assert_eq!(r.root, host.display().to_string());
    assert!(!r.host.is_empty());
    assert_eq!(r.repo.as_deref(), Some("o/r"));
    assert_eq!(r.ref_name, "refs/heads/main");
    assert!(r.has_provenance());
    let json = serde_json::to_string(r).unwrap();
    for leak in ["ghp_", "user:", "https://"] {
        assert!(!json.contains(leak), "{leak} in {json}");
    }
    let back: CheckoutFastForwardRecord = serde_json::from_str(&json).unwrap();
    assert_eq!(back, *r);

    // The same tip on a later pass: nothing moved, so nothing is said.
    let (_, again) = capture(|| apply_pass(&host, &mut memory, &|| None));
    assert!(fast_forwards(&again).is_empty(), "{:?}", again.records);

    // Reset by hand afterwards: the next pass reports the reset as the
    // external change it always was, and its own repair as a fast-forward.
    git(&host, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let (pass, after) = capture(|| apply_pass(&host, &mut memory, &|| None));
    assert_eq!(pass.external_changes.len(), 1, "{:?}", pass.external_changes);
    assert_eq!(pass.external_changes[0].move_kind, "reset");
    let repair = fast_forwards(&after);
    assert_eq!(repair.len(), 1, "{repair:?}");
    assert_eq!(
        (repair[0].from_sha.as_str(), repair[0].to_sha.as_str()),
        (old.as_str(), tip.as_str())
    );
}

#[test]
fn a_fast_forward_that_needs_the_collision_retry_emits_one_record() {
    let (_t, host, _seed) = behind();
    let old = git(&host, &["rev-parse", "HEAD"]);
    // Untracked, in the way of the incoming commit and byte-identical to what
    // it writes (#13580): the first merge refuses, and the retry moves.
    fs::write(host.join("second"), "second").unwrap();
    let mut memory = Memory::default();
    let (pass, captured) = capture(|| apply_pass(&host, &mut memory, &|| None));
    let report = &pass.checkouts[0];
    assert_eq!(report.state, CheckoutState::FastForwarded, "{report:?}");
    let detail = report.detail.as_deref().unwrap();
    assert!(
        detail.contains("removed untracked files identical to origin: second"),
        "{detail}"
    );
    let records = fast_forwards(&captured);
    assert_eq!(records.len(), 1, "{records:?}");
    let (from, to, epoch, _) = last_reflog_line(&host);
    assert_eq!(from, old, "one merge moved it: the refused one wrote no entry");
    assert_eq!(
        (
            records[0].from_sha.as_str(),
            records[0].to_sha.as_str(),
            records[0].at.timestamp()
        ),
        (from.as_str(), to.as_str(), epoch)
    );
}

#[test]
fn a_checkout_the_pass_did_not_move_emits_no_fast_forward_record() {
    // Already at the tip, because anything else pulled it there.
    let (_t, host, _seed) = behind();
    let tip = git(&host, &["rev-parse", "refs/remotes/origin/main"]);
    git(&host, &["pull", "--quiet", "--ff-only", "origin", "main"]);
    let (_, pulled) = capture(|| apply_pass(&host, &mut Memory::default(), &|| None));
    assert!(fast_forwards(&pulled).is_empty(), "{:?}", pulled.records);
    assert_eq!(git(&host, &["rev-parse", "HEAD"]), tip);

    // Behind and clean, but held.
    let (_t, host, _seed) = behind();
    let old = git(&host, &["rev-parse", "HEAD"]);
    let (held_pass, held) =
        capture(|| apply_pass(&host, &mut Memory::default(), &|| Some("roll pending")));
    assert_eq!(held_pass.checkouts[0].state, CheckoutState::Behind, "{held_pass:?}");
    assert!(fast_forwards(&held).is_empty(), "{:?}", held.records);
    assert_eq!(git(&host, &["rev-parse", "HEAD"]), old, "nothing written");

    // A merge refused by a local file of its own.
    let (_t, host, _seed) = behind();
    fs::write(host.join("second"), "second, but mine").unwrap();
    let (refused, refused_records) =
        capture(|| apply_pass(&host, &mut Memory::default(), &|| None));
    assert_eq!(refused.checkouts[0].state, CheckoutState::WouldOverwrite, "{refused:?}");
    assert!(fast_forwards(&refused_records).is_empty(), "{:?}", refused_records.records);
    assert_eq!(fs::read_to_string(host.join("second")).unwrap(), "second, but mine");
}

#[test]
fn a_merge_git_had_nothing_left_to_do_emits_nothing() {
    let (_t, host, _seed) = behind();
    let tip = git(&host, &["rev-parse", "refs/remotes/origin/main"]);
    // Someone else pulls between the pass's checks and its write. `held` is
    // asked right before the write, so that is when the pull lands.
    let asks = Cell::new(0);
    let held = || -> Option<&'static str> {
        asks.set(asks.get() + 1);
        git(&host, &["merge", "--quiet", "--ff-only", "refs/remotes/origin/main"]);
        None
    };
    let (pass, captured) = capture(|| apply_pass(&host, &mut Memory::default(), &held));
    assert_eq!(asks.get(), 1);
    // git's `Already up to date` is exit 0, so the pass still says it moved.
    assert_eq!(pass.checkouts[0].state, CheckoutState::FastForwarded, "{pass:?}");
    assert_eq!(git(&host, &["rev-parse", "HEAD"]), tip);
    assert!(fast_forwards(&captured).is_empty(), "{:?}", captured.records);
    assert!(
        !last_reflog_line(&host).3.starts_with(&own_tag()),
        "the pull's entry is not Loom's"
    );
}

// ------------------------------------------- which entry is the pass's own

#[test]
fn a_move_before_any_exporter_exists_is_held_until_one_does() {
    let (_t, host, _seed) = behind();
    let old = git(&host, &["rev-parse", "HEAD"]);
    let tip = git(&host, &["rev-parse", "refs/remotes/origin/main"]);
    let mut memory = Memory::default();
    // As at startup, where the pass runs before the OTLP exporter does: the
    // move is made and goes nowhere yet.
    let pass = without_global_sink(|| apply_pass(&host, &mut memory, &|| None));
    assert_eq!(pass.checkouts[0].state, CheckoutState::FastForwarded, "{pass:?}");
    assert_eq!(git(&host, &["rev-parse", "HEAD"]), tip);
    let (from, to, epoch, _) = last_reflog_line(&host);
    assert_eq!((from.as_str(), to.as_str()), (old.as_str(), tip.as_str()));

    // The first pass that exports emits it, though the checkout stands still.
    let (_, captured) = capture(|| apply_pass(&host, &mut memory, &|| None));
    let records = fast_forwards(&captured);
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(
        (
            records[0].from_sha.as_str(),
            records[0].to_sha.as_str(),
            records[0].at.timestamp()
        ),
        (from.as_str(), to.as_str(), epoch)
    );

    // And then it is said no more.
    let (_, again) = capture(|| apply_pass(&host, &mut memory, &|| None));
    assert!(fast_forwards(&again).is_empty(), "{:?}", again.records);
}

#[test]
fn the_hold_keeps_the_newest_records_and_empties_on_the_first_exporting_pass() {
    let sample = |n: usize| CheckoutFastForwardRecord {
        change_id: n.to_string(),
        repo: None,
        visibility: Default::default(),
        root: String::new(),
        host: String::new(),
        ref_name: String::new(),
        from_sha: String::new(),
        to_sha: String::new(),
        at: Utc::now(),
        observed_at: Utc::now(),
        loom: Provenance::current(),
    };
    let mut memory = Memory::default();
    for n in 0..65 {
        memory.hold_fast_forward(sample(n));
    }
    let ids: Vec<&str> = memory.held.iter().map(|r| r.change_id.as_str()).collect();
    assert_eq!((ids.len(), ids[0], ids[63]), (64, "1", "64"), "the oldest is dropped");

    let (_t, dir) = repo();
    let (_, captured) = capture(|| apply_pass(&dir, &mut memory, &|| None));
    assert!(memory.held.is_empty());
    assert_eq!(fast_forwards(&captured).len(), 64);
}

#[test]
fn an_external_pull_after_the_mark_is_not_the_pass_s_fast_forward() {
    let (_t, host, _seed) = behind();
    let before = before_merge(&host).unwrap();
    // From the very commit, to the very tip: but not Loom's.
    git(&host, &["pull", "--quiet", "--ff-only", "origin", "main"]);
    assert_eq!(fast_forwarded(&host, &before), None);
}

#[test]
fn a_stale_tagged_line_is_not_the_merge_and_the_merge_is_found_after_an_external_one() {
    let (_t, host, seed) = behind();
    // An earlier pass's tagged entry, then behind again.
    super::super::git::fast_forward(&host, "main").unwrap();
    commit(&seed, "third");
    git(&seed, &["push", "--quiet", "origin", "HEAD:main"]);
    git(&host, &["fetch", "--quiet", "origin"]);
    assert!(
        last_reflog_line(&host).3.starts_with(&own_tag()),
        "the last line of the log is a tagged entry"
    );

    // Nothing has been added since the mark: that stale line is no merge's.
    let before = before_merge(&host).unwrap();
    assert_eq!(fast_forwarded(&host, &before), None);

    // An external entry lands after the mark, and moves nothing.
    git(&host, &["reset", "--quiet", "--hard", "HEAD"]);
    assert_eq!(fast_forwarded(&host, &before), None);

    // Then the pass's own merge, after that external entry: found by where it
    // started, and by being the one tagged line the attempt added.
    let start = git(&host, &["rev-parse", "HEAD"]);
    let tip = git(&host, &["rev-parse", "refs/remotes/origin/main"]);
    super::super::git::fast_forward(&host, "main").unwrap();
    let (from, to, epoch, _) = last_reflog_line(&host);
    assert_eq!((from.as_str(), to.as_str()), (start.as_str(), tip.as_str()));
    let entry = fast_forwarded(&host, &before).unwrap();
    assert_eq!(
        (entry.from.as_str(), entry.to.as_str(), entry.at.timestamp()),
        (from.as_str(), to.as_str(), epoch)
    );
}

#[test]
fn two_tagged_moves_from_the_same_commit_are_attributed_to_neither() {
    let (_t, host, _seed) = behind();
    let before = before_merge(&host).unwrap();
    let back = || git(&host, &["reset", "--quiet", "--hard", "HEAD~1"]);
    for _ in 0..2 {
        super::super::git::fast_forward(&host, "main").unwrap();
        back();
    }
    super::super::git::fast_forward(&host, "main").unwrap();
    assert_eq!(fast_forwarded(&host, &before), None);
}

#[test]
fn a_reflog_that_cannot_be_read_is_unknown_not_a_fact() {
    let (_t, host, _seed) = behind();
    let before = before_merge(&host).unwrap();
    super::super::git::fast_forward(&host, "main").unwrap();
    // The entry was written; the log is gone by the time of the lookup.
    fs::remove_file(host.join(".git/logs/HEAD")).unwrap();
    assert_eq!(fast_forwarded(&host, &before), None);
    let nowhere = host.parent().unwrap().join("nowhere");
    assert!(before_merge(&nowhere).is_none());
}

/// `land-resync-commit.sh`, the one installed script that moves the default
/// branch of a managed root (#6646): its fallback commits the resync change and
/// then `reset --hard origin/<default>`s the primary checkout. Both run under
/// an inline `GIT_REFLOG_ACTION="loom:resync"`, which must be a Loom tag.
const LAND_RESYNC_SCRIPT: &str = include_str!("../../../../defaults/scripts/land-resync-commit.sh");

#[test]
fn land_resync_commit_tags_its_ref_moves_as_loom() {
    let tagged = format!("GIT_REFLOG_ACTION=\"{}\" git -C \"$REPO_ROOT\"", tag("resync"));
    for op in ["commit --quiet", "reset --hard"] {
        assert!(
            LAND_RESYNC_SCRIPT.contains(&format!("{tagged} {op}")),
            "land-resync-commit.sh's `git {op}` must run under the Loom reflog tag"
        );
    }
}

#[test]
fn land_resync_commit_fallback_reset_emits_nothing() {
    let (_t, host, _seed) = with_origin();
    let mut memory = Memory::default();
    watch(&host, &mut memory);
    git(&host, &["fetch", "--quiet", "origin"]);
    // The script's own sequence, with the env it exports: commit on the
    // default branch, then reset back to origin's tip (discarding the commit
    // locally; it lives on the pushed side branch).
    let resync = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&host)
            .args(args)
            .env("GIT_REFLOG_ACTION", tag("resync"))
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    fs::write(host.join("resync.txt"), "x").unwrap();
    git(&host, &["add", "-A"]);
    resync(&[
        "commit",
        "--quiet",
        "-m",
        "chore: resync installed Loom surfaces",
    ]);
    resync(&["reset", "--quiet", "--hard", "origin/main"]);
    let log = fs::read_to_string(host.join(".git/logs/refs/heads/main")).unwrap();
    let last = log.lines().last().unwrap();
    assert!(last.contains("\tloom:resync: updating HEAD"), "{log}");
    let pass = watch(&host, &mut memory);
    assert!(pass.external_changes.is_empty(), "{:?}", pass.external_changes);
}

#[test]
fn a_standing_hold_is_recorded_and_cited_when_it_clears() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    let began = Utc::now() - chrono::Duration::minutes(5);
    let hold = WorkspaceHold {
        kind: HoldKind::InstallIncompatible,
        copy: HeldCopy::Checkout,
        since: began,
        detail: "test".into(),
        verdict_at: began,
    };
    crate::workspace_hold::set_for_test(&dir, Some(hold));
    commit(&dir, "unpushed");
    watch(&dir, &mut memory);
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let pass = watch(&dir, &mut memory);
    crate::workspace_hold::set_for_test(&dir, None);
    let fact = &pass.external_changes[0];
    assert!(fact.hold_active);
    let note = cited(&dir, began);
    assert!(note.contains(&fact.change_id), "{note}");
    assert_eq!(
        cited(&dir, Utc::now() + chrono::Duration::minutes(1)),
        "",
        "later holds are not cited"
    );
}

#[test]
fn a_pass_over_the_root_reports_the_change() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    let roots = vec![dir.clone()];
    let run_pass = |memory: &mut Memory| {
        let env = Env {
            write: false,
            held: &|| None,
            gate_in_flight: &|_| false,
            hold: &|_| Some(MoveHold::free()),
            network: false,
            backing_off: &|_| false,
            confirmed: &|_, _| None,
            breaker_open: &|| false,
            budget: None,
            elapsed: &|| Duration::ZERO,
            clock: &Utc::now,
            pr_head: &|_, _| None,
        };
        super::super::run(&env, &roots, memory)
    };
    run_pass(&mut memory);
    commit(&dir, "unpushed");
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let pass = run_pass(&mut memory);
    assert_eq!(pass.external_changes.len(), 1, "{:?}", pass.external_changes);
    assert!(run_pass(&mut memory).external_changes.is_empty());
}

#[test]
fn the_record_round_trips_and_names_the_ref() {
    let (_t, dir) = repo();
    let mut memory = Memory::default();
    watch(&dir, &mut memory);
    commit(&dir, "x");
    git(&dir, &["reset", "--quiet", "--hard", "HEAD~1"]);
    let fact = watch(&dir, &mut memory).external_changes.remove(0);
    let json = serde_json::to_value(&fact).unwrap();
    assert!(json["ref"] == "HEAD" || json["ref"] == "refs/heads/main");
    let back: CheckoutExternalChangeRecord = serde_json::from_value(json).unwrap();
    assert_eq!(back, fact);
}

#[test]
fn loom_reflog_tag_is_namespace_colon_op_and_only_that_counts_as_loom() {
    use crate::telemetry::kinds::checkout_external_change::is_loom_reflog_message;
    assert_eq!(tag("resync"), "loom:resync");
    assert!(is_loom_reflog_message("loom:resync: updating HEAD"));
    assert!(is_loom_reflog_message("loom:ff: fast-forward"));
    // A namespace look-alike without the separator is not Loom's.
    assert!(!is_loom_reflog_message("loomx: reset"));
    assert!(!is_loom_reflog_message("loom reset: moving to HEAD~1"));
    assert!(!is_loom_reflog_message("reset: moving to HEAD~1"));
}

#[test]
fn repo_slug_accepts_trailing_slashes_and_repeated_git_suffixes() {
    // #15372: the slug extraction delegates to the shared
    // `parse_nwo_from_remote_url`, but the fleet checkout's own URL
    // normalization — any number of trailing `/` and `.git` repetitions —
    // must keep working on top of it.
    let (_t, dir) = repo();
    let urls = [
        "git@github.com:acme/widgets.git",
        "https://github.com/acme/widgets.git",
        "https://github.com/acme/widgets/",
        "https://github.com/acme/widgets.git/",
        "https://github.com/acme/widgets.git.git",
        "https://github.com/acme/widgets.git.git/",
    ];
    git(&dir, &["remote", "add", "origin", urls[0]]);
    for url in urls {
        git(&dir, &["remote", "set-url", "origin", url]);
        assert_eq!(repo_slug(&dir).as_deref(), Some("acme/widgets"), "url = {url}");
    }
    // No origin at all is `None`, not a guess.
    git(&dir, &["remote", "remove", "origin"]);
    assert_eq!(repo_slug(&dir), None);
}
