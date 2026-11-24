#![allow(clippy::unwrap_used)]
//! #14902: the merge-observation pass, against an in-memory forge.
//!
//! The convergence fixture at the bottom is the issue's disposable fixture:
//! one scenario driven twice — once as a **script merge** (the retired
//! `merge-pr.sh` passes replayed through [`bookkeeping::run`] with the
//! shell's own pre-merge facts), once as a **direct forge merge** (only the
//! observation tick) — over identically-seeded state, on a workspace that
//! has no merge queue at all (the direct-mode shape: discovery must not
//! consult the merge mode). After one successful tick both give the same
//! eligible issue state, labels and partial-increment comments, and the
//! pinned [`MergeRecord`] shows the recorded identities.

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;

use crate::merge_pr::post_merge_bookkeeping as bookkeeping;

const MERGED_AT: &str = "2026-11-21T10:00:00Z";
const AFTER_MERGE: &str = "2026-11-21T11:30:00Z";
const BEFORE_MERGE: &str = "2026-11-20T08:00:00Z";

#[derive(Default, Clone)]
struct FakeIssue {
    state: String,
    labels: Vec<String>,
    body: String,
    is_pr: bool,
    comments: Vec<String>,
}

impl FakeIssue {
    fn open(labels: &[&str]) -> Self {
        Self {
            state: "open".into(),
            labels: labels.iter().map(|s| (*s).to_string()).collect(),
            ..Self::default()
        }
    }
    fn closed(labels: &[&str]) -> Self {
        Self {
            state: "closed".into(),
            ..Self::open(labels)
        }
    }
    fn json(&self) -> String {
        let mut v = serde_json::json!({
            "state": self.state,
            "labels": self.labels.iter().map(|l| serde_json::json!({"name": l})).collect::<Vec<_>>(),
            "body": self.body,
        });
        if self.is_pr {
            v["pull_request"] = serde_json::json!({});
        }
        v.to_string()
    }
}

#[derive(Default)]
struct FakeForge {
    issues: RefCell<HashMap<String, FakeIssue>>,
    branch: String,
    body: String,
    commits: String,
    close_targets: RefCell<Option<String>>,
    merged: Vec<MergedPull>,
    timeline: RefCell<HashMap<String, String>>,
    cbm: HashMap<String, String>,
    classify: HashMap<String, (i32, String)>,
    classify_none: Vec<String>,
    fail_helpers: Vec<&'static str>,
    calls: RefCell<Vec<String>>,
    classify_calls: RefCell<usize>,
}

impl FakeForge {
    fn issue(&self, n: &str) -> FakeIssue {
        self.issues.borrow().get(n).cloned().unwrap_or_default()
    }
    fn labels(&self, n: &str) -> Vec<String> {
        self.issue(n).labels
    }
    fn state(&self, n: &str) -> String {
        self.issue(n).state
    }
    fn headings(&self, n: &str) -> Vec<String> {
        self.issue(n)
            .comments
            .iter()
            .filter_map(|c| c.lines().next().map(|l| l.trim().to_string()))
            .collect()
    }
    fn comment_count(&self, n: &str) -> usize {
        self.issue(n).comments.len()
    }
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
    fn timeline_at(&self, n: &str, at: &str) {
        self.timeline.borrow_mut().insert(
            n.to_string(),
            serde_json::json!([{
                "event": "labeled",
                "label": {"name": "loom:building"},
                "created_at": at,
                "actor": {"login": "someone"},
            }])
            .to_string(),
        );
    }
    fn reopened_at(&self, n: &str, at: &str) {
        self.timeline.borrow_mut().insert(
            n.to_string(),
            serde_json::json!([{
                "event": "reopened",
                "created_at": at,
            }])
            .to_string(),
        );
    }
}

// Free helpers so the Forge and bookkeeping::Host impls can share bodies
// without a same-named-method recursion.
fn fake_read_issue(f: &FakeForge, _repo: &str, issue: &str) -> String {
    f.issues
        .borrow()
        .get(issue)
        .map_or_else(|| "{}".to_string(), FakeIssue::json)
}
fn fake_issue_kind(f: &FakeForge, _repo: &str, issue: &str) -> String {
    let i = f.issue(issue);
    if i.is_pr {
        "pr".into()
    } else {
        i.state
    }
}
fn fake_helper(f: &FakeForge, func: &str, args: &[&str]) -> bool {
    f.calls
        .borrow_mut()
        .push(format!("{func} {}", args.join(" ")));
    if f.fail_helpers.contains(&func) {
        return false;
    }
    let mut issues = f.issues.borrow_mut();
    match func {
        "forge_gh_reopen_issue_rl_safe" => {
            if let Some(i) = issues.get_mut(args[1]) {
                i.state = "open".into();
            }
        }
        "forge_gh_swap_label_rl_safe" => {
            if let Some(i) = issues.get_mut(args[1]) {
                i.labels.retain(|l| l != args[2]);
                i.labels.push(args[3].to_string());
            }
        }
        "forge_gh_remove_label_rl_safe" => {
            if let Some(i) = issues.get_mut(args[1]) {
                i.labels.retain(|l| l != args[2]);
            }
        }
        "forge_gh_comment_rl_safe" => {
            if let Some(i) = issues.get_mut(args[1]) {
                i.comments.push(args[2].to_string());
            }
        }
        _ => {}
    }
    true
}
fn fake_close_issue(f: &FakeForge, _repo: &str, issue: &str, comment: &str) -> bool {
    f.calls.borrow_mut().push(format!("close {issue}"));
    if let Some(i) = f.issues.borrow_mut().get_mut(issue) {
        i.state = "closed".into();
        i.comments.push(comment.to_string());
    }
    true
}

impl Forge for FakeForge {
    fn merged_pulls(&self) -> Option<Vec<MergedPull>> {
        Some(self.merged.clone())
    }
    fn nwo(&self) -> Option<String> {
        Some("o/r".into())
    }
    fn read_issue(&self, repo: &str, issue: &str) -> String {
        fake_read_issue(self, repo, issue)
    }
    fn commit_messages(&self, _repo: &str, _pr: &str) -> String {
        self.commits.clone()
    }
    fn issue_kind(&self, repo: &str, issue: &str) -> String {
        fake_issue_kind(self, repo, issue)
    }
    fn close_targets(&self, _repo: &str, _pr: &str) -> Option<String> {
        self.close_targets.borrow().clone()
    }
    fn issue_comments(&self, _repo: &str, issue: &str) -> Vec<String> {
        self.issue(issue).comments
    }
    fn label_events(&self, _repo: &str, issue: &str) -> Option<String> {
        self.timeline.borrow().get(issue).cloned()
    }
    fn merge_commit_message(&self, _repo: &str, _sha: &str) -> String {
        "Merge pull request #9 from feature/issue-42".into()
    }
    fn closed_by_merge_json(&self, _repo: &str, issue: u64, _pr: u64) -> String {
        self.cbm
            .get(&issue.to_string())
            .cloned()
            .unwrap_or_default()
    }
    fn trusted_evidence(&self, _repo: &str, _number: u64, _with_body: bool) -> Vec<String> {
        Vec::new()
    }
    fn classify_ac(&self, _body: &str, _evidence: &str, head: &str) -> Option<(i32, String)> {
        *self.classify_calls.borrow_mut() += 1;
        if self.classify_none.iter().any(|h| h == head) {
            return None;
        }
        Some(
            self.classify
                .get(head)
                .cloned()
                .unwrap_or((10, String::new())),
        )
    }
    fn helper(&self, func: &str, args: &[&str]) -> bool {
        fake_helper(self, func, args)
    }
    fn close_issue(&self, repo: &str, issue: &str, comment: &str) -> bool {
        fake_close_issue(self, repo, issue, comment)
    }
    fn add_label(&self, _repo: &str, issue: &str, label: &str) -> bool {
        self.calls
            .borrow_mut()
            .push(format!("add_label {issue} {label}"));
        if let Some(i) = self.issues.borrow_mut().get_mut(issue) {
            if !i.labels.iter().any(|l| l == label) {
                i.labels.push(label.to_string());
            }
        }
        true
    }
}

/// The retired shell side: the three `merge-pr.sh` passes with the shell's
/// own pre-merge facts, no observation, no dedupe — exactly what
/// `_post_merge_bookkeeping` did.
impl bookkeeping::Host for FakeForge {
    fn read_issue(&self, repo: &str, issue: &str) -> String {
        fake_read_issue(self, repo, issue)
    }
    fn commit_messages(&self, repo: &str, pr: &str) -> String {
        Forge::commit_messages(self, repo, pr)
    }
    fn issue_kind(&self, repo: &str, issue: &str) -> String {
        fake_issue_kind(self, repo, issue)
    }
    fn helper(&self, func: &str, args: &[&str]) -> bool {
        fake_helper(self, func, args)
    }
    fn close_issue(&self, repo: &str, issue: &str, comment: &str) -> bool {
        fake_close_issue(self, repo, issue, comment)
    }
    fn label_events(&self, repo: &str, issue: &str) -> Option<String> {
        Forge::label_events(self, repo, issue)
    }
    fn timestamp(&self) -> String {
        "T".into()
    }
}

/// A `closed-by-merge` GraphQL answer for `issue`.
fn cbm(issue: u64, state: &str, closed_at: &str, closer: serde_json::Value) -> String {
    serde_json::json!({"data":{"repository":{
        "pullRequest":{"number":9,"mergedAt":MERGED_AT,"mergeCommit":{"oid":"m1"},
                       "commits":{"nodes":[{"commit":{"oid":"c1"}}]}},
        "issue":{"number":issue,"state":state,"closedAt":closed_at,
                 "timelineItems":{"nodes":[{"closer":closer}]}}
    }}})
    .to_string()
}

fn this_merge(issue: u64) -> String {
    cbm(
        issue,
        "closed",
        "2026-11-21T10:00:01Z",
        serde_json::json!({"__typename":"PullRequest","number":9}),
    )
}
fn closed_before_merge(issue: u64) -> String {
    cbm(
        issue,
        "closed",
        "2026-11-21T09:00:00Z",
        serde_json::json!({"__typename":"PullRequest","number":8}),
    )
}

fn merged_row() -> MergedPull {
    MergedPull {
        number: 9,
        head: "h1".into(),
        merge_commit: "m1".into(),
        merged_at: MERGED_AT.into(),
        branch: "feature/issue-42".into(),
        body: String::new(),
    }
}

fn run(f: &FakeForge, store: &RecordStore, gate: bool) -> Stats {
    run_pass(f, store, gate, || false)
}

// ---------------------------------------------------------------------------
// pure helpers
// ---------------------------------------------------------------------------

#[test]
fn parse_merged_pulls_keeps_only_pinnable_merged_rows() {
    let raw = serde_json::json!([
        {"number": 9, "merged_at": MERGED_AT, "merge_commit_sha": "m1",
         "head": {"sha": "h1", "ref": "feature/issue-42"}, "body": "Part of #5"},
        {"number": 10, "merged_at": null, "merge_commit_sha": null,
         "head": {"sha": "h2", "ref": "x"}, "body": ""},
        {"number": 11, "merged_at": MERGED_AT, "merge_commit_sha": "",
         "head": {"sha": "h3", "ref": "x"}, "body": ""},
        {"number": 12, "merged_at": MERGED_AT, "merge_commit_sha": "m2",
         "head": {"sha": "", "ref": "x"}, "body": ""}
    ])
    .to_string();
    let rows = parse_merged_pulls(&raw);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].number, 9);
    assert_eq!(rows[0].head, "h1");
    assert_eq!(rows[0].merge_commit, "m1");
    assert_eq!(rows[0].branch, "feature/issue-42");
    assert_eq!(rows[0].body, "Part of #5");
}

#[test]
fn claim_predates_merge_answer_the_three_ways() {
    let events = |at: &str| {
        serde_json::json!([{"event": "labeled", "label": {"name": "loom:building"},
            "created_at": at, "actor": {"login": "a"}},
            {"event": "unlabeled", "label": {"name": "loom:building"}, "created_at": at}])
        .to_string()
    };
    assert_eq!(claim_predates_merge(&events(BEFORE_MERGE), MERGED_AT), Some(true));
    assert_eq!(claim_predates_merge(&events(AFTER_MERGE), MERGED_AT), Some(false));
    assert_eq!(claim_predates_merge("[]", MERGED_AT), None);
    assert_eq!(claim_predates_merge(&events(BEFORE_MERGE), "not a time"), None);
    let unlabeled_only = serde_json::json!([{"event": "unlabeled", "label": {"name": "x"},
        "created_at": BEFORE_MERGE}])
    .to_string();
    assert_eq!(claim_predates_merge(&unlabeled_only, MERGED_AT), None);
}

#[test]
fn reopened_after_merge_answers_the_four_ways() {
    let events = |at: &str| {
        serde_json::json!([{"event": "labeled", "label": {"name": "x"}, "created_at": at},
            {"event": "reopened", "created_at": at}])
        .to_string()
    };
    // Absent (a readable timeline with no reopen) and before: still close.
    assert_eq!(reopened_after_merge("[]", MERGED_AT), Some(false));
    assert_eq!(reopened_after_merge(&events(BEFORE_MERGE), MERGED_AT), Some(false));
    // After: a deliberate reopen the replay must not undo.
    assert_eq!(reopened_after_merge(&events(AFTER_MERGE), MERGED_AT), Some(true));
    // The newest reopen decides, whatever the page order.
    let both = serde_json::json!([
        {"event": "reopened", "created_at": AFTER_MERGE},
        {"event": "reopened", "created_at": BEFORE_MERGE}])
    .to_string();
    assert_eq!(reopened_after_merge(&both, MERGED_AT), Some(true));
    // Malformed: the unreadable read (an empty string), broken JSON, a
    // broken merged_at — each unknown, never a guess.
    assert_eq!(reopened_after_merge("", MERGED_AT), None);
    assert_eq!(reopened_after_merge("not json", MERGED_AT), None);
    assert_eq!(reopened_after_merge(&events(AFTER_MERGE), "not a time"), None);
}

#[test]
fn incomplete_records_expire_at_the_ttl_boundary() {
    let now = "2026-11-28T10:00:00Z";
    // Exactly the 7-day TTL old: the record gets its whole bound.
    assert!(!incomplete_record_expired("2026-11-21T10:00:00Z", now));
    // One second past it: expired.
    assert!(incomplete_record_expired("2026-11-21T09:59:59Z", now));
    // Younger: still retried.
    assert!(!incomplete_record_expired("2026-11-27T23:00:00Z", now));
    // A corrupt stamp cannot age out — reclaim it.
    assert!(incomplete_record_expired("not a time", now));
    // A clock that cannot be read keeps the record.
    assert!(!incomplete_record_expired("2026-11-21T10:00:00Z", "not a time"));
}

#[test]
fn comment_dedupe_matches_headings_and_markers() {
    let partial = "## Partial Increment Merged\n\nPR #9 merged with a non-closing reference";
    assert!(comment_already_posted(partial, &[partial.into()]));
    assert!(comment_already_posted(
        partial,
        &["prefix\n## Partial Increment Merged (remainder held)\n\nPR #9 merged".into()]
    ));
    // A different PR's notice on the same issue is not a duplicate.
    assert!(!comment_already_posted(
        partial,
        &["## Partial Increment Merged\n\nPR #10 merged".into()]
    ));
    let premature = "## Premature Auto-Close Reverted\n\nPR #9 referenced";
    assert!(comment_already_posted(
        premature,
        &["## Premature Auto-Close Reverted\n\nPR #9 referenced".into()]
    ));
    assert!(!comment_already_posted(premature, &["## Other\nPR #9".into()]));
    let hold = "<!-- champion:ac-hold pr=9 sha=h1 -->\nbody";
    assert!(comment_already_posted(hold, &[format!("x\n{hold}\ny")]));
    assert!(!comment_already_posted(
        hold,
        &["<!-- champion:ac-hold pr=9 sha=OTHER -->".into()]
    ));
    assert!(!comment_already_posted("plain comment", &[partial.into()]));
}

#[test]
fn ac_gate_is_on_unless_explicitly_false() {
    assert!(ac_gate_enabled_from(&serde_json::json!({})));
    assert!(!ac_gate_enabled_from(
        &serde_json::json!({"champion": {"outOfBandAcGate": false}})
    ));
    assert!(ac_gate_enabled_from(
        &serde_json::json!({"champion": {"outOfBandAcGate": true}})
    ));
    // The string "false" is not the boolean false (#12703).
    assert!(ac_gate_enabled_from(
        &serde_json::json!({"champion": {"outOfBandAcGate": "false"}})
    ));
}

#[test]
fn records_round_trip_and_corruption_reads_as_unobserved() {
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    let mut rec = MergeRecord {
        repo: "o/r".into(),
        pr: 9,
        head: "h1".into(),
        merge_commit: "m1".into(),
        merged_at: MERGED_AT.into(),
        branch: "feature/issue-42".into(),
        body: "Part of #5".into(),
        observed_at: MERGED_AT.into(),
        done: vec![PARTIAL_RESET.into()],
    };
    assert!(!rec.complete());
    store.store(&rec).unwrap();
    assert_eq!(store.load(9).as_ref(), Some(&rec));
    rec.mark(CLOSE_CHECK);
    rec.mark(BRANCH_CLOSE);
    rec.mark(STRIP);
    assert!(rec.complete());
    assert_eq!(rec.done, ["partial-reset", "close-check", "branch-close", "strip"]);
    store.store(&rec).unwrap();
    assert!(store.load(9).unwrap().complete());

    std::fs::write(dir.path().join(".loom/state/merge-bookkeeping/pr-10.json"), "{").unwrap();
    assert!(store.load(10).is_none());
}

// ---------------------------------------------------------------------------
// the pass against the fake
// ---------------------------------------------------------------------------

#[test]
fn unreadable_close_targets_defer_the_whole_record() {
    let mut f = FakeForge::default();
    f.issues
        .borrow_mut()
        .insert("5".into(), FakeIssue::open(&["loom:building"]));
    f.merged = vec![merged_row()];
    // close_targets stays None: the read itself failed.
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    let stats = run(&f, &store, false);
    assert_eq!(stats.merges_seen, 1);
    assert_eq!(stats.records_completed, 0);
    // The identity is pinned but no pass ran: a missing fact defers, it
    // never reads as "no targets".
    let rec = store.load(9).unwrap();
    assert!(rec.done.is_empty());
    assert_eq!(f.state("5"), "open");
    assert_eq!(f.labels("5"), ["loom:building"]);
    assert!(f.calls().is_empty());
}

#[test]
fn suppression_stops_between_merges() {
    let mut f = FakeForge::default();
    let mut row = merged_row();
    row.number = 9;
    let row2 = MergedPull {
        number: 10,
        body: row.body.clone(),
        ..row.clone()
    };
    f.merged = vec![row, row2];
    f.close_targets.borrow_mut().replace(String::new());
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    let stats = run_pass(&f, &store, false, || true);
    assert_eq!(stats.merges_seen, 2);
    assert_eq!(stats.records_completed, 0);
    assert!(store.load(9).is_none());
    assert!(store.load(10).is_none());
}

fn partial_fixture() -> FakeForge {
    let mut f = FakeForge::default();
    f.branch = "feature/issue-42".into();
    f.body = "Slice one.\n\nPart of #5\n".into();
    f.merged = vec![MergedPull {
        body: f.body.clone(),
        ..merged_row()
    }];
    f.close_targets.borrow_mut().replace(String::new());
    f.issues
        .borrow_mut()
        .insert("5".into(), FakeIssue::open(&["loom:building"]));
    f.timeline_at("5", BEFORE_MERGE);
    f
}

#[test]
fn an_open_partial_increment_is_swapped_and_commented_once() {
    let f = partial_fixture();
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, false);
    assert_eq!(f.state("5"), "open");
    assert_eq!(f.labels("5"), ["loom:issue"]);
    assert_eq!(f.comment_count("5"), 1);
    assert!(f.headings("5") == ["## Partial Increment Merged"]);
    let rec = store.load(9).unwrap();
    assert!(rec.complete());
    // The recorded identities (#14902): repository, PR, landed head, merge
    // commit — everything a retry refers to.
    assert_eq!(
        (rec.repo.as_str(), rec.pr, rec.head.as_str(), rec.merge_commit.as_str()),
        ("o/r", 9, "h1", "m1")
    );
    // Replay: the completed record is never revisited.
    let (comments, labels, state) = (f.comment_count("5"), f.labels("5"), f.state("5"));
    let calls = f.calls().len();
    run(&f, &store, false);
    assert_eq!(f.comment_count("5"), comments);
    assert_eq!(f.labels("5"), labels);
    assert_eq!(f.state("5"), state);
    assert!(f.calls().len() >= calls);
    assert!(!f.calls()[calls..].iter().any(|c| c.contains("5")));
}

#[test]
fn a_newer_builder_claim_survives_the_observed_pass() {
    let f = partial_fixture();
    f.timeline_at("5", AFTER_MERGE);
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, false);
    assert_eq!(f.state("5"), "open");
    assert_eq!(f.labels("5"), ["loom:building"]);
    assert_eq!(f.comment_count("5"), 0);
    assert!(store.load(9).unwrap().complete());
}

#[test]
fn an_unreadable_claim_timeline_fails_closed_and_leaves_the_pass_open() {
    let f = partial_fixture();
    f.timeline.borrow_mut().remove("5");
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, false);
    assert_eq!(f.labels("5"), ["loom:building"]);
    assert_eq!(f.comment_count("5"), 0);
    // The pass is not marked done: a transient read failure retries rather
    // than losing the eligible swap. Make the timeline readable and the
    // next tick completes the record and the swap.
    let rec = store.load(9).unwrap();
    assert!(!rec.complete());
    f.timeline_at("5", BEFORE_MERGE);
    run(&f, &store, false);
    assert_eq!(f.labels("5"), ["loom:issue"]);
    assert_eq!(f.comment_count("5"), 1);
    assert!(store.load(9).unwrap().complete());
}

#[test]
fn a_preventive_reopens_only_what_this_merge_closed() {
    // #6: closed by this merge (attribution YES) with a stray closing
    // reference — the #4569 shape, re-derived post-merge.
    let mut f = partial_fixture();
    f.issues.borrow_mut().insert(
        "6".into(),
        FakeIssue {
            body: String::new(),
            ..FakeIssue::closed(&["loom:building"])
        },
    );
    f.merged[0].body = "Part of #5\nPart of #6\n\n…accidentally said close #6…\n".into();
    f.cbm.insert("6".into(), this_merge(6));
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, false);
    assert_eq!(f.state("6"), "open");
    assert_eq!(f.labels("6"), ["loom:issue"]);
    let h = f.headings("6");
    assert!(h.contains(&"## Premature Auto-Close Reverted".to_string()));
    assert!(h.contains(&"## Partial Increment Merged".to_string()));

    // #7: closed BEFORE the merge by another PR — an unrelated historical
    // close survives the replay of the same merge.
    let mut f2 = partial_fixture();
    f2.issues
        .borrow_mut()
        .insert("7".into(), FakeIssue::closed(&["loom:building"]));
    f2.merged[0].body = "Part of #5\nPart of #7\n".into();
    f2.cbm.insert("7".into(), closed_before_merge(7));
    let dir2 = tempfile::tempdir().unwrap();
    run(&f2, &RecordStore::for_root(dir2.path()), false);
    assert_eq!(f2.state("7"), "closed");
    assert_eq!(f2.labels("7"), ["loom:building"]);
    assert_eq!(f2.comment_count("7"), 0);
}

#[test]
fn missing_attribution_skips_the_dependent_reopen() {
    let mut f = partial_fixture();
    f.issues
        .borrow_mut()
        .insert("6".into(), FakeIssue::closed(&["loom:building"]));
    f.merged[0].body = "Part of #5\nPart of #6\n\n…close #6…\n".into();
    // No `closed_by_merge` answer at all: the pre-merge facts cannot be
    // established, so the reopen does not happen.
    let dir = tempfile::tempdir().unwrap();
    run(&f, &RecordStore::for_root(dir.path()), false);
    assert_eq!(f.state("6"), "closed");
    assert_eq!(f.labels("6"), ["loom:building"]);
    assert_eq!(f.comment_count("6"), 0);
}

#[test]
fn a_held_remainder_swaps_to_the_operator_label() {
    let mut f = partial_fixture();
    let mut issue = FakeIssue::open(&["loom:building"]);
    issue.body = "<!-- loom:remainder-hold reason=\"scope\" -->".into();
    f.issues.borrow_mut().insert("10".into(), issue);
    f.timeline_at("10", BEFORE_MERGE);
    f.merged[0].body = "Part of #5\nPart of #10\n".into();
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, false);
    assert_eq!(f.state("10"), "open");
    assert_eq!(f.labels("10"), ["loom:operator"]);
    assert!(f.headings("10") == ["## Partial Increment Merged (remainder held)"]);
}

// ---------------------------------------------------------------------------
// the close check (Champion Step 4)
// ---------------------------------------------------------------------------

#[allow(clippy::field_reassign_with_default)]
fn close_target_fixture() -> (FakeForge, tempfile::TempDir) {
    let mut f = FakeForge::default();
    f.merged = vec![MergedPull {
        body: "Closes #3\n".into(),
        ..merged_row()
    }];
    f.close_targets.borrow_mut().replace("3\n".to_string());
    f.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::open(&["loom:building"]));
    // A readable timeline with no post-merge reopen: the close gate's
    // `Some(false)` shape (#15145).
    f.timeline_at("3", BEFORE_MERGE);
    let dir = tempfile::tempdir().unwrap();
    (f, dir)
}

#[test]
fn an_open_linked_issue_is_closed_by_the_observation() {
    let (f, dir) = close_target_fixture();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "closed");
    assert_eq!(f.comment_count("3"), 1);
    assert!(f.calls().iter().any(|c| c == "close 3"));
}

#[test]
fn a_target_reopened_after_the_merge_is_not_closed_again() {
    let (f, dir) = close_target_fixture();
    f.reopened_at("3", AFTER_MERGE);
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "open");
    assert!(!f.calls().iter().any(|c| c == "close 3"));
    // A verified reopen is a decision, not a failure (#15145): the pass
    // completed having deliberately closed nothing, so it does not loop.
    assert!(store.load(9).unwrap().complete());
}

#[test]
fn a_target_reopened_only_before_the_merge_still_closes() {
    let (f, dir) = close_target_fixture();
    f.reopened_at("3", BEFORE_MERGE);
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "closed");
    assert!(store.load(9).unwrap().complete());
}

#[test]
fn an_unreadable_reopen_timeline_leaves_the_close_check_unmarked() {
    let (f, dir) = close_target_fixture();
    f.timeline.borrow_mut().remove("3");
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "open");
    assert!(!f.calls().iter().any(|c| c == "close 3"));
    assert!(!store.load(9).unwrap().complete());
    // Readable again: a later tick closes it and completes the record.
    f.reopened_at("3", BEFORE_MERGE);
    run(&f, &store, true);
    assert_eq!(f.state("3"), "closed");
    assert!(store.load(9).unwrap().complete());
}

#[test]
fn an_already_closed_linked_issue_is_confirmed_not_recommented() {
    let (f, dir) = close_target_fixture();
    f.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::closed(&["loom:building"]));
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "closed");
    assert_eq!(f.comment_count("3"), 0);
    assert!(!f.calls().iter().any(|c| c == "close 3"));
    // The strip pass (which runs after the close check) took the stale claim.
    assert_eq!(f.labels("3"), [] as [String; 0]);
}

#[test]
fn an_unreadable_state_is_never_closed_and_leaves_the_pass_open() {
    let (f, dir) = close_target_fixture();
    f.issues.borrow_mut().remove("3");
    f.close_targets.borrow_mut().replace("3\n".to_string());
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert!(!f.calls().iter().any(|c| c == "close 3"));
    assert!(!store.load(9).unwrap().complete());
    // The issue appearing (a readable state) lets a later tick close it.
    f.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::open(&[]));
    run(&f, &store, true);
    assert_eq!(f.state("3"), "closed");
    assert!(store.load(9).unwrap().complete());
}

#[test]
fn unverified_out_of_band_ac_holds_the_issue_open() {
    let (mut f, dir) = close_target_fixture();
    f.classify
        .insert("h1".into(), (12, "next run\tConfirm on the next real run".into()));
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    // Held open, notice posted once, operator label applied.
    assert_eq!(f.state("3"), "open");
    assert_eq!(f.comment_count("3"), 1);
    assert!(f.issue("3").comments[0].starts_with("<!-- champion:ac-hold pr=9 sha=h1 -->"));
    assert!(f.labels("3").contains(&"loom:operator".to_string()));
    // Posting the same hold again would duplicate nothing.
    assert!(comment_already_posted(
        &f.issue("3").comments[0],
        &[f.issue("3").comments[0].clone()]
    ));
}

#[test]
fn a_marker_for_a_different_head_holds_like_a_missing_one() {
    let (mut f, dir) = close_target_fixture();
    f.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::closed(&[]));
    f.classify.insert("h1".into(), (13, String::new()));
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "open");
    assert!(f.labels("3").contains(&"loom:operator".to_string()));
}

#[test]
fn a_classifier_that_cannot_run_fails_closed() {
    let (mut f, dir) = close_target_fixture();
    f.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::closed(&[]));
    f.classify_none.push("h1".into());
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    // Held (reopened), not closed: "could not run" is not "criteria met".
    assert_eq!(f.state("3"), "open");
    assert!(f.labels("3").contains(&"loom:operator".to_string()));
    assert!(f.issue("3").comments[0].starts_with("<!-- champion:ac-hold pr=9 sha=h1 -->"));
    assert!(f.issue("3").comments[0].contains("could not complete"));
}

#[test]
fn the_ac_gate_off_never_consults_the_classifier() {
    let (f, dir) = close_target_fixture();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, false);
    assert_eq!(*f.classify_calls.borrow(), 0);
    assert_eq!(f.state("3"), "closed");
}

#[test]
fn negated_only_references_reopen_what_this_merge_closed() {
    let (mut f, dir) = close_target_fixture();
    f.merged[0].body = "does not fix #3\n".into();
    f.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::closed(&[]));
    f.cbm.insert("3".into(), this_merge(3));
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "open");
    assert!(f
        .issue("3")
        .comments
        .iter()
        .any(|c| c.contains("negated (#1057)")));

    // A historical close is left alone.
    let (mut f2, dir2) = close_target_fixture();
    f2.merged[0].body = "does not fix #3\n".into();
    f2.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::closed(&[]));
    f2.cbm.insert("3".into(), closed_before_merge(3));
    run(&f2, &RecordStore::for_root(dir2.path()), true);
    assert_eq!(f2.state("3"), "closed");
    assert_eq!(f2.comment_count("3"), 0);
}

#[test]
fn an_open_negated_only_issue_stays_open() {
    let (mut f, dir) = close_target_fixture();
    f.merged[0].body = "does not fix #3\n".into();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("3"), "open");
    assert!(!f.calls().iter().any(|c| c == "close 3"));
}

// ---------------------------------------------------------------------------
// branch close + strip, and their ordering
// ---------------------------------------------------------------------------

#[test]
#[allow(clippy::field_reassign_with_default)]
fn the_branch_derived_issue_closes_before_the_strip_reads_it() {
    let mut f = FakeForge::default();
    f.merged = vec![MergedPull {
        body: String::new(),
        ..merged_row()
    }];
    f.close_targets.borrow_mut().replace(String::new());
    f.issues
        .borrow_mut()
        .insert("42".into(), FakeIssue::open(&["loom:building"]));
    f.issues
        .borrow_mut()
        .insert("3".into(), FakeIssue::open(&["loom:building"]));
    f.merged[0].body = "Closes #3\n".into();
    f.close_targets.borrow_mut().replace("3\n".to_string());
    // Both closes pass their reopen gates (#15145): readable timelines,
    // no post-merge reopen.
    f.timeline_at("3", BEFORE_MERGE);
    f.timeline_at("42", BEFORE_MERGE);
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("42"), "closed");
    assert_eq!(f.labels("3"), [] as [String; 0]);
    // Ordering: the close check's close of #3 precedes the strip's label
    // removal on the same issue (partial-reset -> close check -> branch
    // close -> strip).
    let calls = f.calls();
    let close_at = calls.iter().position(|c| c == "close 3").unwrap();
    let strip_at = calls
        .iter()
        .position(|c| c.starts_with("forge_gh_remove_label_rl_safe") && c.contains(" 3 "))
        .unwrap();
    assert!(close_at < strip_at);
}

#[test]
fn a_partial_failure_resumes_from_the_last_completed_pass() {
    let f = partial_fixture();
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    // Interrupt after the partial-reset landed but before the record
    // recorded it: the next observation replays the merge identity and
    // converges without a duplicate comment.
    // The partial-reset ran (as the shell would have run it), then the
    // process died after persisting the pass mark but nothing else.
    let shell_inputs = bookkeeping::Inputs {
        repo: "o/r".into(),
        pr_number: "9".into(),
        pr_branch: "feature/issue-42".into(),
        pr_body: f.merged[0].body.clone(),
        conflicted: String::new(),
        open_before_merge: "5".into(),
        merged_at: None,
    };
    match bookkeeping::run(
        bookkeeping::Pass::PartialReset,
        &shell_inputs,
        &bookkeeping::Facts::default(),
        &f,
    ) {
        bookkeeping::Outcome::Done(_) => {}
        bookkeeping::Outcome::Need(n) => panic!("unexpected need {n:?}"),
        bookkeeping::Outcome::Defer(_) => panic!("unexpected defer"),
    }
    let mut rec = MergeRecord {
        repo: "o/r".into(),
        pr: 9,
        head: "h1".into(),
        merge_commit: "m1".into(),
        merged_at: MERGED_AT.into(),
        branch: "feature/issue-42".into(),
        body: f.merged[0].body.clone(),
        observed_at: MERGED_AT.into(),
        done: Vec::new(),
    };
    rec.mark(PARTIAL_RESET);
    store.store(&rec).unwrap();
    run(&f, &store, false);
    assert_eq!(f.comment_count("5"), 1);
    assert!(store.load(9).unwrap().complete());
}

#[test]
fn a_deferred_record_retries_after_its_pr_leaves_the_discovery_window() {
    let mut f = partial_fixture();
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    // Tick 1: close targets unreadable -> the record is pinned but deferred.
    *f.close_targets.borrow_mut() = None;
    run(&f, &store, false);
    assert!(!store.load(9).unwrap().complete());
    // PR 9 is evicted from the listing; targets become readable again.
    f.merged.clear();
    *f.close_targets.borrow_mut() = Some(String::new());
    let stats = run(&f, &store, false);
    assert_eq!(stats.merges_seen, 0);
    assert_eq!(stats.records_completed, 1);
    assert!(store.load(9).unwrap().complete());
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn the_branch_derived_close_respects_a_post_merge_reopen() {
    let mut f = FakeForge::default();
    f.merged = vec![MergedPull {
        body: String::new(),
        ..merged_row()
    }];
    f.close_targets.borrow_mut().replace(String::new());
    f.issues
        .borrow_mut()
        .insert("42".into(), FakeIssue::open(&["loom:building"]));
    f.reopened_at("42", AFTER_MERGE);
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("42"), "open");
    assert!(!f.calls().iter().any(|c| c == "close 42"));
    // The verified reopen completed the pass: no retry loop (#15145).
    assert!(store.load(9).unwrap().complete());
}

#[test]
#[allow(clippy::field_reassign_with_default)]
fn the_branch_derived_close_defers_on_an_unreadable_reopen_timeline() {
    let mut f = FakeForge::default();
    f.merged = vec![MergedPull {
        body: String::new(),
        ..merged_row()
    }];
    f.close_targets.borrow_mut().replace(String::new());
    f.issues
        .borrow_mut()
        .insert("42".into(), FakeIssue::open(&["loom:building"]));
    // No timeline for #42: the observation-driven branch close defers.
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    run(&f, &store, true);
    assert_eq!(f.state("42"), "open");
    assert!(!f.calls().iter().any(|c| c == "close 42"));
    let rec = store.load(9).unwrap();
    assert!(!rec.complete());
    assert!(!rec.done.iter().any(|d| d == BRANCH_CLOSE));
    // Readable, with no post-merge reopen: a later tick closes it.
    f.reopened_at("42", BEFORE_MERGE);
    run(&f, &store, true);
    assert_eq!(f.state("42"), "closed");
    assert!(store.load(9).unwrap().complete());
}

fn rec_for(pr: u64, observed_at: &str, done: &[&str]) -> MergeRecord {
    MergeRecord {
        repo: "o/r".into(),
        pr,
        head: "h1".into(),
        merge_commit: "m1".into(),
        merged_at: MERGED_AT.into(),
        branch: "feature/issue-42".into(),
        body: String::new(),
        observed_at: observed_at.to_string(),
        done: done.iter().map(|s| (*s).to_string()).collect(),
    }
}

#[test]
fn an_expired_incomplete_record_is_dropped_and_young_or_complete_survive() {
    let f = FakeForge::default();
    let dir = tempfile::tempdir().unwrap();
    let store = RecordStore::for_root(dir.path());
    let week_ago = (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339();
    let now = chrono::Utc::now().to_rfc3339();
    store.store(&rec_for(20, &week_ago, &PASSES)).unwrap();
    store.store(&rec_for(21, &week_ago, &[])).unwrap();
    store.store(&rec_for(22, &now, &[])).unwrap();
    let stats = run(&f, &store, false);
    // Only the expired incomplete record is reclaimed (#15145): the
    // complete one keeps making its merge invisible, the young one is
    // still retried.
    assert_eq!(stats.records_expired, 1);
    assert!(store.load(20).unwrap().complete());
    assert!(store.load(21).is_none());
    assert!(!store.load(22).unwrap().complete());
}

#[test]
fn ghforge_resolve_pins_identity_and_refuses_without_one() {
    let gh = Path::new("/nonexistent/gh");
    let root = Path::new("/tmp");
    let f = GhForge::resolve(gh, root, |_, _| Some("o/r".into())).unwrap();
    assert_eq!(Forge::nwo(&f).as_deref(), Some("o/r"));
    assert!(GhForge::resolve(gh, root, |_, _| None).is_none());
    assert!(GhForge::resolve(gh, root, |_, _| Some("noslash".into())).is_none());
    // The unresolved constructor is the defect the review measured.
    assert_eq!(Forge::nwo(&GhForge::new(gh, root)), None);
}

#[test]
fn a_resolved_ghforge_reaches_the_discovery_listing() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("gh.log");
    let bin = dir.path().join("fake-gh.sh");
    std::fs::write(
        &bin,
        format!("#!/usr/bin/env bash\nprintf '%s\\n' \"$*\" >> '{}'\necho '[]'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let forge = GhForge::resolve(&bin, dir.path(), |_, _| Some("o/r".into())).unwrap();
    let stats = run_pass(&forge, &RecordStore::for_root(dir.path()), false, || false);
    assert_eq!(stats.merges_seen, 0);
    let calls = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(calls.contains("pulls?state=closed"), "listing never issued: {calls:?}");
}

// ---------------------------------------------------------------------------
// the convergence fixture (AC: script merge vs direct forge merge)
// ---------------------------------------------------------------------------

/// The one scenario both entry points drive. Issues:
///
/// - `#3`  close target, closed at merge, still `loom:building` → stripped
/// - `#5`  open partial increment, `loom:building`, claim predates merge →
///   swapped to `loom:issue` + one partial comment
/// - `#6`  partial increment auto-closed by a stray `close #6` → the #4569
///   reopen, swap, then re-closed by the close check and stripped of
///   `loom:issue` (#11517)
/// - `#7`  partial increment closed BEFORE the merge by another PR →
///   untouched (unrelated historical close)
/// - `#8`  open partial increment re-claimed AFTER the merge → the daemon
///   gate keeps the newer claim (the one intended divergence: the shell's
///   synchronous pass had no gate)
/// - `#10` held-remainder partial increment → `loom:operator` (#9264)
/// - `#11` close target that lost its Builder label → nothing to do
/// - `#42` the `feature/issue-42` branch issue, open, undeclared →
///   branch-derived close (#12615)
#[allow(clippy::field_reassign_with_default)]
fn seeded_forge() -> FakeForge {
    let mut f = FakeForge::default();
    f.branch = "feature/issue-42".into();
    f.commits = "wip\n".into();
    f.merged = vec![MergedPull {
        number: 9,
        head: "h1".into(),
        merge_commit: "m1".into(),
        merged_at: MERGED_AT.into(),
        branch: "feature/issue-42".into(),
        body: "Implements a slice.\n\nPart of #5\nPart of #6\nPart of #7\nPart of #8\nPart of #10\n\nThe checklist said to close #6 in passing, which was not intended.\n\nCloses #3\n".into(),
    }];
    f.close_targets
        .borrow_mut()
        .replace("3\n6\n11\n".to_string());
    let mut issues = f.issues.borrow_mut();
    issues.insert("3".into(), FakeIssue::closed(&["loom:building"]));
    issues.insert("5".into(), FakeIssue::open(&["loom:building"]));
    issues.insert(
        "6".into(),
        FakeIssue {
            state: "closed".into(),
            labels: vec!["loom:building".into()],
            ..FakeIssue::default()
        },
    );
    issues.insert("7".into(), FakeIssue::closed(&["loom:building"]));
    issues.insert("8".into(), FakeIssue::open(&["loom:building"]));
    issues.insert(
        "10".into(),
        FakeIssue {
            state: "open".into(),
            labels: vec!["loom:building".into()],
            body: "<!-- loom:remainder-hold reason=\"operator scope\" -->".into(),
            ..FakeIssue::default()
        },
    );
    issues.insert("11".into(), FakeIssue::closed(&[]));
    issues.insert("42".into(), FakeIssue::open(&["loom:building"]));
    drop(issues);
    for n in ["3", "5", "6", "7", "10", "11", "42"] {
        f.timeline_at(n, BEFORE_MERGE);
    }
    // #8 was re-claimed after the merge.
    f.timeline_at("8", AFTER_MERGE);
    // The #4569 attribution for #6: this merge closed it.
    f.cbm.insert("6".into(), this_merge(6));
    // #7 was closed before the merge by another PR.
    f.cbm.insert("7".into(), closed_before_merge(7));
    f
}

/// Snapshot of everything the fixture compares: state, sorted labels, and
/// audit-comment headings (bodies carry timestamps, headings do not).
type Snapshot = (String, Vec<String>, Vec<String>);

fn snapshot(f: &FakeForge, n: &str) -> Snapshot {
    let i = f.issue(n);
    let mut labels = i.labels.clone();
    labels.sort();
    let mut headings = i
        .comments
        .iter()
        .filter_map(|c| c.lines().next().map(|l| l.trim().to_string()))
        .collect::<Vec<_>>();
    headings.sort();
    (i.state, labels, headings)
}

#[test]
fn script_merge_and_direct_forge_merge_converge_after_one_tick() {
    // -- the script merge: the retired shell passes, with the shell's own
    //    pre-merge facts, then one observation tick --
    let script = seeded_forge();
    let script_store = RecordStore::for_root(tempfile::tempdir().unwrap().path());
    let shell_inputs = |body: String| bookkeeping::Inputs {
        repo: "o/r".into(),
        pr_number: "9".into(),
        pr_branch: "feature/issue-42".into(),
        pr_body: body,
        conflicted: "6".into(),
        open_before_merge: "5 6 8 10".into(),
        merged_at: None,
    };
    let shell_facts = bookkeeping::Facts {
        close_targets: Some("3\n6\n11\n".into()),
    };
    for pass in [
        bookkeeping::Pass::PartialReset,
        bookkeeping::Pass::BranchClose,
        bookkeeping::Pass::Strip,
    ] {
        match bookkeeping::run(
            pass,
            &shell_inputs(script.merged[0].body.clone()),
            &shell_facts,
            &script,
        ) {
            bookkeeping::Outcome::Done(_) => {}
            bookkeeping::Outcome::Need(n) => panic!("unexpected need {n:?}"),
            bookkeeping::Outcome::Defer(_) => panic!("unexpected defer"),
        }
    }
    let script_stats = run(&script, &script_store, true);

    // -- the equivalent merge made directly on the forge: one tick only --
    let direct = seeded_forge();
    let direct_store = RecordStore::for_root(tempfile::tempdir().unwrap().path());
    let direct_stats = run(&direct, &direct_store, true);

    assert_eq!(script_stats, direct_stats);
    assert_eq!(script_stats.records_completed, 1);

    for n in ["3", "5", "6", "7", "10", "11", "42"] {
        assert_eq!(
            snapshot(&script, n),
            snapshot(&direct, n),
            "issue #{n} diverged between the script and direct merges"
        );
    }
    // The one intended divergence (#14902's newer-claim gate): the shell's
    // synchronous pass reset the post-merge claim; the observed pass keeps
    // the live claim it can only attribute to a newer Builder.
    assert_eq!(
        snapshot(&script, "8"),
        (
            "open".into(),
            vec!["loom:issue".to_string()],
            vec!["## Partial Increment Merged".to_string()]
        )
    );
    assert_eq!(
        snapshot(&direct, "8"),
        ("open".into(), vec!["loom:building".to_string()], Vec::<String>::new())
    );

    // End states, spelled out once so a failure reads:
    // - #3 stripped, - #5 swapped, - #6 reopened then re-closed and stripped
    // to nothing (#11517), - #7 untouched, - #10 held for the operator,
    // - #42 branch-closed.
    assert_eq!(
        snapshot(&direct, "3"),
        ("closed".into(), Vec::<String>::new(), Vec::<String>::new())
    );
    assert_eq!(
        snapshot(&direct, "5"),
        (
            "open".into(),
            vec!["loom:issue".to_string()],
            vec!["## Partial Increment Merged".to_string()]
        )
    );
    assert_eq!(snapshot(&direct, "6").0, "closed");
    assert_eq!(snapshot(&direct, "6").1, Vec::<String>::new());
    assert_eq!(
        snapshot(&direct, "6").2,
        vec![
            "## Partial Increment Merged".to_string(),
            "## Premature Auto-Close Reverted".to_string(),
            "Closed by PR #9 (merge observed by Loom post-merge bookkeeping, #14902).".to_string(),
        ]
    );
    assert_eq!(
        snapshot(&direct, "7"),
        ("closed".into(), vec!["loom:building".to_string()], Vec::<String>::new())
    );
    assert_eq!(
        snapshot(&direct, "10"),
        (
            "open".into(),
            vec!["loom:operator".to_string()],
            vec!["## Partial Increment Merged (remainder held)".to_string()]
        )
    );
    assert_eq!(
        snapshot(&direct, "11"),
        ("closed".into(), Vec::<String>::new(), Vec::<String>::new())
    );
    assert_eq!(snapshot(&direct, "42").0, "closed");
    assert_eq!(snapshot(&direct, "42").1, vec!["loom:building".to_string()]);
    assert_eq!(snapshot(&direct, "42").2.len(), 1);

    // Recorded identities, shown (#14902): both sides pinned the same
    // repository, PR, landed head and merge commit, and completed all four
    // passes over them.
    for store in [&script_store, &direct_store] {
        let rec = store.load(9).unwrap();
        assert_eq!(
            (
                rec.repo.as_str(),
                rec.pr,
                rec.head.as_str(),
                rec.merge_commit.as_str(),
                rec.branch.as_str()
            ),
            ("o/r", 9, "h1", "m1", "feature/issue-42")
        );
        assert_eq!(rec.done, ["partial-reset", "close-check", "branch-close", "strip"]);
        assert!(rec.body.contains("Part of #5"));
    }

    // Re-observing the same merge converges: nothing moves, no new comments.
    let before: Vec<(&str, Snapshot)> = ["3", "5", "6", "7", "10", "11", "42"]
        .iter()
        .map(|n| (*n, snapshot(&direct, n)))
        .collect();
    run(&direct, &direct_store, true);
    for (n, was) in before {
        assert_eq!(snapshot(&direct, n), was, "replay moved issue #{n}");
    }
}
