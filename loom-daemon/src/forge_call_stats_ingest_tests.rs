//! Unit tests for [`super`] (the agent-row ingest, #10607 slice B).
#![allow(clippy::unwrap_used)]

use super::*;
use std::io::Write;

const NOW: i64 = 1_800_000_000;
const HOUR: i64 = NOW / 3600;

fn file(dir: &Path, hour: i64) -> PathBuf {
    dir.join(format!("calls-{hour}.jsonl"))
}

fn append(path: &Path, text: &str) {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap();
    f.write_all(text.as_bytes()).unwrap();
}

/// A W5 passthrough row as the front writes it.
fn passthrough(t: i64) -> String {
    format!(
        r#"{{"t":{t},"c":"agent.gh.pr","p":"graphql","o":"ok","op":"unknown","pv":"github","og":"github.com","rp":"Acme/widget","ir":"agent-builder","ro":"target","ca":"app-4486636","co":"Acme","ci":"55501","tk":"writer","rr":"graphql","ag":"builder","vi":"passthrough"}}"#
    ) + "\n"
}

/// A served facade row (a free 304).
fn served(t: i64) -> String {
    format!(
        r#"{{"t":{t},"c":"agent_gh_front","p":"core","o":"not_modified","op":"issue.view","rp":"acme/widget","ir":"reader","ca":"app-5100879","co":"acme","tk":"reader","rr":"core","ag":"judge","vi":"served"}}"#
    ) + "\n"
}

/// A daemon row: no stamp, already exported by the facade itself.
fn daemon(t: i64) -> String {
    format!(r#"{{"t":{t},"c":"issue.view","p":"graphql","o":"ok","op":"issue.view"}}"#) + "\n"
}

fn primed(sink: &Path) -> Cursors {
    let mut cursors = Cursors::default();
    let first = drain(sink, &mut cursors, NOW);
    assert!(first.calls.is_empty(), "the first tick only primes");
    cursors
}

#[test]
fn only_new_stamped_rows_are_ingested_and_never_twice() {
    let sink = tempfile::tempdir().unwrap();
    let f = file(sink.path(), HOUR);
    append(&f, &passthrough(NOW - 10)); // before the daemon started
    let mut cursors = primed(sink.path());
    append(&f, &daemon(NOW));
    append(&f, &passthrough(NOW));
    append(&f, &served(NOW));
    let got = drain(sink.path(), &mut cursors, NOW);
    assert_eq!(got.rejected, 0);
    assert_eq!(got.calls.len(), 2, "{:?}", got.calls);
    let pr = &got.calls[0];
    assert_eq!((pr.agent, pr.via, pr.source), ("builder", "passthrough", Source::Host));
    assert_eq!(pr.labels.caller, "agent.gh.pr");
    assert_eq!(pr.labels.role, "agent-builder");
    assert_eq!(pr.labels.account, "app-4486636");
    assert_eq!(
        (pr.labels.cred_owner.as_str(), pr.labels.target_owner.as_str()),
        ("acme", "acme")
    );
    assert_eq!((pr.labels.resource.as_str(), pr.labels.outcome), ("graphql", CallOutcome::Ok));
    assert_eq!(pr.labels.installation, "55501");
    let view = &got.calls[1];
    assert_eq!((view.agent, view.via), ("judge", "served"));
    assert_eq!(view.labels.outcome, CallOutcome::NotModified);
    assert_eq!(view.labels.op, "issue.view");
    assert_eq!(view.labels.installation, "-", "no ci: no installation");
    assert!(drain(sink.path(), &mut cursors, NOW).calls.is_empty(), "never re-emitted");
}

#[test]
fn a_partial_trailing_line_waits_for_the_next_tick() {
    let sink = tempfile::tempdir().unwrap();
    let f = file(sink.path(), HOUR);
    let mut cursors = primed(sink.path());
    let row = passthrough(NOW);
    let (head, tail) = row.split_at(40);
    append(&f, head);
    assert!(drain(sink.path(), &mut cursors, NOW).calls.is_empty());
    append(&f, tail);
    assert_eq!(drain(sink.path(), &mut cursors, NOW).calls.len(), 1);
}

#[test]
fn a_new_hour_file_is_read_from_its_start_and_a_pruned_one_forgotten() {
    let sink = tempfile::tempdir().unwrap();
    let mut cursors = primed(sink.path());
    append(&file(sink.path(), HOUR + 1), &passthrough(NOW + 3600));
    let got = drain(sink.path(), &mut cursors, NOW + 3600);
    assert_eq!(got.calls.len(), 1);
    let later = NOW + (RETAIN_HOURS + 3) * 3600;
    let _ = drain(sink.path(), &mut cursors, later);
    assert!(!cursors.offsets.contains_key(&file(sink.path(), HOUR + 1)));
}

#[test]
fn a_shrunk_file_is_never_re_read() {
    let sink = tempfile::tempdir().unwrap();
    let f = file(sink.path(), HOUR);
    let mut cursors = primed(sink.path());
    append(&f, &passthrough(NOW));
    append(&f, &passthrough(NOW));
    assert_eq!(drain(sink.path(), &mut cursors, NOW).calls.len(), 2);
    std::fs::write(&f, passthrough(NOW)).unwrap();
    assert!(drain(sink.path(), &mut cursors, NOW).calls.is_empty());
}

#[test]
fn rows_outside_the_vocabulary_are_refused() {
    let sink = tempfile::tempdir().unwrap();
    let f = file(sink.path(), HOUR);
    let mut cursors = primed(sink.path());
    let good = passthrough(NOW);
    let bad = [
        good.replace(r#""ag":"builder""#, r#""ag":"root""#),
        good.replace(r#""c":"agent.gh.pr""#, r#""c":"agent.gh.help""#),
        good.replace(r#""c":"agent.gh.pr""#, r#""c":"work_finder""#),
        good.replace(r#""vi":"passthrough""#, r#""vi":"served""#),
        good.replace(r#""vi":"passthrough""#, r#""vi":"x""#),
        passthrough(NOW + 3600),
        passthrough(NOW - (RETAIN_HOURS + 2) * 3600),
        "not json\n".to_string(),
        format!("{}\n", "x".repeat(MAX_LINE_BYTES + 1)),
    ];
    for b in &bad {
        append(&f, b);
    }
    let got = drain(sink.path(), &mut cursors, NOW);
    assert!(got.calls.is_empty(), "{:?}", got.calls);
    assert_eq!(got.rejected, bad.len() as u64);
}

#[test]
fn labels_are_re_derived_never_copied() {
    let sink = tempfile::tempdir().unwrap();
    let f = file(sink.path(), HOUR);
    let mut cursors = primed(sink.path());
    let hostile = passthrough(NOW)
        .replace("app-4486636", "ghp_secret")
        .replace(r#""co":"Acme""#, r#""co":"../../etc""#)
        .replace(r#""ci":"55501""#, r#""ci":"1;rm -rf""#)
        .replace("Acme/widget", "evil owner/x")
        .replace(r#""ir":"agent-builder""#, r#""ir":"agent-$(id)""#)
        .replace(r#""op":"unknown""#, r#""op":"Issue View!""#)
        .replace(r#""rr":"graphql""#, r#""rr":"GRAPHQL/../x""#)
        .replace(r#""tk":"writer""#, r#""tk":"writer","pg":4000000000"#);
    append(&f, &hostile);
    let got = drain(sink.path(), &mut cursors, NOW);
    let l = &got.calls[0].labels;
    assert_eq!(l.account, "unknown");
    assert_eq!(l.installation, "-");
    assert_eq!((l.cred_owner.as_str(), l.target_owner.as_str()), ("unknown", "unknown"));
    assert_eq!((l.role.as_str(), l.op.as_str()), ("agent-other", "unknown"));
    assert_eq!(l.resource, "graphql", "an invalid rr falls back to the pool");
    assert_eq!(got.calls[0].requests, u64::from(MAX_ROW_REQUESTS));
}

#[cfg(unix)]
#[test]
fn the_contained_dir_is_read_and_swept_without_following_links() {
    let sink = tempfile::tempdir().unwrap();
    let contained = sink.path().join(CONTAINED_SUBDIR);
    std::fs::create_dir(&contained).unwrap();
    let mut cursors = primed(sink.path());
    append(&file(&contained, HOUR), &served(NOW));
    // A link to a file the daemon can read, named like a sink file.
    let secret = sink.path().join("secret");
    std::fs::write(&secret, passthrough(NOW)).unwrap();
    std::os::unix::fs::symlink(&secret, file(&contained, HOUR - 1)).unwrap();
    // A FIFO must never block the tick.
    let fifo = std::ffi::CString::new(
        file(&contained, HOUR - 2)
            .into_os_string()
            .into_encoded_bytes(),
    )
    .unwrap();
    // SAFETY: mkfifo only reads the NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let stale = file(&contained, HOUR - RETAIN_HOURS - 5);
    std::fs::write(&stale, "").unwrap();
    // A file no front writes (disk filler) is removed too.
    let junk = contained.join("filler.bin");
    std::fs::write(&junk, "x").unwrap();
    let got = drain(sink.path(), &mut cursors, NOW);
    assert_eq!(got.calls.len(), 1, "{:?}", got.calls);
    assert_eq!(got.calls[0].source, Source::Contained);
    assert_eq!(got.removed, 4, "the link, the FIFO, the stale hour and the filler");
    assert!(secret.exists(), "a link is removed, never its target");
    assert!(!stale.exists());
    assert!(!junk.exists());
}

#[test]
fn an_oversize_contained_file_is_removed() {
    let sink = tempfile::tempdir().unwrap();
    let contained = sink.path().join(CONTAINED_SUBDIR);
    std::fs::create_dir(&contained).unwrap();
    let big = file(&contained, HOUR);
    let f = std::fs::File::create(&big).unwrap();
    f.set_len(MAX_CONTAINED_FILE_BYTES + 1).unwrap();
    assert_eq!(sweep_contained(&contained, HOUR), 1);
    assert!(!big.exists());
}

/// Host-only names accepted by `is_sink_entry` (the parent dir's mount
/// gate) but never written by a front into the contained dir: the bucket
/// book snapshot, its legacy staging branch, and the `write_atomic`
/// staging shape of #14978. Only `calls-<hour>.jsonl` is non-foreign
/// there (#14993).
#[test]
fn host_only_sink_names_are_foreign_in_the_contained_dir() {
    let sink = tempfile::tempdir().unwrap();
    let contained = sink.path().join(CONTAINED_SUBDIR);
    std::fs::create_dir(&contained).unwrap();
    let keep = file(&contained, HOUR);
    std::fs::write(&keep, "").unwrap();
    for name in ["bucket-book.json", ".bucket-book.json.123", ".tmpAbC123"] {
        std::fs::write(contained.join(name), "x").unwrap();
    }
    // A negative hour parses and is stale, exactly as before.
    std::fs::write(file(&contained, -5), "").unwrap();
    // A subdirectory is never touched.
    std::fs::create_dir(contained.join("adir")).unwrap();
    assert_eq!(
        sweep_contained(&contained, HOUR),
        4,
        "the three host-only names and the stale hour"
    );
    assert!(keep.exists(), "the in-window hour file stays");
    assert!(contained.join("adir").exists(), "a subdirectory is left alone");
    assert!(!file(&contained, -5).exists(), "calls--5.jsonl is stale");
    for name in ["bucket-book.json", ".bucket-book.json.123", ".tmpAbC123"] {
        assert!(!contained.join(name).exists(), "{name} removed");
    }
}

/// A spelling that parses to an in-window hour but is not byte-equal to
/// what [`super::sink::append`] writes is foreign residue, not an hour
/// file: removed and counted, while the canonical name stays (#15000).
#[test]
fn padded_hour_spellings_are_foreign_and_counted() {
    let sink = tempfile::tempdir().unwrap();
    let contained = sink.path().join(CONTAINED_SUBDIR);
    std::fs::create_dir(&contained).unwrap();
    let keep = file(&contained, HOUR);
    std::fs::write(&keep, "").unwrap();
    let padded = [
        format!("calls-0{HOUR}.jsonl"),
        format!("calls-00{HOUR}.jsonl"),
        format!("calls-+{HOUR}.jsonl"),
        format!("calls-+0{HOUR}.jsonl"),
        format!("calls-0{}.jsonl", HOUR - RETAIN_HOURS),
    ];
    for name in &padded {
        std::fs::write(contained.join(name), "x").unwrap();
    }
    assert_eq!(sweep_contained(&contained, HOUR), padded.len() as u64);
    assert!(keep.exists(), "the canonical name the sink writes stays");
    let survivors: Vec<String> = std::fs::read_dir(&contained)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(survivors, vec![format!("calls-{HOUR}.jsonl")]);
}

/// A container cannot starve the [`MAX_DIR_ENTRIES`] scan budget with
/// padded `calls-<hour>.jsonl` spellings: every one is foreign, so
/// repeated ticks remove them all while the canonical hour file a front
/// writes survives (#15000). The 255-byte name limit caps one hour's
/// spellings well below the budget, so the fill spans the whole retained
/// window to get past it.
#[test]
fn padded_spellings_beyond_the_scan_budget_cannot_starve_the_sweep() {
    let sink = tempfile::tempdir().unwrap();
    let contained = sink.path().join(CONTAINED_SUBDIR);
    std::fs::create_dir(&contained).unwrap();
    let keep = file(&contained, HOUR);
    std::fs::write(&keep, "").unwrap();
    let mut filled = 0usize;
    for h in HOUR - RETAIN_HOURS..=HOUR + 1 {
        let digits = h.to_string();
        let room = 255 - "calls-".len() - ".jsonl".len() - digits.len();
        for pad in 1..=room {
            let name = format!("calls-{}{digits}.jsonl", "0".repeat(pad));
            std::fs::write(contained.join(name), "").unwrap();
            filled += 1;
        }
    }
    assert!(filled > MAX_DIR_ENTRIES, "the fill ({filled}) must exceed the scan budget");
    let mut ticks = 0;
    let mut total_removed = 0u64;
    loop {
        ticks += 1;
        let removed = sweep_contained(&contained, HOUR);
        total_removed += removed;
        if removed == 0 {
            break;
        }
        assert!(ticks < 10, "the sweep must converge");
    }
    assert!(ticks >= 2, "the capped scan forces more than one tick");
    assert_eq!(total_removed, filled as u64, "every padded spelling goes");
    let survivors: Vec<String> = std::fs::read_dir(&contained)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(survivors, vec![format!("calls-{HOUR}.jsonl")]);
}

#[test]
fn a_line_longer_than_a_read_is_skipped_not_stuck_on() {
    let sink = tempfile::tempdir().unwrap();
    let f = file(sink.path(), HOUR);
    let mut cursors = primed(sink.path());
    let fh = std::fs::File::create(&f).unwrap();
    fh.set_len(MAX_READ_PER_FILE + 10).unwrap(); // NULs, no newline
    let got = drain(sink.path(), &mut cursors, NOW);
    assert_eq!(got.rejected, 1);
    append(&f, &format!("\n{}", passthrough(NOW)));
    let got = drain(sink.path(), &mut cursors, NOW);
    assert_eq!(got.calls.len(), 1, "{got:?}");
}

#[test]
fn recorded_rows_become_forge_calls_points_with_the_agent_label() {
    use crate::observability::ops::capture::capture;
    let sink = tempfile::tempdir().unwrap();
    let f = file(sink.path(), HOUR);
    let mut cursors = primed(sink.path());
    append(&f, &passthrough(NOW));
    append(&f, &served(NOW));
    let (points, _) = capture(|| {
        let _ = forge_calls::drain_points();
        assert_eq!(record(drain(sink.path(), &mut cursors, NOW)), 2);
        forge_calls::drain_points()
    });
    let by_caller = |c: &str| points.iter().find(|p| p.labels["caller"] == c).unwrap();
    let pr = by_caller("agent.gh.pr");
    assert_eq!((pr.labels["agent"].as_str(), pr.labels["outcome"].as_str()), ("builder", "ok"));
    let view = by_caller("agent_gh_front");
    assert_eq!(view.labels["agent"], "judge");
    assert_eq!(view.labels["outcome"], "not_modified");
}
