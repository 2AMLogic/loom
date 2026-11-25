//! Tests for the ratification lane's marker posting and tick (#13345).

use super::*;
use crate::champion_hold::ratification::{ChangedFile, Comment};
use crate::role_runner::ratification_dispatch::InstructionSource;
use crate::role_runner::ratification_lane::{
    keys_marked, run_lane, CommentSink, Config, Key, OpenPr, PrSource,
};
use serde_json::json;
use std::cell::RefCell;

const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
const EE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MKT_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn cfg() -> Config {
    Config::from_effective(&json!({"ratificationLane": {
        "enabled": true,
        "ee": {"repo": "k/i", "path": "ee.md", "commit": EE_SHA},
        "market": {"repo": "k/i", "path": "market.md", "commit": MKT_SHA}
    }}))
}

struct Prs {
    author: &'static str,
    /// Calls to `open_prs`, to prove a disabled lane never touches the forge.
    listed: RefCell<u32>,
}

impl PrSource for Prs {
    fn open_prs(&self) -> Vec<OpenPr> {
        *self.listed.borrow_mut() += 1;
        vec![OpenPr {
            number: 7,
            author: self.author.into(),
            head_sha: HEAD.into(),
        }]
    }
    fn files(
        &self,
        _pr: u64,
        _hold: &crate::champion_hold::ratification::Config,
    ) -> Vec<ChangedFile> {
        vec![ChangedFile {
            filename: "ratification/requests/a.md".into(),
            previous_filename: None,
            status: "added".into(),
            head_content: Some("Ratified only on operator merge.".into()),
        }]
    }
    fn trusted_comments(&self, _pr: u64) -> Result<Vec<Comment>, String> {
        Ok(Vec::new())
    }
}

struct Instr {
    fail_market: bool,
}

impl InstructionSource for Instr {
    fn fetch(&self, pin: &crate::role_runner::ratification_lane::Pin) -> Result<String, String> {
        if self.fail_market && pin.path == "market.md" {
            Err("HTTP 404".into())
        } else {
            Ok("INSTR".into())
        }
    }
    fn repo_is_private(&self, _repo: &str) -> Option<bool> {
        Some(true)
    }
}

#[derive(Default)]
struct Sink {
    posted: RefCell<Vec<(u64, String)>>,
}

impl CommentSink for Sink {
    fn post(&self, pr: u64, body: &str) -> Result<(), String> {
        self.posted.borrow_mut().push((pr, body.to_string()));
        Ok(())
    }
}

struct Runner {
    outcome: RoleTickOutcome,
    calls: u32,
    /// Overrides `last_output` for both keys; `None` returns a valid review
    /// for the key that ran last.
    output: Option<String>,
    last_key: std::cell::Cell<Option<Key>>,
}

impl RoleInvocationRunner for Runner {
    fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
        self.calls += 1;
        self.last_key.set(Some(if role == "ratification-market" {
            Key::Market
        } else {
            Key::Ee
        }));
        self.outcome.clone()
    }
    fn last_output(&self) -> Option<String> {
        Some(match &self.output {
            Some(o) => o.clone(),
            None => valid_review(self.last_key.get().unwrap_or(Key::Ee)),
        })
    }
}

fn ok_runner() -> Runner {
    Runner {
        outcome: RoleTickOutcome::Success,
        calls: 0,
        output: None,
        last_key: std::cell::Cell::new(None),
    }
}

/// A well-formed review for `key` per the pinned output contract (#13762).
fn valid_review(key: Key) -> String {
    match key {
        Key::Ee => {
            "## EE-key review: sg13g2 comparator\n\n**Overall verdict:** approve, small and sane.\n"
                .into()
        }
        Key::Market => {
            "## Market-key review: sg13g2 comparator\n\n**Overall verdict:** competitive, priced below alternates.\n"
                .into()
        }
    }
}

/// Launcher/wrapper lines that must never reach a posted comment body.
const LAUNCHER_NOISE: &str = "spawn-claude preflight: token pool ok\n\
     Permission deny rule fired (Bash)\n\
     auth mode: OAuth (len=118 tail=abcd)\n\
     reaped process group 1234\n";

/// Launcher/wrapper lines the wrapper writes *after* the model's final
/// message — the measured tail of a real role log (#13762). The review ends
/// at their first line; they must never reach a posted comment body.
const WRAPPER_TAIL: &str = "[2026-10-15T00:38:41Z] [INFO] Claude CLI completed successfully\n\
     [2026-10-15T00:38:41Z] [INFO] Claude wrapper exiting with code 0\n\
     [claude-wrapper] reaping 3 residual process(es) in process group 4124342 at exit (#6192): 77007 77203 77205\n";

fn comments(sink: &Sink) -> Vec<Comment> {
    sink.posted
        .borrow()
        .iter()
        .map(|(_, b)| Comment::new("bot[bot]", b))
        .collect()
}

#[test]
fn one_candidate_posts_one_marker_per_key_with_selected_head() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let posted = sink.posted.borrow();
    assert_eq!(posted.len(), 2);
    assert!(posted[0].1.contains("reviewer=ratification-ee"));
    assert!(posted[1].1.contains("reviewer=ratification-market"));
    drop(posted);
    let keys = keys_marked(&comments(&sink), HEAD);
    assert!(keys.contains(&Key::Ee) && keys.contains(&Key::Market));
}

#[test]
fn failed_closed_key_posts_nothing_and_other_key_is_unaffected() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    let results = run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: true }, &sink, &mut r);
    assert_eq!(keys_marked(&comments(&sink), HEAD), vec![Key::Ee]);
    assert_eq!(sink.posted.borrow().len(), 1);
    assert_eq!(r.calls, 1);
    assert_buckets(
        &TickOutcome::from_run(&results, 0),
        &[("ee", "posted"), ("market", "pre_launch_failed")],
    );
}

#[test]
fn non_success_session_fails_closed_without_marker() {
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let mut r = Runner {
        outcome: RoleTickOutcome::Failure("boom".into()),
        calls: 0,
        output: None,
        last_key: std::cell::Cell::new(None),
    };
    let results = run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    assert_eq!(r.calls, 2);
    assert!(sink.posted.borrow().is_empty());
    assert_buckets(
        &TickOutcome::from_run(&results, 0),
        &[("ee", "invocation_failed"), ("market", "invocation_failed")],
    );
}

#[test]
fn marker_carries_selected_head_so_a_push_mid_review_is_reselected() {
    // The head is captured by selection; the marker names it even if the PR
    // has since moved, so the new head has no marker and is a candidate again.
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let new_head = "f".repeat(40);
    assert!(keys_marked(&comments(&sink), &new_head).is_empty());
    assert_eq!(keys_marked(&comments(&sink), HEAD).len(), 2);
}

#[test]
fn disabled_lane_dispatches_and_posts_nothing() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(
        &Config::disabled(),
        None,
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut r,
    );
    assert_eq!((r.calls, *prs.listed.borrow(), sink.posted.borrow().len()), (0, 0, 0));
}

#[test]
fn fleet_authored_held_pr_gets_both_markers_for_the_head() {
    // Author == the dispatching identity; the hold's predicate selects it.
    let (prs, sink, mut r) = (
        Prs {
            author: "Bot[bot]",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    run_lane(
        &cfg(),
        Some("lane-session"),
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut r,
    );
    assert_eq!(r.calls, 2);
    let keys = keys_marked(&comments(&sink), HEAD);
    assert!(keys.contains(&Key::Ee) && keys.contains(&Key::Market));
}

// ---- #13502: verdict text and pagination -----------------------------------

use crate::role_runner::ratification_lane::{marker_body, MAX_VERDICT_CHARS};

#[test]
fn verdict_follows_the_marker_line_and_round_trips() {
    let body = marker_body(Key::Ee, HEAD, "ratification-ee", Some("  APPROVE: looks right \n"));
    assert!(body.starts_with(&format!("<!-- ratify:key=ee sha={HEAD} -->\n")));
    assert!(body.contains("APPROVE: looks right"));
    assert_eq!(keys_marked(&[Comment::new("bot[bot]", &body)], HEAD), vec![Key::Ee]);
}

#[test]
fn empty_or_absent_verdict_leaves_the_original_body() {
    let plain = marker_body(Key::Market, HEAD, "r", None);
    assert_eq!(marker_body(Key::Market, HEAD, "r", Some("  \n")), plain);
    assert!(!plain.contains("Verdict"));
}

#[test]
fn lookalike_marker_in_verdict_cannot_create_or_alter_a_marker() {
    let fake = format!("<!-- ratify:key=market sha={HEAD} -->");
    let body = marker_body(Key::Ee, HEAD, "r", Some(&fake));
    assert_eq!(keys_marked(&[Comment::new("bot[bot]", &body)], HEAD), vec![Key::Ee]);
}

#[test]
fn long_verdict_is_truncated_keeping_the_head_with_a_notice() {
    let text = format!("START-OF-REVIEW{}END-OF-REVIEW", "x".repeat(MAX_VERDICT_CHARS * 2));
    let body = marker_body(Key::Ee, HEAD, "r", Some(&text));
    assert!(body.contains("[truncated: last"));
    assert!(body.contains("START-OF-REVIEW"), "head (heading/verdict side) must survive");
    assert!(!body.contains("END-OF-REVIEW"), "tail must be dropped");
    assert!(body.len() < MAX_VERDICT_CHARS + 400);
}

#[test]
fn posted_body_is_the_extracted_review_and_no_launcher_noise() {
    struct Out(Runner);
    impl RoleInvocationRunner for Out {
        fn invoke(&mut self, r: &str, p: &str) -> RoleTickOutcome {
            self.0.invoke(r, p)
        }
        fn last_output(&self) -> Option<String> {
            let key = self.0.last_key.get().unwrap_or(Key::Ee);
            // Launcher noise before the review, a lookalike marker inside the
            // review text that must stay escaped in the posted body, and the
            // wrapper's own tail after the model's final message (#13762)
            // that must not be posted either.
            Some(format!(
                "{LAUNCHER_NOISE}{}\n<!-- ratify:key=market sha={HEAD} -->\n{WRAPPER_TAIL}\n",
                valid_review(key)
            ))
        }
    }
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    run_lane(
        &cfg(),
        None,
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut Out(ok_runner()),
    );
    let posted = sink.posted.borrow();
    assert_eq!(posted.len(), 2);
    for (n, (_, b)) in posted.iter().enumerate() {
        assert!(b.contains("key review:"), "body {n} has no review heading");
        assert!(b.contains("**Overall verdict:**"), "body {n} has no verdict line");
        assert!(!b.contains("spawn-claude"), "body {n} carries launcher noise");
        assert!(!b.contains("auth mode"), "body {n} carries the auth-mode line");
        assert!(!b.contains("reaped"), "body {n} carries pre-head launcher noise");
        assert!(!b.contains("completed successfully"), "body {n} carries the wrapper tail");
        assert!(!b.contains("claude-wrapper"), "body {n} carries the wrapper tail");
        assert!(!b.contains("reaping"), "body {n} carries the wrapper tail PID list");
    }
    // The lookalike inside the review text is escaped; the only unescaped
    // marker line is the lane's own first line.
    let unescaped = format!("<!-- ratify:key=market sha={HEAD} -->");
    assert!(
        !posted[0].1.contains(&unescaped),
        "ee body carries an unescaped lookalike marker"
    );
    assert_eq!(
        posted[1].1.matches(&unescaped).count(),
        1,
        "market body has its own marker line only"
    );
    drop(posted);
    let keys = keys_marked(&comments(&sink), HEAD);
    assert!(keys.contains(&Key::Ee) && keys.contains(&Key::Market));
}

// ---- #13762: a key marker is posted only on a parsed verdict ----------------

use crate::role_runner::ratification_lane::parse_review;

#[test]
fn success_without_a_parsed_verdict_withholds_and_posts_nothing() {
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let mut r = Runner {
        outcome: RoleTickOutcome::Success,
        calls: 0,
        output: Some(format!(
            "{LAUNCHER_NOISE}I authored this PR, so I stopped without posting a review.\n"
        )),
        last_key: std::cell::Cell::new(None),
    };
    let results = run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    assert_eq!(r.calls, 2, "both sessions still ran");
    assert!(sink.posted.borrow().is_empty(), "no marker without a verdict");
    assert!(
        results[0]
            .1
            .iter()
            .all(|(_, p)| matches!(p, KeyPost::Withheld(..))),
        "{:?}",
        results
    );
    assert!(keys_marked(&comments(&sink), HEAD).is_empty());
}

#[test]
fn a_withheld_key_does_not_suppress_a_later_rerun_at_the_same_head() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let mut r = Runner {
        outcome: RoleTickOutcome::Success,
        calls: 0,
        output: Some("stopped without a verdict".into()),
        last_key: std::cell::Cell::new(None),
    };
    let instr = Instr { fail_market: false };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!((r.calls, sink.posted.borrow().len()), (2, 0));
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls, 4, "no marker exists, so the same head is selected again");
    assert!(sink.posted.borrow().is_empty());
}

#[test]
fn one_key_with_a_verdict_and_one_without_post_independently() {
    struct Mixed(Runner);
    impl RoleInvocationRunner for Mixed {
        fn invoke(&mut self, r: &str, p: &str) -> RoleTickOutcome {
            self.0.invoke(r, p)
        }
        fn last_output(&self) -> Option<String> {
            match self.0.last_key.get() {
                Some(Key::Market) => Some("declined: disclosure problem".into()),
                _ => Some(
                    "## EE-key review: fixture\n\n**Overall verdict:** request-changes\n".into(),
                ),
            }
        }
    }
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let results = run_lane(
        &cfg(),
        None,
        "o/r",
        &prs,
        &Instr { fail_market: false },
        &sink,
        &mut Mixed(ok_runner()),
    );
    assert_eq!(keys_marked(&comments(&sink), HEAD), vec![Key::Ee]);
    assert_eq!(sink.posted.borrow().len(), 1);
    assert_eq!(results[0].1[0].1, KeyPost::Posted);
    assert!(matches!(results[0].1[1].1, KeyPost::Withheld(..)));
    assert!(sink.posted.borrow()[0].1.contains("request-changes"));
    assert_buckets(
        &TickOutcome::from_run(&results, 0),
        &[("ee", "posted"), ("market", "verdict_invalid")],
    );
}

#[test]
fn positive_prelaunch_evidence_is_captured_before_the_next_key() {
    use crate::observability::lifecycle::{
        pre_launch, RoleFailure, ROLL_INTERRUPTED, UNCLASSIFIED_PRE_LAUNCH,
    };

    struct AdmissionOnce {
        ee_outcome: RoleTickOutcome,
        evidence: Option<&'static str>,
        last_ee: bool,
    }
    impl RoleInvocationRunner for AdmissionOnce {
        fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
            self.last_ee = role == "ratification-ee";
            if self.last_ee {
                self.ee_outcome.clone()
            } else {
                RoleTickOutcome::Success
            }
        }
        fn unlaunched_failure(&self) -> Option<RoleFailure> {
            self.evidence
                .filter(|_| self.last_ee)
                .map(|class| RoleFailure::new(class, None, "local fixture detail"))
        }
    }

    let failure = RoleTickOutcome::Failure("local admission detail".into());
    for (ee_outcome, evidence, expected) in [
        (
            failure.clone(),
            Some(pre_launch::WRITE_SCOPE_REFUSED.class),
            "pre_launch_failed",
        ),
        (RoleTickOutcome::NoTokenPool, None, "pre_launch_failed"),
        (failure.clone(), None, "invocation_failed"),
        (failure.clone(), Some(UNCLASSIFIED_PRE_LAUNCH), "invocation_failed"),
        (failure, Some(ROLL_INTERRUPTED), "invocation_failed"),
    ] {
        let sink = Sink::default();
        let results = run_lane(
            &cfg(),
            None,
            "o/r",
            &Feed::new(),
            &CountingInstr::default(),
            &sink,
            &mut AdmissionOnce {
                ee_outcome,
                evidence,
                last_ee: false,
            },
        );
        assert!(sink.posted.borrow().is_empty());
        assert_buckets(
            &TickOutcome::from_run(&results, 0),
            &[("ee", expected), ("market", "output_missing")],
        );
    }
}

#[test]
fn parse_review_takes_the_last_heading_and_ignores_noise_before_it() {
    let out = format!(
        "echo ## EE-key review: quoted\n**Overall verdict:** escalate, noise\n{}tail kept\n",
        valid_review(Key::Ee)
    );
    let r = parse_review(Key::Ee, &out).expect("last heading wins");
    assert_eq!(r.token, "approve");
    assert!(r.text.starts_with("## EE-key review: sg13g2 comparator"));
    assert!(r.text.ends_with("tail kept"));
    assert!(!r.text.contains("quoted"));
}

#[test]
fn parse_review_accepts_both_token_terminators() {
    let comma = "## EE-key review: b\n**Overall verdict:** approve, why\n";
    let space = "## EE-key review: b\n**Overall verdict:** request-changes because\n";
    assert_eq!(parse_review(Key::Ee, comma).unwrap().token, "approve");
    assert_eq!(parse_review(Key::Ee, space).unwrap().token, "request-changes");
}

#[test]
fn parse_review_ends_at_the_wrapper_tail_not_end_of_output() {
    // The real log shape (#13762): the model's review, then the wrapper's
    // own lines written after the final message.
    let out = format!("{}{}\n", valid_review(Key::Ee), WRAPPER_TAIL);
    let r = parse_review(Key::Ee, &out).expect("verdict parsed");
    assert_eq!(r.token, "approve");
    assert!(r.text.starts_with("## EE-key review:"));
    assert!(r.text.ends_with("small and sane."));
    assert!(!r.text.contains("completed successfully"), "{:?}", r.text);
    assert!(!r.text.contains("claude-wrapper"), "{:?}", r.text);
    assert!(!r.text.contains("reaping"), "{:?}", r.text);
    // A timestamped line mid-review ends the review there too.
    let out = format!(
        "{}[2026-11-17T12:05:00Z] [ERROR] probe\nmore prose after the tail line\n",
        valid_review(Key::Ee)
    );
    let r = parse_review(Key::Ee, &out).expect("verdict parsed");
    assert!(!r.text.contains("more prose after the tail line"), "{:?}", r.text);
}

#[test]
fn parse_review_rejects_missing_heading_or_verdict_line() {
    let e = parse_review(Key::Ee, "stopped without a verdict")
        .unwrap_err()
        .to_string();
    assert!(e.contains("## EE-key review:"), "{e}");
    // The other key's heading is not this key's.
    let e = parse_review(Key::Ee, &valid_review(Key::Market))
        .unwrap_err()
        .to_string();
    assert!(e.contains("## EE-key review:"), "{e}");
    let e = parse_review(Key::Market, "## Market-key review: b\nno verdict line\n")
        .unwrap_err()
        .to_string();
    assert!(e.contains("**Overall verdict:**"), "{e}");
}

#[test]
fn parse_review_rejects_unknown_or_conflicting_tokens() {
    let review = |t: &str| format!("## EE-key review: b\n**Overall verdict:** {t}, why\n");
    let e = parse_review(Key::Ee, &review("thumbs-up"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("thumbs-up"), "{e}");
    // A market token on an ee review is unknown, and vice versa.
    assert!(parse_review(Key::Ee, &review("competitive")).is_err());
    assert!(parse_review(Key::Market, &valid_review(Key::Ee)).is_err());
    let both = "## EE-key review: b\n**Overall verdict:** approve, a\n**Overall verdict:** request-changes, b\n";
    let e = parse_review(Key::Ee, both).unwrap_err().to_string();
    assert!(e.contains("conflicting"), "{e}");
}

// ---- #15315: harmless verdict formatting and the echoed template -----------

use crate::role_runner::ratification_lane::ReviewError;

/// The pinned EE instructions' output template (2am
/// `ratification/ee-key/SKILL.md@fed7a23` ~:249-255, as quoted in #15315):
/// the intro line names the heading mid-line, then the fenced block.
pub(super) const EE_TEMPLATE: &str = "Write only the human text, starting at the \
`## EE-key review:` heading, with this structure:\n\n```\n\
## EE-key review: <block name / class>\n\n\
**Overall verdict:** <approve|request-changes>, one sentence why.\n```\n";

/// The pinned market instructions' output template (product
/// `ratification/market-key/SKILL.md@d4a4862` ~:266-272, as quoted in #15315).
pub(super) const MARKET_TEMPLATE: &str = "Write only the human text, starting at the \
`## Market-key review:` heading, with this structure:\n\n```\n\
## Market-key review: <block name / class>\n\n\
**Overall verdict:** <competitive|adequate-for-catalog|uncompetitive|escalate>, one sentence why.\n```\n";

/// A one-line review for `key` whose verdict line reads `verdict`.
fn review_with(key: Key, verdict: &str) -> String {
    format!("{} b\n**Overall verdict:** {verdict}\n", key.review_heading())
}

fn token_of(key: Key, verdict: &str) -> String {
    match parse_review(key, &review_with(key, verdict)) {
        Ok(r) => r.token,
        Err(e) => panic!("{verdict:?} on {key:?}: {e}"),
    }
}

#[test]
fn parse_review_accepts_trailing_sentence_punctuation() {
    for key in [Key::Ee, Key::Market] {
        for token in key.allowed_tokens() {
            for suffix in [".", "!", ";", ":", ".."] {
                assert_eq!(token_of(key, &format!("{token}{suffix}")), *token, "{suffix}");
                assert_eq!(token_of(key, &format!("{token}{suffix} why")), *token, "{suffix}");
            }
        }
    }
    assert_eq!(token_of(Key::Ee, "approve. Small and sane."), "approve");
}

#[test]
fn parse_review_unwraps_balanced_emphasis() {
    for (key, verdict, token) in [
        (Key::Market, "**escalate**.", "escalate"),
        (Key::Market, "`escalate`.", "escalate"),
        (Key::Market, "**escalate.**", "escalate"),
        (Key::Ee, "*approve*", "approve"),
        (Key::Ee, "__approve__", "approve"),
        (Key::Ee, "_approve_.", "approve"),
        (Key::Ee, "**request-changes**, why", "request-changes"),
    ] {
        assert_eq!(token_of(key, verdict), token, "{verdict}");
    }
}

#[test]
fn parse_review_folds_ascii_case() {
    assert_eq!(token_of(Key::Ee, "Approve."), "approve");
    assert_eq!(token_of(Key::Market, "ESCALATE"), "escalate");
    // A non-ASCII lookalike (Cyrillic а) is not folded into a token.
    let e = parse_review(Key::Ee, &review_with(Key::Ee, "\u{430}pprove, why"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("unknown verdict token"), "{e}");
}

#[test]
fn parse_review_still_rejects_non_tokens() {
    for (key, verdict) in [
        (Key::Ee, "none"),
        (Key::Ee, "approved"),
        (Key::Ee, "approves"),
        (Key::Ee, "approve?"),
        (Key::Ee, "approve-ish"),
        (Key::Ee, "**approve"),
        (Key::Ee, "approve**"),
        (Key::Ee, "."),
        (Key::Ee, "**"),
        (Key::Ee, "*"),
        (Key::Ee, "__"),
        (Key::Ee, "`"),
        (Key::Ee, "competitive."),
        (Key::Market, "approve."),
        (Key::Market, "adequate for catalog"),
    ] {
        assert!(
            matches!(parse_review(key, &review_with(key, verdict)), Err(ReviewError::Invalid(_))),
            "{verdict:?} on {key:?} must be withheld as invalid"
        );
    }
}

#[test]
fn parse_review_conflict_uses_normalized_tokens() {
    let same = "## EE-key review: b\n**Overall verdict:** approve.\n**Overall verdict:** approve\n";
    assert_eq!(parse_review(Key::Ee, same).unwrap().token, "approve");
    let differ = "## EE-key review: b\n**Overall verdict:** approve.\n**Overall verdict:** **request-changes**\n";
    let e = parse_review(Key::Ee, differ).unwrap_err().to_string();
    assert!(e.contains("conflicting"), "{e}");
    // The reason keeps both raw strings for diagnosis.
    assert!(e.contains("\"approve.\"") && e.contains("\"**request-changes**\""), "{e}");
}

#[test]
fn measured_14956_tokens_now_parse() {
    for (key, verdict, token) in [
        (Key::Ee, "request-changes.", "request-changes"),
        (Key::Market, "adequate-for-catalog.", "adequate-for-catalog"),
        (Key::Market, "escalate.", "escalate"),
        (Key::Ee, "approve.", "approve"),
        (Key::Market, "`escalate`.", "escalate"),
        (Key::Market, "**escalate**.", "escalate"),
    ] {
        assert_eq!(token_of(key, &format!("{verdict} One sentence why.")), token);
    }
}

#[test]
fn parse_review_accepts_harmless_verdict_label_variants() {
    for label in [
        "**Overall verdict**:",
        "**Overall Verdict:**",
        "**OVERALL VERDICT:**",
        "Overall verdict:",
        "  **overall verdict:**",
    ] {
        let out = format!("## EE-key review: b\n{label} approve, why\n");
        assert_eq!(parse_review(Key::Ee, &out).unwrap().token, "approve", "{label}");
    }
    for not_a_label in [
        "*Overall verdict:*",
        "Overall verdict",
        "**Overall verdict**",
        "- **Overall verdict:**",
    ] {
        let out = format!("## EE-key review: b\n{not_a_label} approve, why\n");
        let e = parse_review(Key::Ee, &out).unwrap_err().to_string();
        assert!(e.contains("no **Overall verdict:** line"), "{not_a_label}: {e}");
    }
}

#[test]
fn parse_review_withholds_an_ambiguous_first_clause() {
    for verdict in [
        "approve or request-changes, depending on CI",
        "approve / request-changes.",
        "Approve OR Request-Changes",
        "**approve** or **request-changes**",
    ] {
        let e = parse_review(Key::Ee, &review_with(Key::Ee, verdict)).unwrap_err();
        assert!(
            matches!(e, ReviewError::Invalid(ref r) if r.contains("ambiguous")),
            "{verdict}: {e}"
        );
    }
    assert!(matches!(
        parse_review(Key::Ee, &review_with(Key::Ee, "approve | request-changes, why")),
        Err(ReviewError::Placeholder(_))
    ));
    // The sentence of reasoning after the first clause is not part of it.
    for (key, verdict, token) in [
        (Key::Ee, "approve, though request-changes was considered", "approve"),
        (Key::Ee, "approve — no request-changes needed", "approve"),
        (Key::Ee, "approve — the margin is > 3 dB and < 5 dB", "approve"),
        (Key::Ee, "approve. Delay < 5 ns.", "approve"),
        (Key::Market, "competitive. Not uncompetitive at all.", "competitive"),
        (Key::Market, "uncompetitive (competitive parts exist)", "uncompetitive"),
    ] {
        assert_eq!(token_of(key, verdict), token, "{verdict}");
    }
}

#[test]
fn parse_review_classifies_template_echo_as_placeholder() {
    for (key, template) in [(Key::Ee, EE_TEMPLATE), (Key::Market, MARKET_TEMPLATE)] {
        assert!(
            matches!(parse_review(key, template), Err(ReviewError::Placeholder(_))),
            "{key:?}: {:?}",
            parse_review(key, template)
        );
    }
}

#[test]
fn placeholder_survives_formatting() {
    for verdict in [
        "`<approve|request-changes>`",
        "**<approve|request-changes>**",
        "<approve|request-changes>.",
        "approve|request-changes",
        "<approve>",
    ] {
        let got = parse_review(Key::Ee, &review_with(Key::Ee, &format!("{verdict}, why")));
        assert!(matches!(got, Err(ReviewError::Placeholder(_))), "{verdict}: {got:?}");
    }
}

#[test]
fn placeholder_heading_with_filled_verdict_is_placeholder() {
    let out = "## EE-key review: <block name / class>\n**Overall verdict:** approve, fine\n";
    let got = parse_review(Key::Ee, out);
    assert!(
        matches!(got, Err(ReviewError::Placeholder(ref r)) if r.contains("heading")),
        "{got:?}"
    );
}

#[test]
fn placeholder_takes_precedence_over_a_valid_line() {
    let out = "## EE-key review: b\n**Overall verdict:** approve, x\n\
               **Overall verdict:** <approve|request-changes>, one sentence why.\n";
    assert!(matches!(parse_review(Key::Ee, out), Err(ReviewError::Placeholder(_))));
}

#[test]
fn a_template_before_the_real_review_does_not_make_it_a_placeholder() {
    // Only the review block (from the last heading) is read.
    for key in [Key::Ee, Key::Market] {
        let template = if key == Key::Ee {
            EE_TEMPLATE
        } else {
            MARKET_TEMPLATE
        };
        let out = format!("{template}\n{}", valid_review(key));
        assert!(parse_review(key, &out).is_ok(), "{key:?}");
    }
}

#[test]
fn placeholder_withheld_counts_in_its_own_bucket() {
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let mut r = KeyedRunner {
        ee: EE_TEMPLATE.into(),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    let results = run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let o = TickOutcome::from_run(&results, 0);
    assert_buckets(&o, &[("ee", "verdict_placeholder"), ("market", "posted")]);
    assert_eq!((o.posted, o.withheld), (1, 1));
    assert_eq!(keys_marked(&comments(&sink), HEAD), vec![Key::Market]);
}

#[test]
fn trailing_stop_review_posts_marker_with_raw_text() {
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let mut r = KeyedRunner {
        ee: "## EE-key review: x\n**Overall verdict:** request-changes.\n".into(),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let posted = sink.posted.borrow();
    let ee: Vec<_> = posted
        .iter()
        .filter(|(_, b)| b.starts_with("<!-- ratify:key=ee "))
        .collect();
    assert_eq!(ee.len(), 1, "{posted:?}");
    assert!(ee[0].1.contains("**Overall verdict:** request-changes."), "{}", ee[0].1);
}

fn page(range: std::ops::Range<u32>) -> String {
    serde_json::to_string(&range.map(|n| json!({"number": n})).collect::<Vec<_>>()).unwrap()
}

#[test]
fn paginated_listing_is_flattened_across_pages() {
    use crate::comment_trust::parse_listing;
    let two = format!("{}{}", page(0..100), page(100..101));
    assert_eq!(parse_listing(two.as_bytes()).unwrap().len(), 101);
    assert_eq!(parse_listing(page(0..100).as_bytes()).unwrap().len(), 100);
    let with_empty = format!("{}[]", page(0..100));
    assert_eq!(parse_listing(with_empty.as_bytes()).unwrap().len(), 100);
}

#[test]
fn unreadable_second_page_fails_closed() {
    use crate::comment_trust::parse_listing;
    let torn = format!("{}{{\"message\":\"boom\"}}", page(0..100));
    assert!(parse_listing(torn.as_bytes()).is_none());
    assert!(parse_listing(b"").is_none());
}

// ---- #14797: a completed key is not rerun on the same head ------------------

/// A runner whose output depends on the key that ran last.
struct KeyedRunner {
    ee: String,
    market: String,
    calls: Vec<String>,
}

impl RoleInvocationRunner for KeyedRunner {
    fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
        self.calls.push(role.to_string());
        RoleTickOutcome::Success
    }
    fn last_output(&self) -> Option<String> {
        Some(match self.calls.last() {
            Some(r) if r == "ratification-market" => self.market.clone(),
            _ => self.ee.clone(),
        })
    }
}

/// An instruction source that counts fetches (by pin path) and visibility
/// reads, so a partial-key retry can prove it reads only the missing key's
/// instructions and does market-only visibility work only when market runs.
#[derive(Default)]
struct CountingInstr {
    fetches: RefCell<Vec<String>>,
    visibility: RefCell<u32>,
    private: Option<bool>,
}

impl InstructionSource for CountingInstr {
    fn fetch(&self, pin: &crate::role_runner::ratification_lane::Pin) -> Result<String, String> {
        self.fetches.borrow_mut().push(pin.path.clone());
        Ok("INSTR".into())
    }
    fn repo_is_private(&self, _repo: &str) -> Option<bool> {
        *self.visibility.borrow_mut() += 1;
        self.private
    }
}

#[test]
fn partial_key_retry_runs_and_posts_only_the_missing_key() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let instr = Instr { fail_market: false };
    // Tick 1: both keys missing; market ends in a valid verdict, ee declines.
    let mut r = KeyedRunner {
        ee: "declined without a verdict".into(),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee", "ratification-market"]);
    assert_eq!(sink.posted.borrow().len(), 1);
    assert!(sink.posted.borrow()[0].1.contains("ratify:key=market"));

    // Tick 2: the trusted market marker is visible on the same head, so only
    // the missing ee key may run again — not the completed market key.
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls.len(), 3, "only the missing key reruns; calls so far: {:?}", r.calls);
    assert_eq!(r.calls[2], "ratification-ee");
    assert_eq!(sink.posted.borrow().len(), 1, "no duplicate market marker");
}

#[test]
fn unreadable_forge_comment_rows_stop_dispatch() {
    use crate::comment_trust::TrustPolicy;
    use crate::forge_identity::{FleetLogins, Placement, RouteDecision};
    use crate::gh_invocation::cwd_route::{CwdAnswer, DeriveEnv};
    use crate::gh_invocation::{test_routing, test_support};
    use crate::role_runner::ratification_lane::{marker_body, run_lane_stats, GhPrSource};

    struct CommentReader<'a> {
        prs: Prs,
        forge: GhPrSource<'a>,
    }

    impl PrSource for CommentReader<'_> {
        fn open_prs(&self) -> Vec<OpenPr> {
            self.prs.open_prs()
        }
        fn files(
            &self,
            pr: u64,
            hold: &crate::champion_hold::ratification::Config,
        ) -> Vec<ChangedFile> {
            self.prs.files(pr, hold)
        }
        fn trusted_comments(&self, pr: u64) -> Result<Vec<Comment>, String> {
            self.forge.trusted_comments(pr)
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let reader = dir.path().join("reader");
    std::fs::create_dir(&reader).unwrap();
    let program = test_support::stub(
        dir.path(),
        "gh-comments",
        r#"test "$1" = api && test "$2" = 'repos/o/r/issues/7/comments?per_page=100' && test "$3" = --paginate || exit 64
cat "${0%/*}/reply"
test ! -f "${0%/*}/failed""#,
    );
    let _routing = test_routing::install_with_program(
        test_routing::TestRouting {
            decision: RouteDecision::Reader {
                dir: reader,
                app_id: "fixture-reader".into(),
                placement: Placement::Home,
            },
            env: DeriveEnv::default(),
            checkout: CwdAnswer::Unresolved,
        },
        program,
    );
    let cfg = cfg();
    let policy = TrustPolicy::new(FleetLogins::single("fleet-bot"), None, Vec::new());
    let source = CommentReader {
        prs: Prs {
            author: "alice",
            listed: 0.into(),
        },
        forge: GhPrSource::new(dir.path(), "o/r", &policy),
    };
    let marker =
        marker_body(Key::Market, HEAD, "ratification-market", Some(&valid_review(Key::Market)));
    let marked = json!([{
        "user":{"login":"fixture-owner","type":"User"}, "author_association":"OWNER", "body":marker
    }]);
    let ordinary = json!([
        {"user":{"login":"fixture-owner","type":"User"}, "author_association":"OWNER", "body":"ordinary discussion"},
        {"user":{"login":"unknown[bot]","type":"Bot"}, "author_association":"NONE", "body":marker},
        {"user":null, "author_association":"NONE", "body":marker}
    ]);
    let missing_body = json!([{
        "user":{"login":"fixture-owner","type":"User"}, "author_association":"OWNER", "body":null
    }]);
    let bare_row = json!([{"message":"incomplete comment record"}]);
    let run =
        |label, listing: String, failed: bool, expected_pins: &[&str], expected_unreadable| {
            std::fs::write(dir.path().join("reply"), listing).unwrap();
            let failure = dir.path().join("failed");
            if failed {
                std::fs::write(&failure, "").unwrap();
            }
            let instructions = CountingInstr {
                private: Some(true),
                ..Default::default()
            };
            let sink = Sink::default();
            let mut runner = ok_runner();
            let (_, _, unreadable) =
                run_lane_stats(&cfg, None, "o/r", &source, &instructions, &sink, &mut runner);
            if failed {
                std::fs::remove_file(failure).unwrap();
            }
            assert_eq!(unreadable, expected_unreadable, "{label}: unreadable PRs");
            assert_eq!(*instructions.fetches.borrow(), expected_pins, "{label}: instructions");
            assert_eq!(
                *instructions.visibility.borrow(),
                u32::from(expected_pins.contains(&"market.md")),
                "{label}: visibility reads"
            );
            assert_eq!(runner.calls as usize, expected_pins.len(), "{label}: sessions");
            assert_eq!(sink.posted.borrow().len(), expected_pins.len(), "{label}: marker posts");
        };
    let both = ["ee.md", "market.md"];
    run("empty listing", "[]".into(), false, &both, 0);
    run("readable ordinary and untrusted rows", ordinary.to_string(), false, &both, 0);
    run("trusted marker", marked.to_string(), false, &["ee.md"], 0);
    run("failed read", marked.to_string(), true, &[], 1);
    run("invalid later page", format!("{marked}\n[{{"), false, &[], 1);
    run("malformed row", bare_row.to_string(), false, &[], 1);
    run("trusted row without a readable body", missing_body.to_string(), false, &[], 1);
    run(
        "valid marker before malformed row",
        format!("{marked}\n{missing_body}"),
        false,
        &[],
        1,
    );
}

#[test]
fn partial_key_retry_reads_only_the_missing_keys_instructions() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let instr = CountingInstr {
        private: Some(false),
        ..Default::default()
    };
    let mut r = KeyedRunner {
        ee: "declined without a verdict".into(),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    let run = |r: &mut KeyedRunner| {
        run_lane(&cfg(), None, "o/r", &live, &instr, &sink, r);
    };
    // Tick 1: both keys missing — both instructions read, EE first.
    run(&mut r);
    assert_eq!(*instr.fetches.borrow(), vec!["ee.md", "market.md"]);
    assert_eq!(*instr.visibility.borrow(), 1, "one market visibility read");
    assert_eq!(sink.posted.borrow().len(), 1);

    // Tick 2: market complete, ee still declining. One missing-key
    // instruction fetch, one role call, no new marker, no visibility read.
    run(&mut r);
    assert_eq!(*instr.fetches.borrow(), vec!["ee.md", "market.md", "ee.md"]);
    assert_eq!(r.calls.last().map(String::as_str), Some("ratification-ee"));
    assert_eq!(sink.posted.borrow().len(), 1);
    assert_eq!(*instr.visibility.borrow(), 1);

    // Tick 3: ee finally reviews — only its marker, at the captured head.
    r.ee = valid_review(Key::Ee);
    run(&mut r);
    assert_eq!(sink.posted.borrow().len(), 2);
    assert!(sink.posted.borrow()[1].1.contains("ratify:key=ee"));
    assert!(sink.posted.borrow()[1].1.contains(HEAD));

    // Tick 4: both keys complete — zero instruction reads, role calls, posts.
    let fetches_before = instr.fetches.borrow().len();
    let calls_before = r.calls.len();
    run(&mut r);
    assert_eq!(instr.fetches.borrow().len(), fetches_before);
    assert_eq!(r.calls.len(), calls_before);
    assert_eq!(sink.posted.borrow().len(), 2);
}

#[test]
fn partial_key_retry_works_in_both_directions() {
    // The other direction: ee completes first, market withholds and is then
    // retried alone; its session keeps the public-source constraint.
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let instr = CountingInstr {
        private: Some(false),
        ..Default::default()
    };
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: "declined without a verdict".into(),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(sink.posted.borrow().len(), 1);
    assert!(sink.posted.borrow()[0].1.contains("ratify:key=ee"));

    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(r.calls.len(), 3, "only market reruns: {:?}", r.calls);
    assert_eq!(r.calls[2], "ratification-market");
    assert_eq!(*instr.fetches.borrow(), vec!["ee.md", "market.md", "market.md"]);
    assert_eq!(*instr.visibility.borrow(), 2, "each market session reads visibility");
    assert_eq!(sink.posted.borrow().len(), 1, "no duplicate ee marker");
}

/// A forge whose comment read can fail, and whose trusted comments can be
/// seeded with raw records filtered through the real trust policy.
struct Feed {
    author: &'static str,
    head: std::cell::Cell<String>,
    /// Raw REST comment records, filtered through `TrustPolicy` on read.
    records: RefCell<Vec<serde_json::Value>>,
    fail_comments: Option<String>,
}

impl PrSource for Feed {
    fn open_prs(&self) -> Vec<OpenPr> {
        vec![OpenPr {
            number: 7,
            author: self.author.into(),
            head_sha: self.head.replace(String::new()),
        }]
    }
    fn files(
        &self,
        _pr: u64,
        _hold: &crate::champion_hold::ratification::Config,
    ) -> Vec<ChangedFile> {
        vec![ChangedFile {
            filename: "ratification/requests/a.md".into(),
            previous_filename: None,
            status: "added".into(),
            head_content: Some("Ratified only on operator merge.".into()),
        }]
    }
    fn trusted_comments(&self, _pr: u64) -> Result<Vec<Comment>, String> {
        if let Some(why) = &self.fail_comments {
            return Err(why.clone());
        }
        let policy = crate::comment_trust::TrustPolicy::new(
            crate::forge_identity::FleetLogins::single("fleet-bot"),
            None,
            Vec::new(),
        );
        Ok(self
            .records
            .borrow()
            .iter()
            .filter(|c| policy.trusts_json(c))
            .filter_map(Comment::from_record)
            .collect())
    }
}

impl Feed {
    fn new() -> Self {
        Feed {
            author: "alice",
            head: std::cell::Cell::new(HEAD.into()),
            records: RefCell::new(Vec::new()),
            fail_comments: None,
        }
    }

    /// A raw comment record as the REST listing reports it.
    fn record(login: &str, association: &str, body: &str) -> serde_json::Value {
        serde_json::json!({
            "user": {"login": login, "type": "Bot"},
            "author_association": association,
            "body": body,
        })
    }

    /// A hand-lane marker in the accepted wire format (#14797), optionally
    /// carrying a recorded review.
    fn seed_marker(
        &self,
        login: &str,
        association: &str,
        key: &str,
        sha: &str,
        verdict: Option<&str>,
    ) {
        let body = match verdict {
            Some(v) => format!(
                "<!-- ratify:key={key} sha={sha} -->\nreviewer=hand-run-lane\n\nVerdict:\n\n{v}\n"
            ),
            None => format!("<!-- ratify:key={key} sha={sha} -->\nreviewer=hand-run-lane\n"),
        };
        self.records
            .borrow_mut()
            .push(Self::record(login, association, &body));
    }
}

#[test]
fn a_trusted_hand_lane_marker_completes_only_its_key() {
    let feed = Feed::new();
    // A trusted fleet App's hand-run marker completes market — even though
    // its recorded review was negative (`escalate`).
    feed.seed_marker(
        "fleet-bot[bot]",
        "NONE",
        "market",
        HEAD,
        Some("## Market-key review: x\n\n**Overall verdict:** escalate, niche.\n"),
    );
    // An otherwise identical lookalike from an untrusted author is prose and
    // completes nothing: it goes through the actual trust filter above.
    feed.seed_marker("stranger[bot]", "NONE", "ee", HEAD, None);

    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    let results = run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee"], "only ee was missing");
    assert_eq!(sink.posted.borrow().len(), 1);
    assert!(sink.posted.borrow()[0].1.contains("ratify:key=ee"));
    // The completed key contributes neither a post nor a withheld count.
    assert_eq!(results[0].1.len(), 1);
    assert_eq!(results[0].1[0], (Key::Ee, KeyPost::Posted));
}

#[test]
fn unreadable_comment_list_runs_posts_and_counts_nothing() {
    let feed = Feed {
        fail_comments: Some("exit 1: HTTP 502".into()),
        ..Feed::new()
    };
    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = ok_runner();
    // A failed or malformed paginated read: zero instruction fetches, role
    // calls and posts, and the PR counts as unreadable (degraded), never as
    // "no markers" authorizing a duplicate run.
    let (results, skipped, unreadable) =
        run_lane_stats(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!((results.len(), skipped, unreadable), (0, 0, 1));
    assert_eq!(r.calls, 0);
    assert!(sink.posted.borrow().is_empty());
    assert!(instr.fetches.borrow().is_empty());
    let o = TickOutcome::from_run_stats(&results, skipped, unreadable);
    assert_eq!(o.reason, "degraded");
    assert_buckets(&o, &[]);
}

#[test]
fn a_verified_empty_comment_list_still_selects_both_keys() {
    // `Ok(vec![])` is a successfully read empty list, not an unreadable one.
    let feed = Feed::new();
    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee", "ratification-market"]);
    assert_eq!(sink.posted.borrow().len(), 2);
}

#[test]
fn a_moved_head_loses_both_keys_even_with_old_markers() {
    let feed = Feed::new();
    feed.seed_marker("fleet-bot[bot]", "NONE", "ee", HEAD, None);
    feed.seed_marker("fleet-bot[bot]", "NONE", "market", HEAD, None);
    let new_head = "f".repeat(40);
    feed.head.set(new_head.clone());

    let sink = Sink::default();
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls.len(), 2, "both keys are missing at the new head");
    assert_eq!(sink.posted.borrow().len(), 2);
    for (_, body) in sink.posted.borrow().iter() {
        assert!(body.contains(&new_head), "markers name the new head: {body}");
    }
}

#[test]
fn a_failed_post_for_the_missing_key_leaves_the_completed_key_alone() {
    /// A sink that refuses every post.
    struct Refusing;
    impl CommentSink for Refusing {
        fn post(&self, _pr: u64, _body: &str) -> Result<(), String> {
            Err("HTTP 403".into())
        }
    }

    let feed = Feed::new();
    feed.seed_marker("fleet-bot[bot]", "NONE", "market", HEAD, None);
    let sink = Refusing;
    let instr = CountingInstr::default();
    let mut r = KeyedRunner {
        ee: valid_review(Key::Ee),
        market: valid_review(Key::Market),
        calls: Vec::new(),
    };
    let results = run_lane(&cfg(), None, "o/r", &feed, &instr, &sink, &mut r);
    assert_eq!(r.calls, vec!["ratification-ee"]);
    match results[0].1.as_slice() {
        [(Key::Ee, KeyPost::Withheld(WithheldClass::PostFailed, reason))] => {
            assert!(reason.contains("post failed"), "{reason}")
        }
        other => panic!("expected the failed ee post, got {other:?}"),
    }
    // Withheld stays scoped to the attempted key: the completed market key
    // contributes nothing, and a later tick retries ee alone.
    let o = TickOutcome::from_run_stats(&results, 0, 0);
    assert_eq!((o.reason, o.selected, o.ran, o.posted, o.withheld), ("ran", 1, 1, 0, 1));
    assert_buckets(&o, &[("ee", "post_failed")]);
}

// ---- tick outcome + shared entry points (#13707) ----

use crate::role_runner::ratification_lane::{
    run_lane_counted, run_lane_stats, tick_span, KeyPost, TickOutcome, WithheldClass,
    STEP_TICK_INTERVAL,
};

fn assert_buckets(o: &TickOutcome, expected: &[(&str, &str)]) {
    let span =
        tick_span(std::path::Path::new("/fixture"), "role_tick", o, chrono::Utc::now()).bounded();
    let mut posted = 0;
    let mut withheld = 0;
    for key in ["ee", "market"] {
        for class in [
            "posted",
            "pre_launch_failed",
            "invocation_failed",
            "output_missing",
            "verdict_invalid",
            "verdict_placeholder",
            "post_failed",
        ] {
            let name = format!("loom.ratification.{key}.{class}");
            let expected = if expected.contains(&(key, class)) {
                "1"
            } else {
                "0"
            };
            let value = &span.attributes[&name];
            assert_eq!(value, expected, "{name}");
            let count: usize = value.parse().unwrap();
            if class == "posted" {
                posted += count;
            } else {
                withheld += count;
            }
        }
    }
    assert_eq!((posted, withheld), (o.posted, o.withheld));
}

/// A forge whose trusted comments are whatever the sink has posted so far.
struct Live<'a> {
    inner: Prs,
    sink: &'a Sink,
}

impl PrSource for Live<'_> {
    fn open_prs(&self) -> Vec<OpenPr> {
        self.inner.open_prs()
    }
    fn files(
        &self,
        pr: u64,
        hold: &crate::champion_hold::ratification::Config,
    ) -> Vec<ChangedFile> {
        self.inner.files(pr, hold)
    }
    fn trusted_comments(&self, _pr: u64) -> Result<Vec<Comment>, String> {
        Ok(comments(self.sink))
    }
}

#[test]
fn outcome_counts_selected_ran_posted() {
    let (prs, sink, mut r) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
        ok_runner(),
    );
    let (results, skipped) =
        run_lane_counted(&cfg(), None, "o/r", &prs, &Instr { fail_market: false }, &sink, &mut r);
    let o = TickOutcome::from_run(&results, skipped);
    assert_eq!((o.reason, o.selected, o.skipped, o.ran, o.posted), ("ran", 1, 0, 1, 2));
    assert!(results[0].1.iter().all(|(_, p)| *p == KeyPost::Posted));
}

#[test]
fn nothing_to_do_is_a_distinct_outcome() {
    let o = TickOutcome::from_run(&[], 0);
    assert_eq!((o.reason, o.selected, o.ran, o.posted), ("nothing_to_do", 0, 0, 0));
}

#[test]
fn second_pass_over_a_marked_head_posts_nothing() {
    let sink = Sink::default();
    let live = Live {
        inner: Prs {
            author: "alice",
            listed: 0.into(),
        },
        sink: &sink,
    };
    let mut r = ok_runner();
    let instr = Instr { fail_market: false };
    run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(sink.posted.borrow().len(), 2);
    let results = run_lane(&cfg(), None, "o/r", &live, &instr, &sink, &mut r);
    assert_eq!(sink.posted.borrow().len(), 2, "no double-posting of keys");
    assert_eq!(r.calls, 2, "no second reviewer dispatch");
    assert_buckets(&TickOutcome::from_run(&results, 0), &[]);
}

#[test]
fn tick_span_carries_reason_and_counts() {
    let o = TickOutcome::from_run(&[], 3);
    let span = tick_span(std::path::Path::new("/r"), "step_dispatch", &o, chrono::Utc::now());
    assert_eq!(span.name.as_str(), "ratification_lane.tick");
    // Assert through `.bounded()`: the export allowlist drops unregistered keys.
    let span = span.bounded();
    for (k, v) in [
        ("loom.ratification.reason", "nothing_to_do"),
        ("loom.ratification.source", "step_dispatch"),
        ("loom.ratification.selected", "0"),
        ("loom.ratification.skipped", "3"),
        ("loom.ratification.ran", "0"),
        ("loom.ratification.posted", "0"),
        ("loom.ratification.unreadable", "0"),
        ("loom.ratification.withheld", "0"),
    ] {
        assert_eq!(span.attributes.get(k).map(String::as_str), Some(v), "{k}");
    }
}

#[test]
fn disabled_tick_is_recorded_with_its_reason() {
    // The temp root has no config: the lane is disabled and says so.
    let dir = std::env::temp_dir().join(format!("ratif-tick-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut r = ok_runner();
    let o = crate::role_runner::ratification_lane::tick_for_root(&dir, &mut r);
    assert_eq!((o.reason, r.calls), ("disabled", 0));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn step_cadence_ticks_once_per_interval_per_root() {
    use std::time::{Duration, Instant};
    let mut last = std::collections::HashMap::new();
    let (a, b) = (std::path::Path::new("/a"), std::path::Path::new("/b"));
    let t0 = Instant::now();
    assert!(crate::role_runner::ratification_lane::step_tick_due_for_test(&mut last, a, t0));
    assert!(!crate::role_runner::ratification_lane::step_tick_due_for_test(
        &mut last,
        a,
        t0 + Duration::from_secs(1)
    ));
    assert!(crate::role_runner::ratification_lane::step_tick_due_for_test(&mut last, b, t0));
    assert!(crate::role_runner::ratification_lane::step_tick_due_for_test(
        &mut last,
        a,
        t0 + STEP_TICK_INTERVAL
    ));
}

#[test]
fn tick_with_unreadable_prs_is_degraded_not_nothing_to_do() {
    let o = TickOutcome::from_run_stats(&[], 0, 2);
    assert_eq!((o.reason, o.selected, o.unreadable), ("degraded", 0, 2));
    let span =
        tick_span(std::path::Path::new("/r"), "step_dispatch", &o, chrono::Utc::now()).bounded();
    assert_eq!(
        span.attributes
            .get("loom.ratification.unreadable")
            .map(String::as_str),
        Some("2")
    );
    assert_eq!(
        span.attributes
            .get("loom.ratification.reason")
            .map(String::as_str),
        Some("degraded")
    );
}

#[test]
fn withheld_keys_are_counted() {
    let results = vec![(
        5,
        vec![
            (
                Key::Ee,
                KeyPost::Withheld(WithheldClass::PreLaunchFailed, "could not resolve".into()),
            ),
            (Key::Market, KeyPost::Posted),
        ],
    )];
    let o = TickOutcome::from_run_stats(&results, 0, 0);
    assert_eq!((o.reason, o.posted, o.withheld), ("ran", 1, 1));
}

#[cfg(unix)]
mod gh_failure {
    use crate::cmd_out::{CmdOutcome, Unavailable};
    use crate::role_runner::ratification_dispatch::gh_failure_detail;
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    fn ran(code: i32, stderr: &str) -> CmdOutcome {
        CmdOutcome::Ran(std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        })
    }

    #[test]
    fn nonzero_exit_carries_status_and_stderr() {
        let d = gh_failure_detail(&ran(1, "gh: Not Found (HTTP 404)\n"));
        assert_eq!(d, "exit 1: gh: Not Found (HTTP 404)");
        assert_eq!(gh_failure_detail(&ran(4, "")), "exit 4, no stderr");
    }

    #[test]
    fn stderr_is_bounded_and_token_shaped_words_redacted() {
        let d = gh_failure_detail(&ran(1, &format!("bad ghp_SECRET123 {}", "x".repeat(500))));
        assert!(!d.contains("SECRET123"));
        assert!(d.contains("[redacted]"));
        assert!(d.ends_with("..."));
        assert!(d.chars().count() < 230);
    }

    #[test]
    fn spawn_failure_and_timeout_are_distinct_from_a_nonzero_exit() {
        let spawn = gh_failure_detail(&CmdOutcome::Unavailable(Unavailable::Spawn("no gh".into())));
        let timeout = gh_failure_detail(&CmdOutcome::Unavailable(Unavailable::TimedOut {
            after: Duration::from_secs(120),
            partial_stdout: Vec::new(),
        }));
        assert!(spawn.starts_with("could not start"));
        assert!(timeout.starts_with("timed out"));
        assert_ne!(spawn, timeout);
        assert!(!spawn.starts_with("exit"));
    }
}

// ---- #14834: the read-only status report -------------------------------------

use crate::role_runner::ratification_lane::{
    resolve_status, scan_markers, GhPrSource, MarkerComment, MarkerFormat,
};

/// The head both real PR #98 fixture markers name.
const DROP_HEAD: &str = "6a3cd2c43c31821fe93bbc18828c0eb6e06aa063";

/// The lane's own wire marker, as the daemon posts it (PR #98 shape).
fn lane_marker(key: &str) -> String {
    format!("<!-- ratify:key={key} sha={DROP_HEAD} -->\nreviewer=ratification-{key}\n")
}

/// The hand-applied marker, as observed on PR #98.
fn hand_marker(key: &str, verdict: &str) -> String {
    format!(
        "<!-- RATIFY-KEY: {key} verdict={verdict} block=o/r#98 \
sha={DROP_HEAD} reviewer=01a1fa5f-0873-7071-a338-184a562cfa55 date=2026-11-20T19:14:24Z -->"
    )
}

fn mc(id: u64, at: &str, body: &str) -> MarkerComment {
    MarkerComment {
        id,
        created_at: chrono::DateTime::parse_from_rfc3339(at)
            .unwrap()
            .with_timezone(&chrono::Utc),
        url: format!("https://example/pull/98#issuecomment-{id}"),
        body: body.into(),
    }
}

#[test]
fn status_picks_the_newest_marker_per_key_and_links_the_rest_once_as_superseded() {
    let comments = vec![
        mc(1, "2026-11-20T19:00:37Z", &lane_marker("market")),
        mc(2, "2026-11-20T19:05:00Z", &lane_marker("market")),
        mc(3, "2026-11-20T19:14:25Z", &hand_marker("market", "adequate-for-catalog")),
        mc(4, "2026-11-20T18:00:00Z", &lane_marker("ee")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    let market = &s.market;
    assert_eq!(market.effective.as_ref().unwrap().comment_id, 3);
    assert_eq!(market.effective.as_ref().unwrap().format, MarkerFormat::Hand);
    assert_eq!(
        market
            .superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![1, 2],
        "superseded links are exact and oldest first"
    );
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 4);
    assert!(s.ee.superseded.is_empty());

    // Reversing the fixture moves nothing: ordering is the forge's creation
    // time, never the listing's order.
    let mut reversed = comments.clone();
    reversed.reverse();
    assert_eq!(resolve_status(DROP_HEAD, "o/r", 98, &reversed), s);
}

#[test]
fn equal_creation_times_are_broken_by_comment_id_across_zone_formats() {
    // 20:00:00+01:00 is 19:00:00Z: same instant, different notation — the
    // numeric id decides.
    let comments = vec![
        mc(11, "2026-11-20T19:00:00Z", &lane_marker("ee")),
        mc(12, "2026-11-20T20:00:00+01:00", &lane_marker("ee")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 12);
    assert_eq!(
        s.ee.superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![11]
    );
}

#[test]
fn a_more_favorable_verdict_gives_a_comment_no_priority() {
    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &hand_marker("ee", "approve")),
        mc(2, "2026-11-20T19:30:00Z", &hand_marker("ee", "request-changes")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 2);
}

#[test]
fn both_marker_formats_compete_on_the_same_terms_in_either_direction() {
    let nwo = "o/r";
    // The real #98 pair: the lane marker came first, the hand marker later.
    let a = vec![
        mc(7048105117, "2026-11-20T19:00:37Z", &lane_marker("market")),
        mc(
            7048390167,
            "2026-11-20T19:14:25Z",
            &hand_marker("market", "adequate-for-catalog"),
        ),
    ];
    let s = resolve_status(DROP_HEAD, nwo, 98, &a);
    assert_eq!(s.market.effective.as_ref().unwrap().comment_id, 7048390167);
    assert_eq!(
        s.market
            .superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![7048105117]
    );
    // The mirror: a lane marker newer than a hand marker wins the same way.
    let b = vec![
        mc(1, "2026-11-20T19:00:37Z", &hand_marker("market", "competitive")),
        mc(2, "2026-11-20T19:14:25Z", &lane_marker("market")),
    ];
    let s = resolve_status(DROP_HEAD, nwo, 98, &b);
    assert_eq!(s.market.effective.as_ref().unwrap().format, MarkerFormat::Lane);
}

#[test]
fn stale_head_and_foreign_pr_markers_are_neither_effective_nor_superseded() {
    let stale = format!("<!-- ratify:key=ee sha={} -->", "f".repeat(40));
    let foreign_repo =
        format!("<!-- RATIFY-KEY: ee verdict=approve block=other/repo#98 sha={DROP_HEAD} -->");
    let foreign_pr =
        format!("<!-- RATIFY-KEY: ee verdict=approve block=o/r#99 sha={DROP_HEAD} -->");
    let own = format!("<!-- RATIFY-KEY: ee verdict=approve block=o/r#98 sha={DROP_HEAD} -->");
    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &stale),
        mc(2, "2026-11-20T19:01:00Z", &foreign_repo),
        mc(3, "2026-11-20T19:02:00Z", &foreign_pr),
        mc(4, "2026-11-20T19:03:00Z", &own),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 4);
    assert!(
        s.ee.superseded.is_empty(),
        "markers for another head or PR are not this head's markers"
    );
}

#[test]
fn quoted_and_fenced_lookalikes_never_establish_a_head_or_key() {
    let quoted = format!("> quoted example:\n> <!-- ratify:key=ee sha={DROP_HEAD} -->");
    let fenced = format!("```\n<!-- RATIFY-KEY: ee verdict=approve sha={DROP_HEAD} -->\n```");
    let lane_body = lane_marker("ee");
    let lane = lane_body.lines().next().unwrap();
    let hand = hand_marker("ee", "approve");
    let examples = [
        quoted,
        fenced,
        format!("Example: `{lane}`"),
        format!("`{hand}`"),
        format!("{lane} trailing prose"),
        format!("{hand} {lane}"),
    ];
    for example in &examples {
        assert!(scan_markers(example).is_empty(), "{example}");
    }
    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &lane_body),
        mc(2, "2026-11-20T19:01:00Z", &examples.join("\n")),
    ];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.unwrap().comment_id, 1);
    assert!(s.ee.superseded.is_empty());
    assert_eq!(scan_markers(&hand).len(), 1);
}

#[test]
fn indented_markdown_code_cannot_select_a_marker() {
    let lane_body = lane_marker("ee");
    let lane = lane_body.lines().next().unwrap();
    let hand = hand_marker("market", "approve");
    for marker in [lane, hand.as_str()] {
        assert_eq!(scan_markers(&format!("   {marker}")).len(), 1);
        for indent in ["    ", "     ", "\t", " \t", "   \t"] {
            assert!(scan_markers(&format!("{indent}{marker}")).is_empty());
        }
        assert_eq!(scan_markers(&format!("    ```\n\n{marker}")).len(), 1);
        assert!(scan_markers(&format!("```\n    ```\n{marker}\n```")).is_empty());
    }

    let comments = vec![
        mc(1, "2026-11-20T19:00:00Z", &format!("{lane}\n{hand}")),
        mc(2, "2026-11-20T19:01:00Z", &format!("Example:\n\n    {lane}\n\n\t{hand}")),
    ];
    let status = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    for key in [status.ee, status.market] {
        assert_eq!(key.effective.unwrap().comment_id, 1);
        assert!(key.superseded.is_empty());
    }
}

#[test]
fn fenced_examples_end_only_at_a_matching_markdown_close() {
    let lane_body = lane_marker("ee");
    let lane = lane_body.lines().next().unwrap();
    let hand = hand_marker("market", "approve");
    for (opening, example, closing) in [
        ("````text", "~~~~", "`````  "),
        ("````text", "```", "````"),
        ("~~~~text", "~~~~ ordinary text", "~~~~\t"),
        ("~~~text", "~~~\u{a0}", "~~~"),
        ("- ````text", "  ```", "  ````"),
    ] {
        let indent = if opening.starts_with('-') { "  " } else { "" };
        let inside = format!("{opening}\n{example}\n{indent}{lane}\n{indent}{hand}");
        assert!(scan_markers(&inside).is_empty(), "{opening} / {example}");
        let marks = scan_markers(&format!("{inside}\n{closing}\n\n{lane}\n{hand}"));
        assert_eq!(marks.len(), 2, "{opening} / {closing}");
        assert_eq!(marks[0].format, MarkerFormat::Lane);
        assert_eq!(marks[1].format, MarkerFormat::Hand);
    }
}

#[test]
fn markdown_context_does_not_create_or_hide_a_standalone_marker() {
    let hand = hand_marker("market", "approve");
    assert!(scan_markers(&format!("`inline example` {hand}")).is_empty());
    assert_eq!(
        scan_markers(&format!("Ordinary paragraph `\n{hand}\nNext paragraph `")).len(),
        1
    );
    assert_eq!(scan_markers(&format!("```invalid ` fence opener\n{hand}")).len(), 1);
}

#[test]
fn a_hand_marker_without_a_full_matching_sha_is_not_a_marker() {
    let short = format!("<!-- RATIFY-KEY: ee verdict=approve sha={} -->", "abc".repeat(7));
    let no_sha = "<!-- RATIFY-KEY: ee verdict=approve -->\n";
    assert!(scan_markers(&short).is_empty());
    assert!(scan_markers(no_sha).is_empty());
}

#[test]
fn a_missing_key_is_explicit_and_a_verified_empty_history_is_no_marker() {
    let empty = resolve_status(DROP_HEAD, "o/r", 98, &[]);
    assert!(empty.ee.effective.is_none() && empty.market.effective.is_none());
    let comments = vec![mc(1, "2026-11-20T19:00:00Z", &lane_marker("market"))];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert!(s.ee.effective.is_none(), "ee is missing at this head");
    assert!(s.market.effective.is_some());
}

#[test]
fn a_comment_carrying_several_markers_for_one_key_links_once() {
    let body = format!("{}\n{}", lane_marker("ee"), hand_marker("ee", "approve"));
    let comments = vec![mc(1, "2026-11-20T19:00:00Z", &body)];
    let s = resolve_status(DROP_HEAD, "o/r", 98, &comments);
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 1);
    assert!(s.ee.superseded.is_empty());
}

#[test]
fn status_reads_head_and_history_through_the_forge_and_performs_only_reads() {
    use crate::cli::ratification_lane_cmd::{handle, Action};
    use crate::comment_trust::TrustPolicy;
    use crate::forge_identity::{FleetLogins, Placement, RouteDecision};
    use crate::gh_invocation::cwd_route::{CwdAnswer, DeriveEnv};
    use crate::gh_invocation::{test_routing, test_support};

    let dir = tempfile::tempdir().unwrap();
    let reader = dir.path().join("reader");
    std::fs::create_dir(&reader).unwrap();
    let calls = dir.path().join("calls");
    let program = test_support::stub(
        dir.path(),
        "gh-status",
        &format!(
            r#"printf '%s\n' "$*" >> '{calls}'
test "$1" = api || exit 64
case "$2" in
  repos/o/r/pulls/98) test ! -f "${{0%/*}}/prfail" || exit 1; cat "${{0%/*}}/pr" ;;
  'repos/o/r/issues/98/comments?per_page=100') test "$3" = --paginate || exit 64; cat "${{0%/*}}/reply"; test ! -f "${{0%/*}}/failed" ;;
  *) exit 64 ;;
esac"#,
            calls = calls.display(),
        ),
    );
    let _routing = test_routing::install_with_program(
        test_routing::TestRouting {
            decision: RouteDecision::Reader {
                dir: reader,
                app_id: "fixture-reader".into(),
                placement: Placement::Home,
            },
            env: DeriveEnv::default(),
            checkout: CwdAnswer::Unresolved,
        },
        program,
    );
    let policy = TrustPolicy::new(FleetLogins::single("fleet-bot"), None, Vec::new());
    let source = GhPrSource::new(dir.path(), "o/r", &policy);

    let record = |id: u64, at: &str, login: &str, assoc: &str, r#type: &str, body: &str| {
        json!({
            "id": id,
            "created_at": at,
            "html_url": format!("https://example/o/r/pull/98#issuecomment-{id}"),
            "user": {"login": login, "type": r#type},
            "author_association": assoc,
            "body": body,
        })
    };
    let pr = json!({"number": 98, "head": {"sha": DROP_HEAD}, "base": {"sha": "0".repeat(40)}});

    // Two pages carrying several market AND ee markers. The newest market
    // marker (the hand marker) is on page one, the newest ee marker on page
    // two: page and listing position never decide, creation time does.
    let page_one = json!([
        record(3, "2026-11-20T19:14:25Z", "fixture-owner", "OWNER", "User", &hand_marker("market", "adequate-for-catalog")),
        { "id": 4, "created_at": "2026-11-20T19:20:00Z", "html_url": "u4",
          "user": {"login": "unknown[bot]", "type": "Bot"}, "author_association": "NONE",
          "body": lane_marker("market") },
        { "id": 5, "created_at": "2026-11-20T19:21:00Z", "html_url": "u5",
          "user": null, "author_association": "NONE", "body": lane_marker("ee") }
    ]);
    let page_two = json!([
        record(
            7048105117,
            "2026-11-20T19:00:37Z",
            "fixture-owner",
            "OWNER",
            "User",
            &lane_marker("market")
        ),
        record(
            7048105999,
            "2026-11-20T18:30:00Z",
            "fixture-owner",
            "OWNER",
            "User",
            &hand_marker("ee", "approve")
        ),
        record(
            7048106111,
            "2026-11-20T18:40:00Z",
            "fixture-owner",
            "OWNER",
            "User",
            &lane_marker("ee")
        )
    ]);
    std::fs::write(dir.path().join("pr"), pr.to_string()).unwrap();
    std::fs::write(dir.path().join("reply"), format!("{page_one}\n{page_two}")).unwrap();

    let head = source.pr_head(98).expect("head");
    assert_eq!(head, DROP_HEAD);
    let comments = source.marker_comments(98).expect("comments");
    // Untrusted and deleted-author rows are gone; trusted identity survives.
    assert_eq!(
        comments.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![3, 7048105117, 7048105999, 7048106111]
    );
    assert_eq!(comments[0].url, "https://example/o/r/pull/98#issuecomment-3");
    let s = resolve_status(&head, "o/r", 98, &comments);
    // Market: the hand marker on page one is newer than the lane marker on
    // page two — creation time, not page or listing position, decides.
    assert_eq!(s.market.effective.as_ref().unwrap().comment_id, 3);
    assert_eq!(
        s.market
            .superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![7048105117]
    );
    // EE: the newest ee marker is on page two and wins; the deleted-author
    // ee marker (id 5) never counted.
    assert_eq!(s.ee.effective.as_ref().unwrap().comment_id, 7048106111);
    assert_eq!(
        s.ee.superseded
            .iter()
            .map(|l| l.comment_id)
            .collect::<Vec<_>>(),
        vec![7048105999]
    );

    // A torn second page fails the whole read closed.
    std::fs::write(dir.path().join("reply"), format!("{page_one}\n[{{")).unwrap();
    assert!(source.marker_comments(98).is_err());

    // A verified empty listing reads as empty — both keys genuinely missing,
    // not unreadable.
    std::fs::write(dir.path().join("reply"), "[]").unwrap();
    let empty = source
        .marker_comments(98)
        .expect("empty history is readable");
    assert!(empty.is_empty());
    let s = resolve_status(&head, "o/r", 98, &empty);
    assert!(s.ee.effective.is_none() && s.market.effective.is_none());

    // A malformed trusted record (no id to link) fails the read closed.
    let malformed = json!([
        { "created_at": "2026-11-20T19:00:00Z", "html_url": "u9",
          "user": {"login": "fixture-owner", "type": "User"},
          "author_association": "OWNER", "body": lane_marker("market") }
    ]);
    std::fs::write(dir.path().join("reply"), malformed.to_string()).unwrap();
    let err = source.marker_comments(98).unwrap_err();
    assert!(err.contains("malformed"), "{err}");

    // A failed gh read of either endpoint is an error, never empty.
    std::fs::write(dir.path().join("reply"), format!("{page_one}\n{page_two}")).unwrap();
    std::fs::write(dir.path().join("failed"), "").unwrap();
    assert!(source.marker_comments(98).is_err());
    std::fs::remove_file(dir.path().join("failed")).unwrap();
    std::fs::write(dir.path().join("prfail"), "").unwrap();
    assert!(source.pr_head(98).is_err());
    std::fs::remove_file(dir.path().join("prfail")).unwrap();

    // A malformed captured head stops the actual status command before it
    // reads comment history, even when that history is readable.
    std::fs::write(dir.path().join("reply"), "[]").unwrap();
    for invalid in [
        "".to_string(),
        "a".repeat(39),
        "a".repeat(41),
        "g".repeat(40),
    ] {
        let pr = json!({"head": {"sha": invalid}});
        std::fs::write(dir.path().join("pr"), pr.to_string()).unwrap();
        let before = std::fs::read_to_string(&calls).unwrap();
        let error = handle(Action::Status {
            pr: 98,
            repo: Some("o/r".to_string()),
            root: Some(dir.path().to_path_buf()),
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("head could not be read") && error.contains("40-hex"), "{error}");
        let after = std::fs::read_to_string(&calls).unwrap();
        assert_eq!(after.strip_prefix(&before), Some("api repos/o/r/pulls/98\n"));
    }

    // Every forge call was a read: `gh api <endpoint>` with no method verb.
    let seen = std::fs::read_to_string(&calls).unwrap();
    for line in seen.lines() {
        assert!(line.starts_with("api "), "non-api call: {line}");
        assert!(!line.contains("-X") && !line.contains("--method"), "write call: {line}");
    }
}

// ---- #15315: an echoed prompt is cut out before parsing ---------------------

/// Pinned instructions that are the real templates (by pin path).
struct TemplateInstr;

impl InstructionSource for TemplateInstr {
    fn fetch(&self, pin: &crate::role_runner::ratification_lane::Pin) -> Result<String, String> {
        Ok(if pin.path == "ee.md" {
            EE_TEMPLATE
        } else {
            MARKET_TEMPLATE
        }
        .into())
    }
    fn repo_is_private(&self, _repo: &str) -> Option<bool> {
        Some(true)
    }
}

/// A Codex-like runner: its captured output carries the prompt it was given
/// (the `user` block on stderr) before or after what the model wrote.
struct EchoRunner {
    prompt: String,
    last_key: Option<Key>,
    /// What the model wrote, per key.
    wrote: fn(Key) -> String,
    echo_after: bool,
}

impl RoleInvocationRunner for EchoRunner {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        self.prompt = prompt.to_string();
        self.last_key = Some(if role == "ratification-market" {
            Key::Market
        } else {
            Key::Ee
        });
        RoleTickOutcome::Success
    }
    fn last_output(&self) -> Option<String> {
        let wrote = (self.wrote)(self.last_key.unwrap_or(Key::Ee));
        let echo = format!("user\n{}\n", self.prompt);
        Some(if self.echo_after {
            format!("{LAUNCHER_NOISE}{wrote}{echo}{WRAPPER_TAIL}")
        } else {
            format!("{LAUNCHER_NOISE}{echo}codex\n{wrote}{WRAPPER_TAIL}")
        })
    }
}

fn run_echo(wrote: fn(Key) -> String, echo_after: bool) -> (LaneResults, Vec<(u64, String)>) {
    let (prs, sink) = (
        Prs {
            author: "alice",
            listed: 0.into(),
        },
        Sink::default(),
    );
    let mut r = EchoRunner {
        prompt: String::new(),
        last_key: None,
        wrote,
        echo_after,
    };
    let results = run_lane(&cfg(), None, "o/r", &prs, &TemplateInstr, &sink, &mut r);
    let posted = sink.posted.borrow().clone();
    (results, posted)
}

use crate::role_runner::ratification_lane::LaneResults;

#[test]
fn echoed_prompt_then_review_posts_only_the_review() {
    for echo_after in [false, true] {
        let (results, posted) = run_echo(valid_review, echo_after);
        assert_eq!(posted.len(), 2, "echo_after={echo_after}: {results:?}");
        for (_, body) in &posted {
            assert!(!body.contains("<approve|request-changes>"), "{body}");
            assert!(!body.contains("<competitive|"), "{body}");
            assert!(!body.contains("Lane output contract"), "{body}");
            assert!(!body.contains("<block name / class>"), "{body}");
        }
    }
}

#[test]
fn review_then_echoed_prompt_still_posts_the_review() {
    // Critique A6: Codex stderr is tee'd live, so the echo can land after
    // the review; it must not turn a valid verdict into a withhold.
    let (results, posted) = run_echo(valid_review, true);
    assert_buckets(&TickOutcome::from_run(&results, 0), &[("ee", "posted"), ("market", "posted")]);
    assert_eq!(posted.len(), 2);
}

#[test]
fn echoed_prompt_without_review_is_no_heading() {
    fn prose(_: Key) -> String {
        "I could not finish the review.\n".into()
    }
    for echo_after in [false, true] {
        let (results, posted) = run_echo(prose, echo_after);
        assert!(posted.is_empty());
        assert_buckets(
            &TickOutcome::from_run(&results, 0),
            &[("ee", "verdict_invalid"), ("market", "verdict_invalid")],
        );
        match results[0].1.as_slice() {
            [(Key::Ee, KeyPost::Withheld(_, ee)), (Key::Market, KeyPost::Withheld(_, mk))] => {
                assert!(ee.contains("no ## EE-key review: heading"), "{ee}");
                assert!(mk.contains("no ## Market-key review: heading"), "{mk}");
            }
            other => panic!("{other:?}"),
        }
    }
}

// ---- #15315: a launch the child refused is pre-launch, through the real ----
// ---- classifier ------------------------------------------------------------

#[cfg(unix)]
mod launch_refusal {
    use super::*;
    use crate::observability::lifecycle::{RoleFailure, RoleTrace};

    const ANCHOR: &str = "2026-11-22T10:05:11.123Z";

    /// One tick's region of `role-ratification-ee.log` as the role runner
    /// writes it: its header line (carrying the anchor), then `body`.
    fn tick(body: &str) -> String {
        format!(
            "\n==== loom-daemon role_runner: {ANCHOR} role=ratification-ee model=default ====\n{body}\n"
        )
    }

    /// The EE session is a launched child that exited `code` with role log
    /// `log`; its trace carries the class the **production** classifier
    /// gives that log (critique A3: never a class injected by hand).
    struct LaunchedEe {
        log: String,
        failure: RoleFailure,
        last_ee: bool,
    }

    impl LaunchedEe {
        fn new(code: i32, body: &str) -> Self {
            let log = tick(body);
            let failure = crate::role_runner::launch::exited_failure_for_test(code, &log, ANCHOR);
            Self {
                log,
                failure,
                last_ee: false,
            }
        }
    }

    impl RoleInvocationRunner for LaunchedEe {
        fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
            self.last_ee = role == "ratification-ee";
            if self.last_ee {
                RoleTickOutcome::Failure("`.../spawn-worker.sh` exited with exit status: 78".into())
            } else {
                RoleTickOutcome::Success
            }
        }
        fn trace_context(&self) -> Option<RoleTrace> {
            self.last_ee.then(|| RoleTrace {
                context: crate::telemetry::trace::TraceContext::derived("fixture", &["ee"]),
                execution: "fixture-ee".into(),
                started_at: chrono::Utc::now(),
                failure: Some(self.failure.clone()),
                judge: None,
            })
        }
        fn last_output(&self) -> Option<String> {
            Some(if self.last_ee {
                self.log.clone()
            } else {
                valid_review(Key::Market)
            })
        }
    }

    fn ee_bucket(runner: &mut LaunchedEe) -> (TickOutcome, usize) {
        let sink = Sink::default();
        let results =
            run_lane(&cfg(), None, "o/r", &Feed::new(), &CountingInstr::default(), &sink, runner);
        let ee_posts = sink
            .posted
            .borrow()
            .iter()
            .filter(|(_, b)| b.starts_with("<!-- ratify:key=ee "))
            .count();
        (TickOutcome::from_run(&results, 0), ee_posts)
    }

    const REJECTED: &str = "# LOOM_LAUNCH_REJECTED code=78 stage=credential";

    #[test]
    fn launched_cli_never_started_failure_is_pre_launch() {
        // The loom#14944 shape: the launcher child refused the capped pool at
        // its credential stage and exited 78 before any CLI started.
        let mut r = LaunchedEe::new(78, &format!("spawn-worker: launching\n{REJECTED}"));
        assert_eq!(r.failure.class(), "preflight-no-cli-start", "the real classifier");
        let (o, ee_posts) = ee_bucket(&mut r);
        assert_buckets(&o, &[("ee", "pre_launch_failed"), ("market", "posted")]);
        assert_eq!(ee_posts, 0);

        // The classifier can name an account class for the same refusal
        // (the pool listing quotes exhaustion prose); the launcher's own
        // marker with no CLI start is still positive pre-launch evidence.
        let mut r = LaunchedEe::new(
            78,
            &format!("spawn-worker: account a: You've hit your usage limit\n{REJECTED}"),
        );
        assert!(
            !crate::role_runner::ratification_dispatch::LAUNCH_REFUSAL_CLASSES
                .contains(&r.failure.class()),
            "{}",
            r.failure.class()
        );
        let (o, _) = ee_bucket(&mut r);
        assert_buckets(&o, &[("ee", "pre_launch_failed"), ("market", "posted")]);
    }

    #[test]
    fn launched_real_failure_stays_invocation_failed() {
        for (code, body) in [
            (1, "# LOOM_CLI_START runtime=codex\nError: something codex said".to_string()),
            (78, "# LOOM_CLI_START runtime=opencode\nprovider refused".to_string()),
            (78, "# CLAUDE_CLI_START\n# ACCOUNT_POOL_EXHAUSTED".to_string()),
            // A rejection marker after the CLI started is not a launch refusal.
            (78, format!("# LOOM_CLI_START runtime=opencode\n{REJECTED}")),
        ] {
            let mut r = LaunchedEe::new(code, &body);
            assert!(
                !crate::role_runner::ratification_dispatch::LAUNCH_REFUSAL_CLASSES
                    .contains(&r.failure.class()),
                "{body}: {}",
                r.failure.class()
            );
            let (o, ee_posts) = ee_bucket(&mut r);
            assert_buckets(&o, &[("ee", "invocation_failed"), ("market", "posted")]);
            assert_eq!(ee_posts, 0);
        }
    }
}
