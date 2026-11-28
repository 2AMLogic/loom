use super::*;

const HEAD: &str = "abc1234";

fn file(name: &str, status: &str, content: Option<&str>) -> ChangedFile {
    ChangedFile {
        filename: name.into(),
        previous_filename: None,
        status: status.into(),
        head_content: content.map(str::to_string),
    }
}

fn held(files: &[ChangedFile]) -> bool {
    matches!(scan(files, &Config::default()), Verdict::Hold(_))
}

/// Minimal reconstructions of the 2026-10-20 shadow-run fixtures. The PRs'
/// contents live in private chip repos and are not copied here; each fixture
/// carries the ratification-by-merge wording its spec used, in the path layout
/// the repo used.
const FIXTURES: [(&str, &str, &str); 8] = [
    (
        "sky130-dsp-fft#6",
        "spec/decisions/0001-generator.md",
        "Status: Proposed\n\nThis record is **ratified on merge**.",
    ),
    (
        "sg13g2-noc#6",
        "spec/spec.md",
        "Ratified by the operator's approval of the PR that lands it.",
    ),
    (
        "sky130-droop-detector#8",
        "decisions/0003-threshold.md",
        "Merging this PR ratifies the threshold.",
    ),
    (
        "sky130-lvds-tx#9",
        "docs/decisions/0002-swing.md",
        "Ratified only on human merge approval.",
    ),
    (
        "sg13g2-dll#8",
        "spec/README.md",
        "Status: Ratified. Ratification happens on the operator's merge.",
    ),
    ("gf180-antifuse#61", "spec/antifuse.md", "ratified on\nmerge"),
    (
        "gf180-antifuse#92",
        "notes/adc-decision-record.md",
        "Merging this PR is the ratification.",
    ),
    ("gf180-sram#228", "spec/sram.md", "Ratified by the Operator."),
];

#[test]
fn every_2026_10_20_fixture_is_held() {
    for (pr, path, body) in FIXTURES {
        let v = scan(&[file(path, "added", Some(body))], &Config::default());
        assert_eq!(v, Verdict::Hold(vec![path.to_string()]), "{pr}");
    }
}

#[test]
fn a_pr_that_only_cites_a_spec_is_not_held() {
    // Touches no spec/decision file at all.
    let rtl = file("rtl/fft.v", "modified", Some("// see spec/spec.md: ratified on merge"));
    assert!(!held(&[rtl]));
    // Touches a spec file that carries no ratification clause.
    let plain = file("spec/spec.md", "modified", Some("The FFT is 256 points. Status: Proposed"));
    assert_eq!(scan(&[plain], &Config::default()), Verdict::Clear);
}

#[test]
fn a_ratification_request_file_is_held_like_a_spec_file() {
    let req = "ratification/requests/x.md";
    assert!(held(&[file(req, "added", Some("Ratified on merge."))]));
    // Path alone does not hold: the phrase test still applies.
    assert!(!held(&[file(req, "added", Some("Please review this request."))]));
    // Renamed out of ratification/**: still matched by its previous name.
    let mut moved = file("docs/overview.md", "renamed", Some("Ratified on merge."));
    moved.previous_filename = Some(req.into());
    assert!(held(&[moved]));
}

#[test]
fn edge_cases_follow_the_head_not_the_diff() {
    let declares = "Ratified on merge.";
    // Deleted: nothing at the head to ratify.
    assert!(!held(&[file("spec/spec.md", "removed", None)]));
    // A PR that removes the clause: head lacks the phrase.
    assert!(!held(&[file(
        "spec/spec.md",
        "modified",
        Some("Ratification is pending.")
    )]));
    // Renamed out of spec/**: still matched by its previous name, read at the new path.
    let mut moved = file("docs/overview.md", "renamed", Some(declares));
    moved.previous_filename = Some("spec/spec.md".into());
    assert!(held(&[moved]));
    // Renamed in from elsewhere: matched by the new name.
    assert!(held(&[file("spec/spec.md", "renamed", Some(declares))]));
    // Fail closed: a candidate that cannot be read is never Clear.
    assert_eq!(
        scan(&[file("spec/spec.md", "modified", None)], &Config::default()),
        Verdict::Unreadable(vec!["spec/spec.md".into()])
    );
    // A fenced example still holds: a false hold costs one comment.
    assert!(held(&[file(
        "spec/spec.md",
        "modified",
        Some("```\nratified on merge\n```")
    )]));
}

#[test]
fn a_bare_status_line_needs_a_ratification_clause() {
    let cfg = Config::default();
    assert!(!declares_ratification("Status: Proposed\n\nBody.", &cfg));
    assert!(!declares_ratification("Status: Ratified", &cfg));
    assert!(declares_ratification(
        "Status: Proposed\nThe operator ratifies this by approving the PR.",
        &cfg
    ));
}

#[test]
fn config_extends_paths_and_phrases_but_cannot_remove_built_ins() {
    let cfg = Config {
        paths: vec!["chips/*/design.md".into()],
        phrases: vec![r"sign-off\s+is\s+merge".into()],
    };
    // An explicit path alone does not hold: the phrase test still applies.
    let on_path = file("chips/fft/design.md", "added", Some("An ordinary design."));
    assert_eq!(scan(&[on_path], &cfg), Verdict::Clear);
    let declaring = file("chips/fft/design.md", "added", Some("Sign-off is merge."));
    assert!(matches!(scan(&[declaring], &cfg), Verdict::Hold(_)));
    // Built-ins stay, whatever the config says (additive only).
    assert!(declares_ratification("ratified on merge", &cfg));
    // An invalid extra regex degrades to a literal, never to a panic or a pass.
    let bad = Config {
        paths: vec![],
        phrases: vec!["(unclosed".into()],
    };
    assert!(declares_ratification("see (unclosed here", &bad));
    assert!(declares_ratification("ratified on merge", &bad));
    // From the effective config document.
    let doc = serde_json::json!({"champion": {"ratificationPaths": ["a/*.md"], "ratificationPhrases": ["x"]}});
    assert_eq!(
        Config::from_effective(&doc),
        Config {
            paths: vec!["a/*.md".into()],
            phrases: vec!["x".into()]
        }
    );
}

/// #15430: a proposed record that makes the two reviewer keys and the flip to
/// `Ratified` prerequisites of its own PR's merge, in no built-in phrase.
/// Minimal reconstructions of the wording fleet decision records use; their
/// contents live in private repos and are not copied.
const KEYS_BEFORE_MERGE: &str = "Status: proposed.\n\n## Ratification\n\nRatification of this record takes both independent reviewer keys (`ratification/ee-key/`, `ratification/market-key/`) on the PR that carries it, plus the flip of its status line to `RATIFIED` — both in that same PR, before merge.";
/// The same policy, stated merge first and with Markdown emphasis.
const KEYS_UNTIL_MERGED: &str = "**Status:** Proposed\n\nThis PR must not be merged until both reviewer keys are posted and the status line above reads **Ratified**: the two independent reviews are what ratify it.";
/// `KEYS_BEFORE_MERGE` with emphasis in place of its code ticks.
const KEYS_BEFORE_MERGE_EMPHASIZED: &str = "**Status:** proposed.\n\n## Ratification\n\n**Ratification** of this record takes **both independent reviewer keys** (`ratification/ee-key/`, `ratification/market-key/`) on the PR that carries it, plus the flip of its status line to **RATIFIED** — both in that same PR, **before merge**.";

/// Soft-wrap every paragraph of `text` to `width` columns, changing no words.
fn soft_wrapped(text: &str, width: usize) -> String {
    text.split("\n\n")
        .map(|paragraph| {
            let mut lines = vec![String::new()];
            for word in paragraph.split(' ') {
                let line = lines.last_mut().expect("starts with one line");
                if !line.is_empty() && line.len() + 1 + word.len() > width {
                    lines.push(String::new());
                }
                let line = lines.last_mut().expect("pushed above");
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[test]
fn a_record_requiring_the_reviewer_keys_before_its_merge_is_held() {
    let path = "spec/decision-records/DR-0009.md";
    let cfg = Config::default();
    for body in [
        KEYS_BEFORE_MERGE,
        KEYS_BEFORE_MERGE_EMPHASIZED,
        KEYS_UNTIL_MERGED,
    ] {
        let verdict = scan(&[file(path, "added", Some(body))], &cfg);
        assert_eq!(verdict, Verdict::Hold(vec![path.to_string()]), "{body}");
        assert_eq!(decide(&verdict, &[], HEAD), Action::Hold, "{body}");
    }
}

/// The policy reads the same however an editor soft-wraps it. The line-bound
/// clause check held or cleared these same words with the wrap width.
#[test]
fn soft_wrapping_does_not_change_the_verdict() {
    let path = "spec/decision-records/DR-0009.md";
    let cfg = Config::default();
    for policy in [
        KEYS_BEFORE_MERGE,
        KEYS_BEFORE_MERGE_EMPHASIZED,
        KEYS_UNTIL_MERGED,
    ] {
        for width in [100usize, 80, 72, 60, 40] {
            let body = soft_wrapped(policy, width);
            let verdict = scan(&[file(path, "added", Some(&body))], &cfg);
            assert_eq!(verdict, Verdict::Hold(vec![path.to_string()]), "{body}");
            assert_eq!(decide(&verdict, &[], HEAD), Action::Hold, "{body}");
        }
    }
}

#[test]
fn a_bare_status_or_an_ordinary_review_request_stays_clear() {
    let cfg = Config::default();
    for body in [
        "Status: Proposed",
        // An ordinary request for two code reviews.
        "**Status:** Proposed\n\nThis change needs two code reviews before it can be merged.",
        // `two reviews` on its own is not the reviewer pair.
        "Status: Proposed\n\nRatification is a separate step. This record needs two reviews before it can be merged.",
        // The pair, with no ratification word beside it.
        "Status: Proposed\n\nThis proposal needs two independent reviews before it is merged.",
    ] {
        let verdict = scan(&[file("spec/decision-records/DR-0009.md", "added", Some(body))], &cfg);
        assert_eq!(verdict, Verdict::Clear, "{body}");
        assert_eq!(decide(&verdict, &[], HEAD), Action::None, "{body}");
    }
}

fn hold_note(head: &str) -> Comment {
    Comment::new(
        "fleet[bot]",
        &format!("{HOLD_MARKER}\n<!-- champion:hold-state head={head} -->\nmerging ratifies; the operator approves"),
    )
}

const RELEASE: &str = "clear the hold \u{2014} I read the spec and approve it";

#[test]
fn a_trusted_persons_comment_releases_the_hold() {
    let c = [hold_note(HEAD), Comment::new("rjwalters", RELEASE)];
    assert!(release_due(&c));
    assert_eq!(decide(&Verdict::Hold(vec!["spec/a.md".into()]), &c, HEAD), Action::Release);
    // Leading courtesy and mention are tolerated.
    assert!(is_release_instruction("@champion please clear the hold \u{2014} ok"));
}

#[test]
fn a_bot_or_champion_comment_does_not_release_it() {
    let verdict = Verdict::Hold(vec!["spec/a.md".into()]);
    for who in [
        Comment::new("fleet[bot]", RELEASE),
        Comment::new("Loom-Bot[BOT]", RELEASE),
    ] {
        let c = [hold_note(HEAD), who];
        assert!(!release_due(&c));
        assert_eq!(decide(&verdict, &c, HEAD), Action::Stands);
    }
    // Champion's own markers are never a person's release, whoever posts them.
    let own = Comment::new("rjwalters", &format!("<!-- champion:note -->\n{RELEASE}"));
    assert!(!release_due(&[hold_note(HEAD), own]));
    // A release that predates the hold does not carry over.
    assert!(!release_due(&[Comment::new("rjwalters", RELEASE), hold_note(HEAD)]));
}

#[test]
fn only_an_explicit_hold_clear_releases() {
    for no in [
        "do not clear the hold yet",
        "merge anyway",
        "ok to merge",
        "can we clear the hold?",
        "looks good to me",
        "please do not clear the hold",
    ] {
        assert!(!is_release_instruction(no), "{no}");
    }
    for yes in [
        "clear the hold \u{2014} approved",
        "Clear the hold.",
        "lift this hold: fine",
        "override the hold",
    ] {
        assert!(is_release_instruction(yes), "{yes}");
    }
}

#[test]
fn a_hand_removed_label_is_not_a_release() {
    // `decide` takes no label input: with the notice standing and no person's
    // comment, whatever the label state, the answer is "stands" (the tick
    // re-asserts `loom:blocked`).
    let verdict = Verdict::Hold(vec!["spec/a.md".into()]);
    assert_eq!(decide(&verdict, &[hold_note(HEAD)], HEAD), Action::Stands);
}

#[test]
fn the_state_machine_walks_hold_release_rearm_clear() {
    let verdict = Verdict::Hold(vec!["spec/a.md".into()]);
    // Fresh.
    assert_eq!(decide(&verdict, &[], HEAD), Action::Hold);
    // A quote of the marker is prose, not state.
    let quote = Comment::new("x", &format!("see {HOLD_MARKER}"));
    assert_eq!(decide(&verdict, &[quote], HEAD), Action::Hold);
    // Released at this head: nothing more, and the merge may proceed.
    let released = Comment::new(
        "fleet[bot]",
        &format!("{RELEASED_MARKER}\n<!-- champion:hold-state head={HEAD} -->"),
    );
    let c = [
        hold_note(HEAD),
        Comment::new("rjwalters", RELEASE),
        released.clone(),
    ];
    assert_eq!(
        episode(&c),
        Episode::Released {
            head: Some(HEAD.into())
        }
    );
    assert_eq!(decide(&verdict, &c, HEAD), Action::None);
    // A push after the release that still declares it re-arms.
    assert_eq!(decide(&verdict, &c, "def5678"), Action::Hold);
    // The head stopped declaring it: close the episode, once.
    assert_eq!(decide(&Verdict::Clear, &[hold_note(HEAD)], HEAD), Action::Clear);
    let cleared = Comment::new("fleet[bot]", CLEARED_MARKER);
    assert_eq!(decide(&Verdict::Clear, &[hold_note(HEAD), cleared], HEAD), Action::None);
    assert_eq!(decide(&Verdict::Clear, &[], HEAD), Action::None);
}

/// #14833: `stands` is same-head only. A Doctor push that still declares
/// ratification moves the head; the next tick must re-arm with a fresh notice
/// recording the new head, or the newest `champion:hold-state` record stays at
/// the old head and an audited `merge-pr.sh --release-hold` refuses the
/// release forever.
#[test]
fn a_hold_at_an_older_head_re_arms_with_a_fresh_notice() {
    let verdict = Verdict::Hold(vec!["spec/a.md".into()]);
    let moved = "def5678def5678def5678def5678def5678";
    let c = [hold_note(HEAD)];
    assert_eq!(decide(&verdict, &c, HEAD), Action::Stands);
    assert_eq!(decide(&verdict, &c, moved), Action::Hold);
    // A legacy notice with no recorded head cannot prove the head either:
    // re-arm once so the fresh notice records it.
    let legacy = Comment::new("fleet[bot]", HOLD_MARKER);
    assert_eq!(decide(&verdict, &[legacy], HEAD), Action::Hold);
    // Once the re-arm's notice lands at the new head, a repeat tick is quiet
    // — exactly one notice per held head.
    let c2 = [hold_note(HEAD), hold_note(moved)];
    assert_eq!(decide(&verdict, &c2, moved), Action::Stands);
}

#[test]
fn an_unreadable_candidate_never_merges_and_never_drops_a_hold() {
    let unreadable = Verdict::Unreadable(vec!["spec/a.md".into()]);
    // Unreachable from the CLI (#13932 fails closed first); if one arrives
    // anyway it opens or keeps a hold, never a silent pass.
    assert_eq!(decide(&unreadable, &[], HEAD), Action::Hold);
    assert_eq!(decide(&unreadable, &[hold_note(HEAD)], HEAD), Action::Stands);
}

/// #13932: a candidate whose head content could not be read is named by
/// [`unreadable_candidates`] even when another file already holds — the verb
/// exits 3 on a non-empty list instead of printing a truncated `files=`
/// evidence line at rc=0. The old behaviour held on the readable hit and
/// silently dropped the unreadable path.
#[test]
fn an_unreadable_candidate_is_named_even_when_another_file_holds() {
    let cfg = Config::default();
    let hit = file("spec/spec.md", "modified", Some("Ratified on merge."));
    let unreadable = file("docs/decisions/0002.md", "added", None);
    // The old behaviour: hold on the readable hit, unreadable dropped.
    assert_eq!(
        scan(&[hit.clone(), unreadable.clone()], &cfg),
        Verdict::Hold(vec!["spec/spec.md".into()])
    );
    // The verb's fail-closed check: the unreadable candidate is named.
    assert_eq!(
        unreadable_candidates(&[hit, unreadable], &cfg),
        ["docs/decisions/0002.md".to_string()]
    );
    // A renamed file matched by its old name is still read (at the new path)
    // and still fail-closed when that read fails.
    let mut renamed = file("docs/overview.md", "renamed", None);
    renamed.previous_filename = Some("spec/spec.md".into());
    assert_eq!(unreadable_candidates(&[renamed], &cfg), ["docs/overview.md".to_string()]);
}

/// Every candidate readable → the fail-closed check stays empty and the
/// verdict is unchanged; a removed file is never read, a non-candidate is
/// never read, and zero candidates is simply `clear`.
#[test]
fn readable_removed_and_non_candidates_leave_the_unreadable_list_empty() {
    let cfg = Config::default();
    let hit = file("spec/spec.md", "modified", Some("Ratified on merge."));
    let plain = file("spec/other.md", "modified", Some("Nothing to hold on."));
    let removed = file("spec/gone.md", "removed", None);
    let non_candidate = file("rtl/fft.v", "modified", None);
    let files = [hit, plain, removed, non_candidate];
    assert_eq!(scan(&files, &cfg), Verdict::Hold(vec!["spec/spec.md".into()]));
    assert!(unreadable_candidates(&files, &cfg).is_empty());
    assert_eq!(scan(&[], &cfg), Verdict::Clear);
    assert!(unreadable_candidates(&[], &cfg).is_empty());
}
