//! Tests for `loom-daemon guide work-plan-receipt|work-plan-splice`
//! (issue #15219).

use std::fs;

use super::*;

fn wt() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn read(root: &std::path::Path, rel: &str) -> String {
    fs::read_to_string(root.join(rel)).unwrap()
}

const MARKED_PLAN: &str = "head\n\
                           <!-- guide:plan-body:start -->\n\
                           OLD\n\
                           <!-- guide:plan-body:end -->\n\
                           tail\n";

#[test]
fn record_read_round_trip() {
    let dir = wt();
    record_receipt(dir.path(), "written", "6c47a827e98f").unwrap();
    assert_eq!(read(dir.path(), RECEIPT_FILE), "receipt=written\nrender_hash=6c47a827e98f\n");
    assert_eq!(read_field(dir.path(), "receipt").unwrap().as_deref(), Some("written"));
    assert_eq!(read_field(dir.path(), "render_hash").unwrap().as_deref(), Some("6c47a827e98f"));
}

#[test]
fn record_allows_parens_spaces_and_comparators() {
    // The debounced classification carries spaces, parens and `<` — it must
    // survive the file byte-for-byte (issue #14204).
    let dir = wt();
    let outcome = "debounced (elapsed 300s < window 3600s)";
    record_receipt(dir.path(), outcome, "abc").unwrap();
    assert_eq!(read_field(dir.path(), "receipt").unwrap().as_deref(), Some(outcome));
}

#[test]
fn record_replaces_previous_content() {
    let dir = wt();
    record_receipt(dir.path(), "current", "aaa").unwrap();
    record_receipt(dir.path(), "write-failed", "").unwrap();
    assert_eq!(read(dir.path(), RECEIPT_FILE), "receipt=write-failed\nrender_hash=\n");
    assert_eq!(read_field(dir.path(), "render_hash").unwrap().as_deref(), Some(""));
}

#[test]
fn reset_removes_and_is_idempotent() {
    let dir = wt();
    record_receipt(dir.path(), "written", "h").unwrap();
    reset_receipt(dir.path()).unwrap();
    assert!(!dir.path().join(RECEIPT_FILE).exists());
    // A second reset (no file) is success, like `rm -f`.
    reset_receipt(dir.path()).unwrap();
}

#[test]
fn read_missing_file_or_key_prints_none() {
    let dir = wt();
    assert!(read_field(dir.path(), "receipt").unwrap().is_none());
    record_receipt(dir.path(), "written", "h").unwrap();
    assert!(read_field(dir.path(), "nosuchkey").unwrap().is_none());
}

#[test]
fn read_takes_the_first_match_only() {
    let dir = wt();
    fs::write(dir.path().join(RECEIPT_FILE), "receipt=first\nreceipt=second\n").unwrap();
    assert_eq!(read_field(dir.path(), "receipt").unwrap().as_deref(), Some("first"));
}

#[test]
fn splice_replaces_only_the_marker_region() {
    let dir = wt();
    fs::write(dir.path().join(PLAN_FILE), MARKED_PLAN).unwrap();
    splice_file(dir.path(), "NEW1\nNEW2").unwrap();
    assert_eq!(
        read(dir.path(), PLAN_FILE),
        "head\n\
         <!-- guide:plan-body:start -->\n\
         NEW1\nNEW2\n\
         <!-- guide:plan-body:end -->\n\
         tail\n"
    );
}

#[test]
fn splice_refuses_when_either_marker_is_missing() {
    let dir = wt();
    for plan in [
        "no markers at all\n",
        "start only\n<!-- guide:plan-body:start -->\n",
    ] {
        fs::write(dir.path().join(PLAN_FILE), plan).unwrap();
        assert!(splice_file(dir.path(), "X").is_err(), "refused for {plan:?}");
        assert_eq!(read(dir.path(), PLAN_FILE), plan, "file untouched");
    }
}

#[test]
fn splice_refuses_when_the_plan_is_absent() {
    let dir = wt();
    assert!(splice_file(dir.path(), "X").is_err());
    assert!(!dir.path().join(PLAN_FILE).exists());
}

#[test]
fn splice_leaves_no_scratch_behind() {
    // #13923: no per-tick scratch dir may outlive the splice, on success or
    // on refusal. The verb stages inside the OS temp machinery and renames,
    // so nothing may remain either in the docs worktree or under TMPDIR in
    // the guide-plan.* shape the old shell scratch used.
    let tmp = std::env::temp_dir();
    let before: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("guide-plan."))
        .collect();
    let dir = wt();
    fs::write(dir.path().join(PLAN_FILE), MARKED_PLAN).unwrap();
    splice_file(dir.path(), "NEW").unwrap();
    assert!(splice_file(dir.path(), "NEW2").is_ok());
    let after: Vec<_> = fs::read_dir(&tmp)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("guide-plan."))
        .collect();
    assert_eq!(before.len(), after.len(), "no guide-plan.* scratch left behind");
    let names: Vec<String> = fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec!["WORK_PLAN.md".to_string()], "worktree holds only the plan");
}

#[test]
fn splice_empty_body_inserts_one_blank_line() {
    // `printf '%s\n' ""` wrote a single newline: an empty region is one
    // empty line, not zero lines.
    let out = splice_text(MARKED_PLAN, "").unwrap();
    assert_eq!(
        out,
        "head\n\
         <!-- guide:plan-body:start -->\n\
         \n\
         <!-- guide:plan-body:end -->\n\
         tail\n"
    );
}

#[test]
fn splice_normalizes_a_missing_final_newline_like_the_awk() {
    let plan = "head\n<!-- guide:plan-body:start -->\nOLD\n<!-- guide:plan-body:end -->";
    let out = splice_text(plan, "NEW").unwrap();
    assert_eq!(out, "head\n<!-- guide:plan-body:start -->\nNEW\n<!-- guide:plan-body:end -->\n");
}

#[test]
fn splice_body_without_trailing_newline_is_terminated() {
    let out = splice_text(MARKED_PLAN, "A\nB").unwrap();
    assert!(out.contains("start -->\nA\nB\n<!-- guide:plan-body:end -->"));
}

#[test]
fn splice_replaces_the_whole_single_marker_region() {
    // Multi-line old content between the single marker pair is replaced
    // wholesale, not concatenated with the new body.
    let plan = "a\n<!-- guide:plan-body:start -->\nOLD\nmiddle\n<!-- guide:plan-body:end -->\nb\n";
    let out = splice_text(plan, "NEW").unwrap();
    assert_eq!(out, "a\n<!-- guide:plan-body:start -->\nNEW\n<!-- guide:plan-body:end -->\nb\n");
}

#[test]
fn splice_refuses_a_duplicate_start_marker() {
    // A second start marker means a corrupt plan: refuse rather than
    // splice into either region. Mid-line markers count too.
    for plan in [
        "a\n<!-- guide:plan-body:start -->\nX\n<!-- guide:plan-body:start -->\nY\n<!-- guide:plan-body:end -->\nb\n",
        "a <!-- guide:plan-body:start -->\n<!-- guide:plan-body:start -->\n<!-- guide:plan-body:end -->\nb\n",
    ] {
        let err = splice_text(plan, "NEW").unwrap_err().to_string();
        assert!(err.contains(BODY_START), "names the start marker: {err}");
        assert!(err.contains("refusing to write"), "{err}");
    }
}

#[test]
fn splice_refuses_a_duplicate_end_marker() {
    let plan = "a\n<!-- guide:plan-body:start -->\nOLD\n<!-- guide:plan-body:end -->\nb\n<!-- guide:plan-body:end -->\nc\n";
    let err = splice_text(plan, "NEW").unwrap_err().to_string();
    assert!(err.contains(BODY_END), "names the end marker: {err}");
    assert!(err.contains("refusing to write"), "{err}");
}

#[test]
fn splice_file_leaves_a_duplicate_marker_plan_untouched() {
    let dir = wt();
    let plan = "a\n<!-- guide:plan-body:start -->\nX\n<!-- guide:plan-body:start -->\nY\n<!-- guide:plan-body:end -->\nb\n";
    fs::write(dir.path().join(PLAN_FILE), plan).unwrap();
    assert!(splice_file(dir.path(), "NEW").is_err());
    assert_eq!(read(dir.path(), PLAN_FILE), plan, "file untouched");
}

#[test]
fn receipt_file_name_is_the_documented_one() {
    assert_eq!(RECEIPT_FILE, ".loom-work-plan-receipt");
    assert_eq!(BODY_START, "<!-- guide:plan-body:start -->");
    assert_eq!(BODY_END, "<!-- guide:plan-body:end -->");
}
