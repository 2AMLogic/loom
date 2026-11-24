use super::*;
use crate::comment_trust::TrustPolicy;
use crate::forge_identity::FleetLogins;
use crate::operator_decision::resolution::tests::body;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::Rc;

#[derive(Default)]
struct State {
    view: IssueView,
    /// Comments per issue: `(issue, comment)` pairs, oldest first.
    comments: Vec<(u64, Value)>,
    /// The body's edit history, any order (the fallback sorts it).
    history: Vec<BodyEdit>,
    /// Author JSON stamped on a comment this fake posts.
    poster: Value,
    log: Vec<String>,
}

struct Fake(Rc<RefCell<State>>);

impl ResolveForge for Fake {
    fn repo(&mut self) -> Result<String, String> {
        Ok("Acme/Widgets".into())
    }
    fn view(&mut self, _: u64) -> Result<IssueView, String> {
        Ok(self.0.borrow().view.clone())
    }
    fn comments(&mut self, issue: u64) -> Result<Vec<Value>, String> {
        Ok(self
            .0
            .borrow()
            .comments
            .iter()
            .filter(|(n, _)| *n == issue)
            .map(|(_, c)| c.clone())
            .collect())
    }
    fn body_history(&mut self, _: u64) -> Result<Vec<BodyEdit>, String> {
        let mut s = self.0.borrow_mut();
        s.log.push("history".into());
        Ok(s.history.clone())
    }
    fn post_comment(&mut self, issue: u64, b: &str) -> Result<(), String> {
        let mut s = self.0.borrow_mut();
        s.log.push("post".into());
        let mut c = s.poster.clone();
        c["body"] = json!(b);
        s.comments.push((issue, c));
        Ok(())
    }
    fn remove_label(&mut self, _: u64, l: &str) -> Result<(), String> {
        let mut s = self.0.borrow_mut();
        s.log.push(format!("remove:{l}"));
        s.view.labels.retain(|x| x != l);
        Ok(())
    }
}

fn operator() -> Value {
    json!({"user": {"login": "rjwalters", "type": "User"}, "author_association": "OWNER"})
}
fn bot() -> Value {
    json!({"user": {"login": "loom-fleet-dispatch[bot]", "type": "Bot"}, "author_association": "NONE"})
}

fn state(poster: Value) -> Rc<RefCell<State>> {
    Rc::new(RefCell::new(State {
        view: IssueView {
            title: "hermit: drop the legacy cache".into(),
            body: body("w"),
            labels: vec!["loom:hermit".into(), DECISION_LABEL.into()],
        },
        poster,
        ..State::default()
    }))
}

fn policy() -> TrustPolicy {
    TrustPolicy::new(FleetLogins::of(&crate::forge_identity::Roster::default()), None, Vec::new())
}

fn req(option: &str) -> ResolveRequest {
    ResolveRequest {
        issue: 7,
        option: option.into(),
        effect: Effect::Proceed,
        release: false,
        dry_run: false,
    }
}

struct Out {
    code: i32,
    out: String,
    err: String,
}

fn run(s: &Rc<RefCell<State>>, r: &ResolveRequest, confirm: &mut dyn FnMut(&str) -> bool) -> Out {
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let code = resolve(&mut Fake(s.clone()), &policy(), r, confirm, &mut o, &mut e);
    Out {
        code,
        out: String::from_utf8(o).unwrap(),
        err: String::from_utf8(e).unwrap(),
    }
}

fn yes(_: &str) -> bool {
    true
}

fn posts(s: &Rc<RefCell<State>>) -> usize {
    s.borrow().log.iter().filter(|l| *l == "post").count()
}

#[test]
fn dry_run_shows_the_review_and_touches_nothing() {
    let s = state(operator());
    let mut r = req("keep");
    r.dry_run = true;
    let o = run(&s, &r, &mut |_| panic!("a dry run asks nothing"));
    assert_eq!(o.code, 0, "{}", o.err);
    for k in [
        "OPTION=keep",
        "OPTION_LABEL=Option keep",
        "OPTION_WHY=w",
        "EFFECT=proceed",
        "DECISION_DIGEST=",
        "SCOPE_DIGEST=",
        "DRY_RUN=true",
    ] {
        assert!(o.out.contains(k), "{k} in {}", o.out);
    }
    assert!(s.borrow().log.is_empty());
}

#[test]
fn an_unconfirmed_review_records_nothing() {
    let s = state(operator());
    let o = run(&s, &req("keep"), &mut |_| false);
    assert_eq!(o.code, exit::REFUSED);
    assert!(s.borrow().log.is_empty());
}

#[test]
fn records_verifies_then_releases_in_that_order() {
    let s = state(operator());
    let mut r = req("keep");
    r.release = true;
    let mut asked = None;
    let o = run(&s, &r, &mut |e| {
        asked = Some(e.to_string());
        true
    });
    assert_eq!(o.code, 0, "{}", o.err);
    assert_eq!(asked.as_deref(), Some("keep"), "the operator confirms the option id");
    assert!(o.out.contains("RECORDED=true") && o.out.contains("VERIFIED=true"));
    assert!(o.out.contains("RELEASED=true"));
    assert_eq!(s.borrow().log, ["post", &format!("remove:{DECISION_LABEL}")]);
}

#[test]
fn a_retry_keeps_a_single_effective_receipt() {
    let s = state(operator());
    assert_eq!(run(&s, &req("keep"), &mut yes).code, 0);
    let again = run(&s, &req("keep"), &mut yes);
    assert_eq!(again.code, 0, "{}", again.err);
    assert!(again.out.contains("RECORDED=already"));
    assert_eq!(posts(&s), 1);
}

#[test]
fn a_different_standing_resolution_is_refused_not_stacked() {
    let s = state(operator());
    assert_eq!(run(&s, &req("keep"), &mut yes).code, 0);
    let o = run(&s, &req("drop"), &mut yes);
    assert_eq!(o.code, exit::REFUSED, "{}", o.err);
    assert_eq!(posts(&s), 1);
}

#[test]
fn a_scope_change_between_review_and_recording_is_refused() {
    let s = state(operator());
    let racer = s.clone();
    let o = run(&s, &req("keep"), &mut |_| {
        let mut st = racer.borrow_mut();
        st.view.body = st.view.body.replace("Remove it.", "Remove it and more.");
        true
    });
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("changed"), "{}", o.err);
    assert_eq!(posts(&s), 0);
}

#[test]
fn a_label_or_comment_change_between_review_and_recording_is_not_a_scope_change() {
    let s = state(operator());
    let racer = s.clone();
    let o = run(&s, &req("keep"), &mut |_| {
        let mut st = racer.borrow_mut();
        st.view.labels.push("loom:important".into());
        st.comments
            .push((7, json!({"body": "hello", "user": {"login": "x", "type": "User"}})));
        true
    });
    assert_eq!(o.code, 0, "{}", o.err);
}

#[test]
fn an_option_that_is_not_published_is_refused() {
    let s = state(operator());
    let o = run(&s, &req("ghost"), &mut yes);
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("keep, drop"), "{}", o.err);
    assert!(s.borrow().log.is_empty());
}

#[test]
fn an_unreadable_decision_is_refused_and_names_why() {
    let s = state(operator());
    s.borrow_mut().view.body = "just prose".into();
    let o = run(&s, &req("keep"), &mut yes);
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("decision"), "{}", o.err);
    // A block-less body may read the edit history (#15108); it records nothing.
    assert!(s.borrow().log.iter().all(|l| *l != "post"), "{:?}", s.borrow().log);
}

#[test]
fn a_posting_account_the_forge_does_not_call_an_operator_never_releases() {
    let s = state(bot());
    let mut r = req("keep");
    r.release = true;
    let o = run(&s, &r, &mut yes);
    assert_eq!(o.code, UNVERIFIED, "{}", o.err);
    assert!(s.borrow().log.iter().all(|l| !l.starts_with("remove:")), "{:?}", s.borrow().log);
    assert!(s.borrow().view.labels.iter().any(|l| l == DECISION_LABEL));
}

#[test]
fn release_is_only_for_a_proceed_resolution() {
    let s = state(operator());
    let mut r = req("drop");
    r.effect = Effect::Reject;
    r.release = true;
    assert_eq!(run(&s, &r, &mut yes).code, exit::USAGE);
    assert!(s.borrow().log.is_empty());
}

#[test]
fn a_non_proceed_resolution_is_recorded_without_release() {
    let s = state(operator());
    let mut r = req("drop");
    r.effect = Effect::Defer;
    let o = run(&s, &r, &mut yes);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(s.borrow().view.labels.iter().any(|l| l == DECISION_LABEL));
}

// --- #15108: the history fallback and the batch form ---

/// The issue body after an attended ruling replaced the decision block.
fn ruled_body() -> String {
    "## Operator ruling\n\nkeep: the design stands.\n".into()
}

fn edit(at: &str, login: &str, assoc: &str, body: String) -> BodyEdit {
    BodyEdit {
        body,
        edited_at: at.into(),
        editor: json!({"user": {"login": login, "type": "User"}, "author_association": assoc}),
    }
}

fn history_state(poster: Value) -> Rc<RefCell<State>> {
    let s = state(poster);
    s.borrow_mut().view.body = ruled_body();
    s.borrow_mut().history = vec![
        // Newest first, as the GraphQL `last:` window returns them: the
        // ruling edit's own snapshot has no block; the one before it does.
        edit("2026-11-23T20:09:12Z", "rjwalters", "OWNER", ruled_body()),
        edit("2026-11-23T17:44:00Z", "rjwalters", "OWNER", body("w")),
    ];
    s
}

#[test]
fn a_body_with_a_block_never_reads_the_edit_history() {
    let s = state(operator());
    assert_eq!(run(&s, &req("keep"), &mut yes).code, 0);
    assert!(!s.borrow().log.iter().any(|l| l == "history"), "{:?}", s.borrow().log);
}

#[test]
fn a_ruling_that_replaced_the_block_resolves_from_the_history() {
    let s = history_state(operator());
    let o = run(&s, &req("keep"), &mut yes);
    assert_eq!(o.code, 0, "{}", o.err);
    assert!(o.out.contains("RECORDED=true") && o.out.contains("VERIFIED=true"), "{}", o.out);
    // The digests bind to the exact text the operator reviewed: the version
    // that carried the block, not the ruling that replaced it.
    let cur = current("acme/widgets", 7, "hermit: drop the legacy cache", &body("w")).unwrap();
    assert!(
        o.out
            .contains(&format!("DECISION_DIGEST={}", cur.decision_digest)),
        "{}",
        o.out
    );
    assert!(
        o.out
            .contains(&format!("SCOPE_DIGEST={}", cur.scope_digest)),
        "{}",
        o.out
    );
}

#[test]
fn no_block_and_no_recoverable_history_refuses_naming_the_fix() {
    let s = state(operator());
    s.borrow_mut().view.body = "just a ruling, the block is gone".into();
    let o = run(&s, &req("keep"), &mut yes);
    assert_eq!(o.code, exit::REFUSED);
    assert!(
        o.err.contains("edit history") && o.err.contains("operator-decision apply"),
        "{}",
        o.err
    );
    assert!(s.borrow().log.iter().all(|l| *l != "post"));
}

#[test]
fn a_history_block_from_an_untrusted_editor_is_refused_and_named() {
    let s = history_state(operator());
    s.borrow_mut().history[1] = edit("2026-11-23T17:44:00Z", "rando", "NONE", body("w"));
    let o = run(&s, &req("keep"), &mut yes);
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("`rando`"), "{}", o.err);
    assert!(s.borrow().log.iter().all(|l| *l != "post"));
}

#[test]
fn a_body_that_still_carries_a_block_stays_authoritative() {
    let s = history_state(operator());
    // A malformed block in the body must NOT fall back to the history.
    s.borrow_mut().view.body = "```decision\nnot json\n```\n".into();
    let o = run(&s, &req("keep"), &mut yes);
    assert_eq!(o.code, exit::REFUSED);
    assert!(
        !s.borrow().log.iter().any(|l| l == "history"),
        "a present block must not read the history: {:?}",
        s.borrow().log
    );
}

fn batch(items: &[(u64, &str)], dry_run: bool) -> BatchRequest {
    BatchRequest {
        items: items
            .iter()
            .map(|(n, opt)| ResolveItem {
                repo: "acme/widgets".into(),
                issue: *n,
                option: (*opt).into(),
            })
            .collect(),
        effect: Effect::Proceed,
        release: false,
        dry_run,
    }
}

fn run_batch(
    s: &Rc<RefCell<State>>,
    req: &BatchRequest,
    confirm: &mut dyn FnMut(&str) -> bool,
) -> Out {
    let (mut o, mut e) = (Vec::new(), Vec::new());
    let policy = policy();
    let mut forge_for = |_: &str| -> Box<dyn ResolveForge> { Box::new(Fake(s.clone())) };
    let code = resolve_batch(&mut forge_for, &policy, req, confirm, &mut o, &mut e);
    Out {
        code,
        out: String::from_utf8(o).unwrap(),
        err: String::from_utf8(e).unwrap(),
    }
}

#[test]
fn a_batch_reviews_every_item_then_records_with_one_confirmation() {
    let s = state(operator());
    let r = batch(&[(7, "keep"), (8, "ghost"), (9, "drop")], false);
    let mut asked = None;
    let o = run_batch(&s, &r, &mut |e| {
        asked = Some(e.to_string());
        true
    });
    // The refused middle item does not stop the others.
    assert_eq!(o.code, exit::REFUSED);
    assert_eq!(
        asked.as_deref(),
        Some("resolve 2"),
        "one confirmation, naming the reviewed count"
    );
    assert!(o.out.contains("ITEM=1/3") && o.out.contains("ITEM=3/3"), "{}", o.out);
    assert!(
        o.out.contains("BATCH_ITEMS=3") && o.out.contains("BATCH_REVIEWED=2"),
        "{}",
        o.out
    );
    assert!(o.out.contains("BATCH_RECORDED=2"), "{}", o.out);
    assert_eq!(posts(&s), 2);
    assert!(o.err.contains("ghost"), "{}", o.err);
}

#[test]
fn a_batch_confirmation_a_single_option_id_cannot_satisfy() {
    let s = state(operator());
    let r = batch(&[(7, "keep")], false);
    // Even an option id literally spelled `resolve` cannot match: the batch
    // expects `resolve <count>`, and ids carry no spaces.
    let o = run_batch(&s, &r, &mut |e| {
        assert!(e.starts_with("resolve "));
        false
    });
    assert_eq!(o.code, exit::REFUSED);
    assert_eq!(posts(&s), 0);
}

#[test]
fn a_batch_dry_run_reviews_everything_and_records_nothing() {
    let s = state(operator());
    let r = batch(&[(7, "keep"), (8, "ghost")], true);
    let o = run_batch(&s, &r, &mut |_| panic!("a dry run asks nothing"));
    assert_eq!(o.code, exit::REFUSED, "the refused item still fails the run");
    assert!(
        o.out.contains("BATCH_REVIEWED=1") && o.out.contains("DRY_RUN=true"),
        "{}",
        o.out
    );
    assert!(s.borrow().log.iter().all(|l| *l != "post"), "{:?}", s.borrow().log);
}

#[test]
fn an_unconfirmed_batch_records_nothing() {
    let s = state(operator());
    let r = batch(&[(7, "keep"), (8, "keep")], false);
    let o = run_batch(&s, &r, &mut |_| false);
    assert_eq!(o.code, exit::REFUSED);
    assert!(o.err.contains("not confirmed"), "{}", o.err);
    assert_eq!(posts(&s), 0);
}

#[test]
fn parse_items_reads_lines_comments_and_blank_lines() {
    let items = parse_items(
        "# slate for 2026-11-23\n\nAcme/Widgets#7 = keep\nother/repo#12=drop\n# done\n",
    )
    .unwrap();
    assert_eq!(
        items,
        vec![
            ResolveItem {
                repo: "acme/widgets".into(),
                issue: 7,
                option: "keep".into()
            },
            ResolveItem {
                repo: "other/repo".into(),
                issue: 12,
                option: "drop".into()
            },
        ]
    );
    for bad in [
        "acme/widgets#7",
        "acme#7=keep",
        "acme/widgets#0=keep",
        "acme/widgets#seven=keep",
        "justtext=keep",
        "acme/widgets#7=",
        "#only comments\n\n",
    ] {
        assert!(parse_items(bad).is_err(), "{bad}");
    }
}
