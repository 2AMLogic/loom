//! Direction B's old-daemon selection (#10716): the oldest PUBLISHED release
//! at or above `REQUIRES_DAEMON`, not the exact tag, because releases skip
//! versions. No network: the release listing is a closure, and the end-to-end
//! cases use a scratch git repo with no installed shell, so no forge call and
//! no subcommand probe runs.
//!
//! Then #10868: the three-valued subcommand probe against fake daemons, the
//! note that names a release still uploading, and the bounded download retry
//! (the download is a closure too).

use super::prompt_calls::{self, CommandTree};
use super::*;
use crate::test_git_helpers::run as git;
use std::time::Instant;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

fn tags(list: &[&str]) -> String {
    list.join("\n")
}

#[test]
fn tags_at_or_above_filters_and_sorts_oldest_first() {
    let got = tags_at_or_above(
        &tags(&[
            "v0.19.900",
            "v0.19.10",
            "nightly",
            "v0.19.899",
            "v0.19.9",
            "v0.20.0",
        ]),
        v("0.19.10"),
    );
    let names: Vec<&str> = got.iter().map(|(_, t)| t.as_str()).collect();
    assert_eq!(names, ["v0.19.10", "v0.19.899", "v0.19.900", "v0.20.0"]);
}

#[test]
fn exact_tag_missing_uses_the_oldest_newer_release() {
    // req 0.19.893 was skipped; 0.19.895 and 0.19.897 are published.
    let candidates = tags_at_or_above(
        &tags(&["v0.19.890", "v0.19.891", "v0.19.895", "v0.19.897"]),
        v("0.19.893"),
    );
    let mut asked = Vec::new();
    let got = pick_oldest_published(&candidates, |tag| {
        asked.push(tag.to_string());
        Some(true)
    })
    .unwrap();
    assert_eq!(got.0, OldRelease::Published(v("0.19.895"), "v0.19.895".into()));
    assert!(got.1.is_empty(), "nothing was skipped: {:?}", got.1);
    assert_eq!(asked, ["v0.19.895"], "one lookup when the oldest answers");
}

#[test]
fn no_release_at_or_above_req_is_unpublished_without_a_lookup() {
    // After the post-merge bump VERSION == req, but no release is cut yet.
    let candidates = tags_at_or_above(&tags(&["v0.19.890", "v0.19.891"]), v("0.19.893"));
    let got = pick_oldest_published(&candidates, |_| panic!("no lookup expected")).unwrap();
    assert_eq!(got, (OldRelease::Unpublished, Vec::new()));
}

#[test]
fn a_release_still_uploading_is_skipped() {
    let candidates = tags_at_or_above(&tags(&["v0.19.893", "v0.19.895"]), v("0.19.893"));
    let got = pick_oldest_published(&candidates, |tag| Some(tag != "v0.19.893")).unwrap();
    assert_eq!(got.0, OldRelease::Published(v("0.19.895"), "v0.19.895".into()));
    assert_eq!(got.1, ["v0.19.893"], "the skipped tag is handed back");

    let got = pick_oldest_published(&candidates, |_| Some(false)).unwrap();
    assert_eq!(got.0, OldRelease::Unpublished, "nothing uploaded yet");
    assert_eq!(got.1, ["v0.19.893", "v0.19.895"]);
}

// ---- #10868: the note names the release still uploading ----

#[test]
fn stand_in_note_names_the_skipped_tag_and_the_reason() {
    let candidates = tags_at_or_above(&tags(&["v0.19.893", "v0.19.895"]), v("0.19.893"));
    let (picked, skipped) = pick_oldest_published(&candidates, |_| Some(false)).unwrap();
    assert_eq!(picked, OldRelease::Unpublished);
    let note = stand_in_note("B", v("0.19.893"), &skipped);
    assert!(note.contains("no release at or above 0.19.893 is published yet"), "{note}");
    assert!(note.contains("v0.19.893"), "names the first skipped tag: {note}");
    assert!(note.contains("assets are not uploaded yet"), "{note}");
    assert!(note.contains("the new daemon stands in"), "{note}");

    let one = stand_in_note("B", v("0.19.893"), &["v0.19.893".to_string()]);
    assert!(
        one.contains("release v0.19.893 is tagged but its assets are not uploaded yet"),
        "{one}"
    );
}

#[test]
fn stand_in_note_keeps_its_wording_when_no_release_is_tagged() {
    assert_eq!(
        stand_in_note("B", v("0.19.893"), &[]),
        "B: no release at or above 0.19.893 is published yet, so the new daemon stands in for it"
    );
    assert_eq!(skipped_releases_text(&[]), None);
}

// ---- #10868: a listed asset that fails to download is retried ----

#[test]
fn download_that_fails_twice_then_succeeds_passes() {
    let mut calls = 0;
    download_with_retry("v0.19.895", "loom-daemon-test", Duration::ZERO, || {
        calls += 1;
        calls == 3
    })
    .unwrap();
    assert_eq!(calls, 3);
}

#[test]
fn download_succeeding_first_time_is_tried_once() {
    let mut calls = 0;
    download_with_retry("v0.19.895", "loom-daemon-test", Duration::ZERO, || {
        calls += 1;
        true
    })
    .unwrap();
    assert_eq!(calls, 1);
}

#[test]
fn download_that_always_fails_stops_at_the_bound_and_names_tag_and_asset() {
    let mut calls = 0;
    let err = download_with_retry("v0.19.895", "loom-daemon-test", Duration::ZERO, || {
        calls += 1;
        false
    })
    .unwrap_err()
    .to_string();
    assert_eq!(calls, DOWNLOAD_ATTEMPTS);
    assert!(err.contains("v0.19.895"), "{err}");
    assert!(err.contains("loom-daemon-test"), "{err}");
    assert!(err.contains("may still be uploading"), "{err}");
}

#[test]
fn an_unanswerable_lookup_is_an_error_not_a_skip() {
    let candidates = tags_at_or_above(&tags(&["v0.19.893", "v0.19.895"]), v("0.19.893"));
    let err = pick_oldest_published(&candidates, |_| None).unwrap_err();
    assert!(err.to_string().contains("v0.19.893"), "{err}");
}

#[test]
fn lookups_are_capped() {
    let list: Vec<String> = (900..920).map(|n| format!("v0.19.{n}")).collect();
    let candidates = tags_at_or_above(&list.join("\n"), v("0.19.900"));
    let mut calls = 0;
    let err = pick_oldest_published(&candidates, |_| {
        calls += 1;
        Some(false)
    })
    .unwrap_err();
    assert_eq!(calls, MAX_RELEASE_LOOKUPS);
    assert!(err.to_string().contains("--old-daemon"), "{err}");
}

// ---- end to end through `run`, no forge, no probe ----

/// A checkout at `version` with release tags `release_tags` and a `defaults/`
/// holding no shell, so neither direction runs the detector or a daemon.
fn checkout(version: &str, release_tags: &[&str]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join("defaults/docs")).unwrap();
    std::fs::write(root.join("defaults/docs/x.md"), "x\n").unwrap();
    std::fs::write(root.join("VERSION"), format!("{version}\n")).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    for t in release_tags {
        git(root, &["tag", t]);
    }
    dir
}

fn opts(root: &Path, req: &str, old_daemon: OldDaemon) -> CheckOptions {
    CheckOptions {
        repo_root: root.to_path_buf(),
        new_daemon: PathBuf::from("/nonexistent/new-daemon"),
        old_daemon,
        prev_ref: None,
        supports_installed: v("0.19.0"),
        requires_daemon: v(req),
        invoked_files: Vec::new(),
        command_tree: CommandTree::default(),
    }
}

/// A stand-in old daemon that reports `version`.
fn fake_daemon(dir: &Path, version: &str) -> PathBuf {
    script(dir, &format!("fake-{version}"), &format!("echo 'loom-daemon {version}'"))
}

/// An executable `sh` script named `name` in `dir`.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let bin = dir.join(name);
    crate::exec_test_support::write_executable(&bin, format!("#!/bin/sh\n{body}\n"));
    bin
}

#[test]
fn main_after_the_bump_with_no_release_yet_passes_as_unreleased() {
    // The blocking case: REQUIRES_DAEMON was VERSION + 1, the post-merge bump
    // made VERSION == req, and no release is cut. Fetch and Absent both pass.
    let repo = checkout("0.19.893", &["v0.19.890", "v0.19.891"]);
    let fetch = OldDaemon::Fetch {
        repo: "example/none".into(),
        asset: "loom-daemon-test".into(),
    };
    for old in [fetch, OldDaemon::Absent] {
        let report = run(&opts(repo.path(), "0.19.893", old)).unwrap();
        assert!(report.violations.is_empty(), "{:?}", report.violations);
        assert!(
            report
                .notes
                .iter()
                .any(|n| n.contains("no release at or above 0.19.893 is published")),
            "{:?}",
            report.notes
        );
    }
}

#[test]
fn absent_binary_with_a_release_at_or_above_req_names_it() {
    let repo = checkout("0.19.897", &["v0.19.891", "v0.19.895"]);
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Absent)).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    assert!(report.violations[0].contains("v0.19.895"), "{:?}", report.violations);
}

#[test]
fn given_newer_release_stands_in_for_a_skipped_req() {
    let repo = checkout("0.19.897", &["v0.19.891", "v0.19.895"]);
    let bin = fake_daemon(repo.path(), "0.19.895");
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Given(bin))).unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n
                .contains("daemon 0.19.895, the oldest published release at or above 0.19.893")),
        "{:?}",
        report.notes
    );
}

#[test]
fn given_binary_must_be_the_oldest_release_at_or_above_req() {
    let repo = checkout("0.19.899", &["v0.19.891", "v0.19.895", "v0.19.897"]);
    let too_new = fake_daemon(repo.path(), "0.19.897");
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Given(too_new))).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    assert!(report.violations[0].contains("v0.19.895"), "{:?}", report.violations);

    let too_old = fake_daemon(repo.path(), "0.19.891");
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Given(too_old))).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    assert!(
        report.violations[0].contains("below the claimed 0.19.893"),
        "{:?}",
        report.violations
    );
}

// ---- #10868: `--prev-ref` naming the claim itself is checked, not skipped ----

#[test]
fn prev_ref_equal_to_the_claim_is_checked_not_skipped_as_a_declared_break() {
    // The daily proof runs `--prev-ref v<SUPPORTS_INSTALLED>`.
    let repo = checkout("0.19.893", &["v0.19.0", "v0.19.891"]);
    let mut o = opts(repo.path(), "0.19.893", OldDaemon::Absent);
    o.prev_ref = Some("v0.19.0".into());
    let report = run(&o).unwrap();
    assert!(!report.notes.iter().any(|n| n.contains("declared break")), "{:?}", report.notes);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.starts_with("A (v0.19.0 installed files vs new daemon): ")),
        "{:?}",
        report.notes
    );

    // One patch below the claim is the declared break, and is skipped.
    o.supports_installed = v("0.19.1");
    let report = run(&o).unwrap();
    assert!(report.notes.iter().any(|n| n.contains("declared break")), "{:?}", report.notes);
}

// ---- #10868: the three-valued subcommand probe ----

/// A fake daemon whose answer to `<sub> --help` depends on `<sub>`.
fn probe_daemon(dir: &Path) -> PathBuf {
    script(
        dir,
        "probe-daemon",
        r#"case "$1" in
  ok) echo "Usage: loom-daemon ok"; exit 0 ;;
  own-usage) echo "usage: gh-shim <args>" >&2; exit 2 ;;
  gone) echo "error: unrecognized subcommand 'gone'" >&2; exit 2 ;;
  killed) kill -KILL $$ ;;
  panics) echo "thread 'main' panicked at src/main.rs:1:1:" >&2; exit 101 ;;
  exit-101) echo "refused" >&2; exit 101 ;;
esac"#,
    )
}

#[test]
fn probe_tells_present_missing_and_broken_apart() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = probe_daemon(dir.path());
    let mut probe = Probe::default();
    assert_eq!(probe.probe(&daemon, "ok"), Probed::Present);
    assert_eq!(
        probe.probe(&daemon, "own-usage"),
        Probed::Present,
        "a non-zero exit with its own usage is still the subcommand answering"
    );
    assert_eq!(
        probe.probe(&daemon, "exit-101"),
        Probed::Present,
        "exit 101 alone is not a panic"
    );
    assert_eq!(probe.probe(&daemon, "gone"), Probed::Missing);
    let Probed::Broken(why) = probe.probe(&daemon, "killed") else {
        panic!("a daemon killed by a signal must be broken");
    };
    assert!(why.contains("was killed") && why.contains("signal"), "{why}");
    let Probed::Broken(why) = probe.probe(&daemon, "panics") else {
        panic!("a panicking daemon must be broken");
    };
    assert!(why.contains("panicked") && why.contains("101"), "{why}");
}

fn pairs(list: &[(&str, &str)]) -> BTreeSet<(PathBuf, String)> {
    list.iter()
        .map(|(file, sub)| (PathBuf::from(file), (*sub).to_string()))
        .collect()
}

#[test]
fn a_crashing_probe_is_one_violation_and_never_a_per_file_gap() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = probe_daemon(dir.path());
    let mut probe = Probe::default();
    for sub in ["killed", "panics"] {
        // Two files call the same subcommand: still one violation.
        let gaps = probe.gaps(
            &daemon,
            pairs(&[
                ("a.sh", sub),
                ("b.sh", sub),
                ("a.sh", "ok"),
                ("a.sh", "gone"),
            ]),
        );
        assert_eq!(
            gaps.missing.keys().collect::<Vec<_>>(),
            ["gone"],
            "{sub}: only clap's refusal is a per-file gap"
        );
        assert_eq!(gaps.broken.len(), 1, "{sub}: {:?}", gaps.broken);
        let text = &gaps.broken[0];
        assert!(text.contains("probe-daemon"), "names the binary: {text}");
        assert!(text.contains(&format!("{sub} --help")), "names the subcommand: {text}");
        let status = if sub == "killed" {
            "signal"
        } else {
            "exit status: 101"
        };
        assert!(text.contains(status), "names the exit status: {text}");

        let mut report = Report::default();
        record_gaps("A", &daemon, &gaps, dir.path(), &mut report);
        assert_eq!(report.violations.len(), 2, "{:?}", report.violations);
        assert!(gaps
            .summary("A", 2)
            .ends_with("1 subcommand gaps, 1 broken probes"));

        // The second direction to probe the same binary does not repeat it.
        let again = probe.gaps(&daemon, pairs(&[("c.sh", sub)]));
        assert!(again.broken.is_empty(), "{:?}", again.broken);
        assert!(again.missing.is_empty(), "never cached as a gap or as present");
    }
}

#[test]
fn a_daemon_that_cannot_be_executed_is_broken_and_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let not_executable = dir.path().join("not-executable");
    std::fs::write(&not_executable, "#!/bin/sh\nexit 0\n").unwrap();
    for daemon in [not_executable, dir.path().join("no-such-daemon")] {
        let mut probe = Probe::default();
        let gaps = probe
            .gaps(&daemon, pairs(&[("a.sh", "forge"), ("a.sh", "merge-pr"), ("b.sh", "lease")]));
        assert!(gaps.missing.is_empty(), "{:?}", gaps.missing);
        assert_eq!(gaps.broken.len(), 1, "once, not once per subcommand: {:?}", gaps.broken);
        assert!(gaps.broken[0].contains("could not be executed"), "{:?}", gaps.broken);
        assert!(gaps.broken[0].contains(&daemon.display().to_string()), "{:?}", gaps.broken);
        assert!(probe
            .gaps(&daemon, pairs(&[("c.sh", "init")]))
            .broken
            .is_empty());
    }
}

/// Hold `path` open for writing, so the kernel refuses to exec it (`ETXTBSY`),
/// until a helper thread drops the descriptor `hold` from now. Returns when
/// the hold began, to assert on afterwards.
fn held_writer(path: &Path, hold: Duration) -> (Instant, std::thread::JoinHandle<()>) {
    let writer = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    // Clock first: it must precede the releaser's `sleep` by construction, or
    // a late-scheduled parent could see less than `hold` elapse (#15074).
    let started = Instant::now();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(hold);
        drop(writer);
    });
    (started, releaser)
}

#[test]
fn a_daemon_still_open_for_writing_is_waited_for_not_broken_or_unknown() {
    // #15013: a sibling test thread's `fork` can hold a copy of the write fd
    // `script()` just closed, and an exec of the fixture in that window is
    // refused with ETXTBSY. Holding the fixture open for writing stands in
    // for that child; every site that execs a daemon must wait it out.
    let repo = checkout("0.19.897", &["v0.19.891", "v0.19.895"]);
    let daemon = fake_daemon(repo.path(), "0.19.895");

    let (started, releaser) = held_writer(&daemon, Duration::from_millis(30));
    let probed = Probe::default().probe(&daemon, "forge");
    let waited = started.elapsed();
    releaser.join().unwrap();
    assert_eq!(probed, Probed::Present, "a busy daemon is waited for, not broken");
    assert!(
        waited >= Duration::from_millis(30),
        "the probe ran before its writer closed: {waited:?}"
    );

    // `run` execs `git` before direction B reaches the given daemon, so this
    // hold outlasts that preamble.
    let hold = Duration::from_millis(300);
    let (started, releaser) = held_writer(&daemon, hold);
    let report = run(&opts(repo.path(), "0.19.893", OldDaemon::Given(daemon))).unwrap();
    let waited = started.elapsed();
    releaser.join().unwrap();
    assert!(waited >= hold, "the daemon ran before its writer closed: {waited:?}");
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(
        report
            .notes
            .iter()
            .any(|n| n
                .contains("daemon 0.19.895, the oldest published release at or above 0.19.893")),
        "direction B ran against the given daemon: {:?}",
        report.notes
    );
}

#[test]
fn a_clean_probe_summary_keeps_its_wording() {
    let gaps = Gaps::default();
    assert_eq!(gaps.summary("A", 3), "A: 3 shell files, 0 subcommand gaps");
}

// ---- #14985: a hard-floored nested verb is probed, not just its parent ----

/// The repo's own detector, copied into a fixture checkout so `hard_pairs`
/// runs the real pair-emission logic — the same `--list` CI consumes.
const DETECTOR: &str = include_str!("../../../scripts/check-daemon-subcommand-versions.sh");

/// A checkout whose `defaults/scripts/seat.sh` carries `marker` for
/// `session-exec` and calls `session-exec claude-seat` — the PR #14596
/// shape: a nested verb under a hard-floored subcommand.
fn nested_verb_repo(marker: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "t"]);
    std::fs::create_dir_all(root.join("defaults/scripts")).unwrap();
    std::fs::create_dir_all(root.join("scripts")).unwrap();
    std::fs::write(
        root.join("defaults/scripts/seat.sh"),
        format!(
            "#!/usr/bin/env bash\n{marker}\n\"$LOOM_DAEMON_BIN\" session-exec claude-seat --help\n"
        ),
    )
    .unwrap();
    std::fs::write(root.join("scripts/check-daemon-subcommand-versions.sh"), DETECTOR).unwrap();
    std::fs::write(root.join("VERSION"), "0.19.897\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "init"]);
    for t in ["v0.19.890", "v0.19.895"] {
        git(root, &["tag", t]);
    }
    dir
}

/// A daemon reporting 0.19.890 (the oldest release at or above the floor)
/// that HAS `session-exec` but, unless `has_verb`, not its `claude-seat`
/// verb: clap's refusal for the nested probe, a plain answer for the
/// top-level one.
fn nested_verb_daemon(dir: &Path, has_verb: bool) -> PathBuf {
    let verb = if has_verb {
        "      claude-seat) exit 0 ;;"
    } else {
        "      claude-seat) echo \"error: unrecognized subcommand 'claude-seat'\" >&2; exit 2 ;;"
    };
    script(
        dir,
        "old-0.19.890",
        &format!(
            "case \"${{1:-}}\" in
  --version) echo 'loom-daemon 0.19.890'; exit 0 ;;
  session-exec)
    case \"${{2:-}}\" in
{verb}
      *) exit 0 ;;
    esac ;;
esac
exit 0"
        ),
    )
}

/// `run` over [`nested_verb_repo`]: direction B probes `session-exec
/// claude-seat` against the old daemon 0.19.890 — the release the floor
/// names, which has the subcommand but (unless `has_verb`) not the verb;
/// direction A probes the previous release's copy against a daemon that
/// answers everything.
fn nested_verb_report(marker: &str, has_verb: bool) -> Report {
    let repo = nested_verb_repo(marker);
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(
        repo.path(),
        "0.19.890",
        OldDaemon::Given(nested_verb_daemon(dir.path(), has_verb)),
    );
    o.new_daemon = fake_daemon(dir.path(), "0.19.99");
    let report = run(&o).unwrap();
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.starts_with("A (v0.19.895 installed files vs new daemon): ")),
        "direction A ran: {:?}",
        report.notes
    );
    report
}

#[test]
fn a_nested_verb_missing_from_the_old_daemon_is_a_violation() {
    let report = nested_verb_report("# requires-daemon: session-exec >= 0.19.890", false);
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let v = &report.violations[0];
    assert!(v.contains("defaults/scripts/seat.sh"), "names the file: {v}");
    assert!(v.contains("`session-exec claude-seat`"), "names the verb path: {v}");
    assert!(v.contains("old-0.19.890"), "names the daemon: {v}");
    assert!(v.contains("0.19.890"), "names the daemon's release: {v}");
}

#[test]
fn a_nested_verb_present_in_the_old_daemon_passes() {
    let report = nested_verb_report("# requires-daemon: session-exec >= 0.19.890", true);
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(
        report.notes.iter().any(|n| n.contains(
            "B (new installed files vs daemon 0.19.890): 1 shell files, 0 subcommand gaps"
        )),
        "{:?}",
        report.notes
    );
}

#[test]
fn an_optional_marker_still_skips_a_nested_call() {
    // The daemon LACKS the verb; the `optional` marker must keep the pair
    // out of the probe exactly as it does for a plain subcommand.
    let report = nested_verb_report("# requires-daemon: session-exec optional", false);
    assert!(report.violations.is_empty(), "{:?}", report.violations);
}

#[test]
fn an_unfloored_nested_call_keeps_its_top_level_probe() {
    // No marker at all: the pair stays `session-exec`, so only the
    // subcommand's presence is asked of the old daemon (#14985 keeps the
    // deeper probe for hard-floored calls only).
    let report = nested_verb_report("# prose, not a marker", false);
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(
        report.notes.iter().any(|n| n.contains("0 subcommand gaps")),
        "{:?}",
        report.notes
    );
}

#[test]
fn the_probe_runs_the_full_verb_path() {
    let dir = tempfile::tempdir().unwrap();
    let daemon = script(
        dir.path(),
        "verb-daemon",
        r#"case "$1" in
  seat)
    case "${2:-}" in
      claude-seat) echo "error: unrecognized subcommand 'claude-seat'" >&2; exit 2 ;;
      *) exit 0 ;;
    esac ;;
esac"#,
    );
    let mut probe = Probe::default();
    assert_eq!(probe.probe(&daemon, "seat"), Probed::Present);
    assert_eq!(probe.probe(&daemon, "seat claude-seat"), Probed::Missing);
    assert_eq!(
        probe.probe(&daemon, "seat other-seat"),
        Probed::Present,
        "a verb the daemon has, reached through the path, is present"
    );
}

// ---- #15644: the daemon calls in shipped Markdown prompts ----

/// A command tree from space-separated paths. A trailing `*` marks the path a
/// passthrough (`forge issue*`), whose tail belongs to another program.
fn tree(paths: &[&str]) -> CommandTree {
    let mut tree = CommandTree::default();
    for path in paths {
        let (path, passthrough) = path.strip_suffix('*').map_or((*path, false), |p| (p, true));
        tree.insert(path, passthrough);
    }
    tree
}

/// The command paths the calls in `text` (Markdown) resolve to in `tree`.
fn paths(tree: &CommandTree, text: &str) -> BTreeSet<String> {
    prompt_calls::markdown(text)
        .iter()
        .flat_map(|call| tree.resolve(call))
        .collect()
}

/// The shapes #15644 lists, in one prompt: Builder's inline calls and wrapped
/// span, Guide's nested verb, Champion's quoted executable and passthroughs,
/// `loom.md`'s brace lists, and the prose, fences and noise around them.
const PROMPT: &str = r##"---
name: fixture
description: Call `loom-daemon preflight --issue N` first.
---

Park the issue (`loom-daemon park-record apply --issue <number> --by builder`),
or run `loom-daemon dispatch <N> --workspace "$WORKSPACE_ROOT" [--model M]`.
The `loom-daemon` binary: loom-daemon is running. Neither `loom-daemon --help`
nor `loom-daemon-start.sh` is a call, nor are `loom-daemon/src/ipc.rs` and
`cargo test --bin loom-daemon`.

```bash
# loom-daemon forge gone-refs: a comment is not a call either
loom-daemon forge check-claim <number>   # exit 0 PRINTS the blocker token
command -v loom-daemon >/dev/null 2>&1 || return 0
out="$(loom-daemon guide work-plan-receipt read --docs-worktree "$DOCS_WT" \
    --field "$1" 2>/dev/null)"
LOOM_HOLD="x" loom-daemon park-record apply --pr "$PR_NUMBER" --reason "champion hold" \
    --by champion >/dev/null 2>&1 || true
printf '%s' "$body" | "${LOOM_DAEMON_BIN:-loom-daemon}" forge workflow-secrets-check >/dev/null 2>&1 || return 1
echo "run loom-daemon forge issue create by hand (loom-daemon status)"
```

- `loom-daemon merge-pr-refs
  loom-issue-trailer-refs` is the machine-readable link.
- `loom-daemon tokens {select,bootstrap,check}` and `loom-daemon host {disable,enable,status}`.
- `loom-daemon forge issue create` and `loom-daemon forge pr view <number> --json state` forward to `gh`.
- Prose only: loom-daemon is running, and loom-daemon leese ensure is a typo.

```
loom-daemon cancel --issue <N>        # or: loom-daemon cancel <sweep-id>
```
"##;

/// Every command the fixture names, plus decoys for each word that must NOT
/// become a command path: a decoy that turns up in the result is a leak.
const PROMPT_TREE: &[&str] = &[
    "preflight",
    "park-record",
    "park-record apply",
    "dispatch",
    "forge",
    "forge check-claim",
    "forge workflow-secrets-check",
    "forge issue*",
    "forge pr*",
    "forge gone-refs",
    "forge create",
    "guide",
    "guide work-plan-receipt",
    "guide work-plan-receipt read",
    "merge-pr-refs",
    "merge-pr-refs loom-issue-trailer-refs",
    "tokens select",
    "tokens bootstrap",
    "tokens check",
    "host disable",
    "host enable",
    "host status",
    "cancel",
    "is",
    "running",
    "leese",
    "leese ensure",
    "help",
    "start",
    "status",
    "src",
    "test",
    "v",
    "return",
    "echo",
    "printf",
    "gh",
];

#[test]
fn prompt_calls_resolve_to_the_exact_command_paths() {
    let tree = tree(PROMPT_TREE);
    let got = paths(&tree, PROMPT);
    let want: BTreeSet<String> = [
        "preflight",
        "park-record apply",
        "dispatch",
        "forge check-claim",
        "forge workflow-secrets-check",
        "guide work-plan-receipt read",
        "merge-pr-refs loom-issue-trailer-refs",
        "tokens select",
        "tokens bootstrap",
        "tokens check",
        "host disable",
        "host enable",
        "host status",
        "forge issue",
        "forge pr",
        "cancel",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert_eq!(got, want);
}

#[test]
fn a_wrapped_inline_command_is_one_call_per_line_pair() {
    // The span opens on one source line and closes on the next.
    let got = paths(
        &tree(&["lease", "lease pr", "lease pr release"]),
        "- `loom-daemon lease pr\n  release <n> --role judge` loses the claim.",
    );
    assert_eq!(
        got,
        ["lease pr release"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    );
}

#[test]
fn a_code_span_stays_inside_its_own_block() {
    // An unclosed backtick never swallows the next block's span: a heading is
    // its own block, so is each list item, and a blank line ends a paragraph.
    let text = "# Run `loom-daemon status\nThen `loom-daemon cancel --issue <N>`.\n- `loom-daemon dispatch <N>\n- and `loom-daemon preflight --issue N`\n\n`loom-daemon quarantine clear <issue>`";
    let got = paths(
        &tree(&[
            "status",
            "cancel",
            "dispatch",
            "preflight",
            "quarantine clear",
        ]),
        text,
    );
    assert_eq!(
        got,
        ["cancel", "preflight", "quarantine clear"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    );
}

#[test]
fn a_fence_in_another_language_is_not_read() {
    let text = "```json\n{\"cmd\": \"loom-daemon status\"}\n```\n\n```python\nsubprocess.run(['loom-daemon', 'status'])\n```\n\n```markdown\n- [ ] Passes (`loom-daemon preflight --issue N`)\n```\n\n```typescript\nconst x = `loom-daemon cancel`;\n```\n";
    let got = paths(&tree(&["status", "preflight", "cancel"]), text);
    assert_eq!(
        got,
        ["preflight"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    );
}

#[test]
fn a_word_the_new_daemon_does_not_know_ends_the_path() {
    let tree = tree(&["forge", "guide"]);
    // `gone-refs` is no verb of this daemon: the path stops at `forge`.
    assert_eq!(
        paths(&tree, "`loom-daemon forge gone-refs --x`"),
        ["forge"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>()
    );
    // A first word it does not know names nothing, so nothing is probed —
    // however the floor would have answered.
    assert!(paths(&tree, "```\nloom-daemon is running\n```\n").is_empty());
    assert!(paths(&tree, "`loom-daemon not-a-command`").is_empty());
    // The bare executable, or one followed by a flag, names no command.
    assert!(paths(&tree, "`loom-daemon` and `loom-daemon --help`").is_empty());
}

#[test]
fn the_tree_comes_from_clap_and_stops_at_a_trailing_catch_all() {
    use clap::{arg, Command};
    let cli = Command::new("loom-daemon")
        .subcommand(
            Command::new("forge")
                .alias("gh-shim")
                .subcommand(
                    Command::new("issue").arg(
                        arg!(<args> ... "gh passthrough")
                            .trailing_var_arg(true)
                            .allow_hyphen_values(true),
                    ),
                )
                .subcommand(Command::new("rerun").alias("re-run")),
        )
        .subcommand(Command::new("status").arg(arg!(--pipeline)));
    let tree = CommandTree::from_clap(&cli);
    let call = |words: &[&str]| {
        words
            .iter()
            .map(|w| vec![(*w).to_string()])
            .collect::<Vec<_>>()
    };
    assert_eq!(tree.resolve(&call(&["forge", "issue", "create"])), ["forge issue"]);
    assert_eq!(tree.resolve(&call(&["gh-shim", "re-run", "--failed"])), ["forge rerun"]);
    assert_eq!(tree.resolve(&call(&["status", "--pipeline"])), ["status"]);
    assert!(tree.resolve(&call(&["leese"])).is_empty());
}

/// A checkout whose only prompt is `prompt`, under the directory the harness
/// scans.
fn prompt_repo(prompt: &str) -> tempfile::TempDir {
    let dir = checkout("0.19.899", &["v0.19.891", "v0.19.895"]);
    let prompts = dir.path().join("defaults/.claude/commands/loom");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(prompts.join("champion-pr-merge.md"), prompt).unwrap();
    dir
}

/// The shipped Champion prompt's call, the one #15639 found above the floor.
const SECRETS_CHECK: &str = r#"printf '%s' "$body" | "${LOOM_DAEMON_BIN:-loom-daemon}" forge workflow-secrets-check >/dev/null 2>&1 || return 1"#;

/// A floor daemon reporting `version` that logs every invocation to `log`,
/// answers `forge` and its `issue` passthrough, and knows
/// `forge workflow-secrets-check` only when `has_verb`. Anything else is
/// clap's refusal.
fn floor_daemon(dir: &Path, version: &str, has_verb: bool, log: &Path) -> PathBuf {
    let answer = if has_verb {
        "exit 0 ;;".to_string()
    } else {
        r#"echo "error: unrecognized subcommand 'workflow-secrets-check'" >&2; exit 2 ;;"#
            .to_string()
    };
    script(
        dir,
        &format!("floor-{version}"),
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "${{1:-}}" in
  --version) echo 'loom-daemon {version}'; exit 0 ;;
  forge)
    case "${{2:-}}" in
      --help | issue) exit 0 ;;
      workflow-secrets-check) {answer}
      *) echo "error: unrecognized subcommand '${{2:-}}'" >&2; exit 2 ;;
    esac ;;
  *) echo "error: unrecognized subcommand '${{1:-}}'" >&2; exit 2 ;;
esac"#,
            log.display()
        ),
    )
}

fn secrets_check_report(has_verb: bool) -> (Report, String) {
    let repo = prompt_repo(&format!("Merge it:\n\n```bash\n{SECRETS_CHECK}\n```\n"));
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("probes.log");
    let mut o = opts(
        repo.path(),
        "0.19.893",
        OldDaemon::Given(floor_daemon(scratch.path(), "0.19.895", has_verb, &log)),
    );
    o.command_tree = tree(&["forge", "forge workflow-secrets-check"]);
    let report = run(&o).unwrap();
    let probes = std::fs::read_to_string(&log).unwrap_or_default();
    (report, probes)
}

#[test]
fn a_prompt_verb_missing_from_the_old_daemon_is_a_violation_naming_both() {
    let (report, probes) = secrets_check_report(false);
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let v = &report.violations[0];
    assert!(
        v.contains("defaults/.claude/commands/loom/champion-pr-merge.md"),
        "names the prompt: {v}"
    );
    assert!(v.contains("`forge workflow-secrets-check`"), "names the full path: {v}");
    assert!(v.contains("floor-0.19.895"), "names the daemon: {v}");
    // The tree of the NEW daemon decided the depth, so the floor was asked
    // about the child although it knows the parent.
    assert_eq!(
        probes
            .lines()
            .filter(|l| *l == "forge workflow-secrets-check --help")
            .count(),
        1,
        "{probes}"
    );
}

#[test]
fn a_prompt_verb_present_in_the_old_daemon_passes() {
    let (report, probes) = secrets_check_report(true);
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(probes.contains("forge workflow-secrets-check --help"), "{probes}");
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("1 prompt files, 1 command paths, 0 subcommand gaps")),
        "{:?}",
        report.notes
    );
}

#[test]
fn a_broken_probe_stays_apart_from_a_missing_verb() {
    // The daemon dies on the probe: one violation, worded as a crash, never
    // as "has no `forge workflow-secrets-check` subcommand".
    let repo = prompt_repo(&format!("```bash\n{SECRETS_CHECK}\n```\n"));
    let scratch = tempfile::tempdir().unwrap();
    let daemon = script(
        scratch.path(),
        "floor-dying",
        r#"case "${1:-}" in
  --version) echo 'loom-daemon 0.19.895'; exit 0 ;;
  forge) kill -KILL $$ ;;
esac"#,
    );
    let mut o = opts(repo.path(), "0.19.893", OldDaemon::Given(daemon));
    o.command_tree = tree(&["forge", "forge workflow-secrets-check"]);
    let report = run(&o).unwrap();
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let v = &report.violations[0];
    assert!(v.contains("forge workflow-secrets-check --help"), "{v}");
    assert!(v.contains("was killed"), "{v}");
    assert!(!v.contains("has no"), "{v}");
}

#[test]
fn repeated_mentions_share_a_probe_and_keep_every_prompt_in_the_gap() {
    let repo = checkout("0.19.899", &["v0.19.891", "v0.19.895"]);
    let prompts = repo.path().join("defaults/.claude/commands/loom");
    std::fs::create_dir_all(&prompts).unwrap();
    std::fs::write(
        prompts.join("doctor.md"),
        "```bash\nloom-daemon forge wait-checks <number> --timeout 20\n```\n",
    )
    .unwrap();
    std::fs::write(
        prompts.join("judge.md"),
        "Then `loom-daemon forge wait-checks --settle-red`.\n\nAnd `loom-daemon forge wait-checks` again.\n",
    )
    .unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("probes.log");
    let daemon = script(
        scratch.path(),
        "floor-0.19.895",
        &format!(
            r#"printf '%s\n' "$*" >> '{}'
case "${{1:-}}" in
  --version) echo 'loom-daemon 0.19.895'; exit 0 ;;
  forge)
    case "${{2:-}}" in
      --help) exit 0 ;;
    esac ;;
esac
echo "error: unrecognized subcommand" >&2
exit 2"#,
            log.display()
        ),
    );
    let mut o = opts(repo.path(), "0.19.893", OldDaemon::Given(daemon));
    o.command_tree = tree(&["forge", "forge wait-checks"]);
    let report = run(&o).unwrap();
    let probes = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        probes
            .lines()
            .filter(|l| *l == "forge wait-checks --help")
            .count(),
        1,
        "three mentions, one probe: {probes}"
    );
    assert_eq!(report.violations.len(), 1, "{:?}", report.violations);
    let v = &report.violations[0];
    assert!(v.contains("loom/doctor.md") && v.contains("loom/judge.md"), "names both: {v}");
    assert!(
        report
            .notes
            .iter()
            .any(|n| n.contains("2 prompt files, 1 command paths, 1 subcommand gaps")),
        "{:?}",
        report.notes
    );
}

#[test]
fn passthrough_arguments_and_prose_never_reach_the_probe() {
    let repo = prompt_repo(
        "```bash\nloom-daemon forge issue view 42 --json state\n```\n\nRun loom-daemon gone-refs by hand, or `loom-daemon gone-refs --x`.\n",
    );
    let scratch = tempfile::tempdir().unwrap();
    let log = scratch.path().join("probes.log");
    let daemon = floor_daemon(scratch.path(), "0.19.895", false, &log);
    let mut o = opts(repo.path(), "0.19.893", OldDaemon::Given(daemon));
    o.command_tree = tree(&["forge", "forge issue*"]);
    let report = run(&o).unwrap();
    let probes = std::fs::read_to_string(&log).unwrap();
    assert_eq!(
        probes
            .lines()
            .filter(|l| !l.contains("--version"))
            .collect::<Vec<_>>(),
        ["forge issue --help"],
        "the passthrough itself, and nothing after it: {probes}"
    );
    assert!(report.violations.is_empty(), "{:?}", report.violations);
}

#[test]
fn a_tree_without_prompts_reports_no_prompt_summary() {
    // No prompt directory (a consumer repo's fixture): nothing to report.
    let repo = checkout("0.19.893", &["v0.19.890", "v0.19.891"]);
    let mut o = opts(repo.path(), "0.19.893", OldDaemon::Absent);
    o.command_tree = tree(PROMPT_TREE);
    let report = run(&o).unwrap();
    assert!(report.violations.is_empty(), "{:?}", report.violations);
    assert!(
        !report.notes.iter().any(|n| n.contains("prompt files")),
        "no prompt summary without prompts: {:?}",
        report.notes
    );
}

/// The real prompts against the real command tree, on an 8 MiB thread like
/// the binary's main thread: the tree build needs more than libtest's 2 MiB
/// in a debug build (#10616). Every call #15644 found by hand is among the
/// pairs direction B probes.
#[test]
fn the_shipped_prompts_resolve_against_this_daemon() {
    let defaults = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../defaults");
    let prompts = shipped_prompts(&defaults).unwrap();
    assert!(prompts.len() > 40, "found {} prompts", prompts.len());
    let calls = crate::cli::whole_cli_parse::on_stack(
        "prompt-calls-tree",
        crate::cli::whole_cli_parse::MAIN_THREAD_STACK,
        move || prompt_calls::scan(&prompts, &CommandTree::loom_daemon()),
    )
    .unwrap();
    let found: BTreeSet<String> = calls.pairs.iter().map(|(_, p)| p.clone()).collect();
    for path in [
        "forge workflow-secrets-check",
        "guide work-plan-receipt read",
        "guide work-plan-splice",
        "merge-pr-refs loom-issue-trailer-refs",
        "host disable",
        "host enable",
        "host status",
        "tokens select",
        "forge issue",
        "forge pr",
        "lease pr claim",
        "park-record decision",
    ] {
        assert!(found.contains(path), "{path} not probed; found {found:#?}");
    }
    assert!(
        !found
            .iter()
            .any(|p| p.starts_with("forge issue ") || p.starts_with("forge pr ")),
        "a passthrough's tail was probed: {found:?}"
    );
}

// ---- #15667 review: blockquoted fences, parent options, unreadable prompts ----

#[test]
fn a_fence_inside_a_blockquote_is_read() {
    let t = tree(&["affected-files check"]);
    let text = "> ```bash\n> git fetch origin --quiet\n> loom-daemon affected-files check --body-file \"$B\"; rc=$?\n> ```\n";
    assert_eq!(paths(&t, text), BTreeSet::from(["affected-files check".to_string()]));
    // A `>` line inside an ordinary fence is a redirect, not a quote marker.
    let plain = "```bash\n> out\nloom-daemon affected-files check\n```\n";
    assert_eq!(paths(&t, plain), BTreeSet::from(["affected-files check".to_string()]));
}

#[test]
fn a_parent_option_does_not_hide_the_child_command() {
    let mut t = tree(&["accounts list"]);
    t.insert_option("accounts", "--workspace", true);
    t.insert_option("accounts", "--json", false);
    let want = BTreeSet::from(["accounts list".to_string()]);
    for text in [
        "`loom-daemon accounts --workspace . list`",
        "`loom-daemon accounts --workspace=. list`",
        "`loom-daemon accounts --json list`",
        "`loom-daemon accounts list --workspace .`",
    ] {
        assert_eq!(paths(&t, text), want, "{text}");
    }
}

#[test]
fn an_unreadable_prompt_fails_the_scan_naming_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bad.md");
    std::fs::write(&file, b"<!-- \xff -->\n`loom-daemon affected-files check`\n").unwrap();
    let err = prompt_calls::scan(&[file], &tree(&["affected-files check"])).unwrap_err();
    assert!(format!("{err:#}").contains("bad.md"), "{err:#}");
}
