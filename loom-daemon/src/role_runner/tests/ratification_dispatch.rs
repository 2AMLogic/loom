//! Tests for the ratification lane's two-session dispatch (#13189).

use super::*;
use crate::role_runner::ratification_dispatch::{
    dispatch, role_name, Dispatched, InstructionSource, KeyOutcome, PUBLIC_SOURCES_ONLY,
};
use crate::role_runner::ratification_lane::{Key, Pin};
use std::collections::HashMap;

const EE_SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const MKT_SHA: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const PR_REF: &str = "owner/repo#7@cccccccccccccccccccccccccccccccccccccccc";

fn pin(path: &str, commit: &str) -> Pin {
    Pin {
        repo: "keys/instructions".into(),
        path: path.into(),
        commit: commit.into(),
    }
}

struct Src {
    files: HashMap<String, Result<String, String>>,
    private: Option<bool>,
    /// Calls to `repo_is_private` (visibility is market-only work, #14797).
    visibility_reads: std::cell::Cell<u32>,
}

impl Src {
    fn ok(private: Option<bool>) -> Self {
        let mut files = HashMap::new();
        files.insert("ee.md".into(), Ok("EE-INSTRUCTIONS".into()));
        files.insert("market.md".into(), Ok("MARKET-INSTRUCTIONS".into()));
        Src {
            files,
            private,
            visibility_reads: std::cell::Cell::new(0),
        }
    }
}

impl InstructionSource for Src {
    fn fetch(&self, pin: &Pin) -> Result<String, String> {
        self.files
            .get(&pin.path)
            .cloned()
            .unwrap_or_else(|| Err("missing".into()))
    }
    fn repo_is_private(&self, _repo: &str) -> Option<bool> {
        self.visibility_reads.set(self.visibility_reads.get() + 1);
        self.private
    }
}

#[derive(Default)]
struct Recorder {
    calls: Vec<(String, String)>,
}

impl RoleInvocationRunner for Recorder {
    fn invoke(&mut self, role: &str, prompt: &str) -> RoleTickOutcome {
        self.calls.push((role.to_string(), prompt.to_string()));
        RoleTickOutcome::Success
    }
}

fn run(src: &Src, ee: &Pin, mkt: &Pin) -> (Dispatched, Recorder) {
    let mut r = Recorder::default();
    let d = dispatch(Some(ee), Some(mkt), "owner/repo", PR_REF, src, &mut r);
    (d, r)
}

#[test]
fn one_pr_yields_two_sessions_with_disjoint_context() {
    let src = Src::ok(Some(true));
    let (d, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert_eq!(r.calls.len(), 2);
    assert_eq!(r.calls[0].0, role_name(Key::Ee));
    assert_eq!(r.calls[1].0, role_name(Key::Market));
    assert_ne!(r.calls[0].0, r.calls[1].0);
    // Only the PR reference is shared; neither prompt carries the other's text.
    assert!(r.calls[0].1.contains("EE-INSTRUCTIONS") && r.calls[0].1.contains(PR_REF));
    assert!(r.calls[1].1.contains("MARKET-INSTRUCTIONS") && r.calls[1].1.contains(PR_REF));
    assert!(!r.calls[0].1.contains("MARKET-INSTRUCTIONS"));
    assert!(!r.calls[1].1.contains("EE-INSTRUCTIONS"));
    assert_eq!(d.get(Key::Ee), Some(&KeyOutcome::Ran(RoleTickOutcome::Success)));
    assert_eq!(d.get(Key::Market), Some(&KeyOutcome::Ran(RoleTickOutcome::Success)));
}

#[test]
fn branch_or_short_ref_pin_fails_only_that_session_closed() {
    let src = Src::ok(Some(true));
    for bad in ["main", "abc1234", ""] {
        let (d, r) = run(&src, &pin("ee.md", bad), &pin("market.md", MKT_SHA));
        assert!(matches!(d.ee, Some(KeyOutcome::FailedClosed(_))), "{bad:?}");
        assert!(matches!(d.market, Some(KeyOutcome::Ran(_))));
        assert_eq!(r.calls.len(), 1);
        assert_eq!(r.calls[0].0, role_name(Key::Market));
    }
}

#[test]
fn failed_or_empty_fetch_fails_closed_without_launching() {
    let mut src = Src::ok(Some(true));
    src.files.insert("market.md".into(), Err("HTTP 404".into()));
    let (d, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert!(matches!(d.market, Some(KeyOutcome::FailedClosed(_))));
    assert!(matches!(d.ee, Some(KeyOutcome::Ran(_))));
    assert_eq!(r.calls.len(), 1);

    src.files.insert("ee.md".into(), Ok("  \n".into()));
    src.files.insert("market.md".into(), Err("boom".into()));
    let (d, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert!(matches!(d.ee, Some(KeyOutcome::FailedClosed(_))));
    assert!(matches!(d.market, Some(KeyOutcome::FailedClosed(_))));
    assert!(r.calls.is_empty());
}

#[test]
fn public_or_unknown_visibility_constrains_market_only() {
    for private in [Some(false), None] {
        let src = Src::ok(private);
        let (_, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
        assert!(r.calls[1].1.contains(PUBLIC_SOURCES_ONLY), "{private:?}");
        assert!(!r.calls[0].1.contains(PUBLIC_SOURCES_ONLY));
    }
    let src = Src::ok(Some(true));
    let (_, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert!(!r.calls[1].1.contains(PUBLIC_SOURCES_ONLY));
}

// ---- #14797: only the selected key pins are dispatched -----------------------

#[test]
fn a_complete_key_pin_is_not_attempted_at_all() {
    let src = Src::ok(Some(true));
    let mut r = Recorder::default();
    // Market already complete for this head: only EE is dispatched.
    let d = dispatch(Some(&pin("ee.md", EE_SHA)), None, "owner/repo", PR_REF, &src, &mut r);
    assert_eq!(r.calls.len(), 1);
    assert_eq!(r.calls[0].0, role_name(Key::Ee));
    assert_eq!(d.get(Key::Ee), Some(&KeyOutcome::Ran(RoleTickOutcome::Success)));
    assert_eq!(d.market, None, "market was not attempted");
    assert!(d.market_output.is_none());
    // Visibility is market-only work: a market-less pass never reads it.
    assert_eq!(src.visibility_reads.get(), 0);

    // The other direction: only market dispatched.
    let mut r = Recorder::default();
    let d = dispatch(None, Some(&pin("market.md", MKT_SHA)), "owner/repo", PR_REF, &src, &mut r);
    assert_eq!(r.calls.len(), 1);
    assert_eq!(r.calls[0].0, role_name(Key::Market));
    assert_eq!(d.ee, None);
    assert_eq!(src.visibility_reads.get(), 1, "a market session reads visibility once");

    // Nothing selected: no fetch, no session, no visibility read.
    let mut r = Recorder::default();
    let d = dispatch(None, None, "owner/repo", PR_REF, &src, &mut r);
    assert!(r.calls.is_empty());
    assert_eq!((d.ee, d.market), (None, None));
    assert_eq!(src.visibility_reads.get(), 1);
}

#[test]
fn a_withheld_pin_failure_for_one_key_leaves_the_other_dispatched() {
    // A complete key plus a failed pin for the other: only the failed key is
    // recorded, as fail-closed, and nothing else runs.
    let mut src = Src::ok(Some(true));
    src.files.insert("ee.md".into(), Err("HTTP 404".into()));
    let mut r = Recorder::default();
    let d = dispatch(Some(&pin("ee.md", EE_SHA)), None, "owner/repo", PR_REF, &src, &mut r);
    assert!(matches!(d.ee, Some(KeyOutcome::FailedClosed(_))));
    assert_eq!(d.market, None);
    assert!(r.calls.is_empty());
}

/// Issue #13749: the production runner rejects a role name that
/// `resolve_and_admit` does not know, which the `Recorder` stub never
/// exercised. This runner applies the real admission to each role name the
/// lane dispatches.
struct AdmittingRunner {
    root: std::path::PathBuf,
    admitted: Vec<String>,
    rejected: Vec<String>,
}

impl RoleInvocationRunner for AdmittingRunner {
    fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
        match crate::runtime_admission::resolve_and_admit(&self.root, role, None) {
            Ok(_) => {
                self.admitted.push(role.to_string());
                RoleTickOutcome::Success
            }
            Err(e) => {
                self.rejected.push(role.to_string());
                RoleTickOutcome::RuntimeRejected(e)
            }
        }
    }
}

#[test]
fn lane_sessions_pass_real_runtime_admission() {
    let _env = crate::runtime_selection_test_support::ClearedRuntimeSelectionEnv::new();
    let d = tempfile::tempdir().unwrap();
    for sub in ["defaults/roles", "defaults/runtimes", "defaults/scripts"] {
        std::fs::create_dir_all(d.path().join(sub)).unwrap();
    }
    std::fs::write(d.path().join("defaults/roles/judge.json"), "{}").unwrap();
    std::fs::write(
        d.path().join("defaults/runtimes/claude.json"),
        r#"{"runtime":"claude","capabilities":{"mcp":"yes"}}"#,
    )
    .unwrap();
    let adapter = d.path().join("defaults/scripts/spawn-claude.sh");
    std::fs::write(&adapter, "#!/bin/sh\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&adapter, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let src = Src::ok(Some(true));
    let mut r = AdmittingRunner {
        root: d.path().to_path_buf(),
        admitted: vec![],
        rejected: vec![],
    };
    let out = dispatch(
        Some(&pin("ee.md", EE_SHA)),
        Some(&pin("market.md", MKT_SHA)),
        "owner/repo",
        PR_REF,
        &src,
        &mut r,
    );
    assert!(r.rejected.is_empty(), "rejected: {:?}", r.rejected);
    assert_eq!(r.admitted, vec![role_name(Key::Ee), role_name(Key::Market)]);
    assert_eq!(out.get(Key::Ee), Some(&KeyOutcome::Ran(RoleTickOutcome::Success)));
    assert_eq!(out.get(Key::Market), Some(&KeyOutcome::Ran(RoleTickOutcome::Success)));
}

// ---- #15315: Loom's output contract and the echoed prompt ---------------------

use super::ratification_markers::{EE_TEMPLATE, MARKET_TEMPLATE};
use crate::role_runner::ratification_dispatch::{build_prompt, lane_contract, strip_prompt_echo};
use crate::role_runner::ratification_lane::{parse_review, ReviewError};

const KEYS: [Key; 2] = [Key::Ee, Key::Market];

fn other(key: Key) -> Key {
    match key {
        Key::Ee => Key::Market,
        Key::Market => Key::Ee,
    }
}

#[test]
fn prompt_contract_is_last_and_names_exactly_the_allowed_tokens() {
    for key in KEYS {
        let contract = lane_contract(key);
        for token in key.allowed_tokens() {
            assert!(contract.contains(token), "{key:?} contract lacks {token}");
        }
        for token in other(key).allowed_tokens() {
            assert!(!contract.contains(token), "{key:?} contract names {token}");
        }
        for public in [false, true] {
            let prompt = build_prompt(key, "INSTR", PR_REF, public);
            assert!(prompt.trim_end().ends_with(contract.as_str()), "{key:?}/{public}");
            if public {
                let at = prompt.find(PUBLIC_SOURCES_ONLY).unwrap();
                assert!(at < prompt.find(&contract).unwrap(), "contract after public line");
            }
        }
    }
    // The real dispatch sends it last, after the public line on a public market repo.
    let src = Src::ok(Some(false));
    let (_, r) = run(&src, &pin("ee.md", EE_SHA), &pin("market.md", MKT_SHA));
    assert!(r.calls[0].1.trim_end().ends_with(&lane_contract(Key::Ee)));
    assert!(r.calls[1]
        .1
        .trim_end()
        .ends_with(&lane_contract(Key::Market)));
    assert!(r.calls[1].1.contains(PUBLIC_SOURCES_ONLY));
}

#[test]
fn lane_contract_never_carries_a_heading_or_a_verdict_line() {
    for key in KEYS {
        let contract = lane_contract(key);
        // Critique A4: the heading is a substring search target, so the
        // contract must not contain either key's heading literal at all.
        for k in KEYS {
            assert!(!contract.contains(k.review_heading()), "{key:?} has {k:?}'s heading");
        }
        assert!(!contract.contains(&other(key).review_heading()[3..]), "{key:?}");
        // Even placed inside a review block, it supplies no verdict line.
        let block = format!("{} fixture\n{contract}\n", key.review_heading());
        let e = parse_review(key, &block).unwrap_err().to_string();
        assert!(e.contains("no **Overall verdict:** line"), "{key:?}: {e}");
    }
}

#[test]
fn prompt_echo_never_parses_as_a_verdict() {
    for (key, template) in [(Key::Ee, EE_TEMPLATE), (Key::Market, MARKET_TEMPLATE)] {
        let before = format!("{}\n\nPR: {PR_REF}\n", template.trim_end());
        for public in [false, true] {
            let after = build_prompt(key, template, PR_REF, public);
            for prompt in [&before, &after] {
                assert!(
                    matches!(parse_review(key, prompt), Err(ReviewError::Placeholder(_))),
                    "{key:?}: {:?}",
                    parse_review(key, prompt)
                );
            }
        }
    }
}

#[test]
fn strip_prompt_echo_cuts_every_verbatim_echo_and_keeps_both_sides() {
    let prompt = build_prompt(Key::Ee, EE_TEMPLATE, PR_REF, false);
    let review = "## EE-key review: comparator\n**Overall verdict:** approve, fine.\n";
    for out in [
        format!("user\n{prompt}\n{review}"),
        format!("{review}user\n{prompt}"),
        format!("user\n{prompt}\n{review}user\n{prompt}"),
    ] {
        let cut = strip_prompt_echo(&out, &prompt);
        assert!(!cut.contains("<approve|request-changes>"), "{cut}");
        assert!(cut.contains(review.trim_end()), "{cut}");
        assert_eq!(parse_review(Key::Ee, &cut).unwrap().token, "approve");
    }
    // No echo: unchanged.
    assert_eq!(strip_prompt_echo(review, &prompt), review);
    assert_eq!(strip_prompt_echo(review, "  \n"), review);
}

#[test]
fn roll_interrupted_key_is_flagged_rolled() {
    use crate::observability::lifecycle::{RoleFailure, ROLL_INTERRUPTED};

    struct RolledEe {
        last_ee: bool,
    }
    impl RoleInvocationRunner for RolledEe {
        fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
            self.last_ee = role == role_name(Key::Ee);
            if self.last_ee {
                RoleTickOutcome::Failure("stopped by the version roll".into())
            } else {
                RoleTickOutcome::Failure("exit 1".into())
            }
        }
        fn unlaunched_failure(&self) -> Option<RoleFailure> {
            self.last_ee
                .then(|| RoleFailure::new(ROLL_INTERRUPTED, None, "fixture roll"))
        }
    }

    let src = Src::ok(Some(true));
    let mut r = RolledEe { last_ee: false };
    let d = dispatch(
        Some(&pin("ee.md", EE_SHA)),
        Some(&pin("market.md", MKT_SHA)),
        "owner/repo",
        PR_REF,
        &src,
        &mut r,
    );
    assert!(d.rolled(Key::Ee));
    assert!(!d.rolled(Key::Market));
    // Telemetry classification is unchanged: both still ran and failed.
    assert!(matches!(d.ee, Some(KeyOutcome::Ran(RoleTickOutcome::Failure(_)))));
    assert_eq!(
        d.failure(Key::Market),
        Some(&crate::role_runner::ratification_dispatch::AttemptFailure::Launched(None))
    );
}
