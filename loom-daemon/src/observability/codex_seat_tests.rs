use super::*;

fn stamped(runtime: &str, account: Option<&str>) -> TraceAttributes {
    let mut a = TraceAttributes::new();
    stamp(&mut a, runtime, account);
    a
}

#[test]
fn a_codex_launch_names_its_seat() {
    assert_eq!(stamped("codex", Some("agent-2"))[KEY], "agent-2");
}

#[test]
fn only_codex_carries_the_seat() {
    for runtime in ["claude", "pi", "opencode", "aider", ""] {
        assert!(stamped(runtime, Some("agent-2")).is_empty(), "{runtime}");
    }
}

#[test]
fn an_invalid_or_empty_account_omits_the_key() {
    for account in [
        None,
        Some(""),
        Some("a b"),
        Some("/home/u/.codex"),
        Some("x;rm"),
        Some(&"a".repeat(129)[..]),
    ] {
        assert!(stamped("codex", account).is_empty(), "{account:?}");
    }
}

#[test]
fn the_key_is_in_the_export_allowlist() {
    assert!(crate::telemetry::ops::OPS_SPAN_ATTRIBUTE_KEYS.contains(&KEY));
    // And it survives the span journal's content filter.
    use crate::telemetry::trace::{journal::Journal, SpanName, SpanStatus, TraceContext};
    let dir = tempfile::tempdir().unwrap();
    let journal = Journal::for_context(&dir.path().join("root.json"));
    let root = TraceContext::root(true);
    let mut a = TraceAttributes::new();
    a.insert(KEY.into(), "agent-2".into());
    let active = journal
        .start(root.child(), Some(&root), SpanName::RuntimeRun, chrono::Utc::now(), a)
        .unwrap();
    journal
        .finish(&active, chrono::Utc::now(), SpanStatus::Ok, TraceAttributes::new())
        .unwrap();
    let mut spans = Vec::new();
    journal
        .drain(|record| {
            spans.push(record);
            Ok(())
        })
        .unwrap();
    assert_eq!(spans[0].attributes.get(KEY).map(String::as_str), Some("agent-2"));
}

#[test]
fn usage_inherits_only_an_agreed_seat() {
    let a = stamped("codex", Some("agent-2"));
    let b = stamped("codex", Some("agent-3"));
    let none = TraceAttributes::new();

    let mut usage = TraceAttributes::new();
    stamp_agreed(&mut usage, [&a, &a]);
    assert_eq!(usage[KEY], "agent-2");

    for disagreeing in [vec![&a, &b], vec![&a, &none], vec![&none, &a]] {
        let mut usage = TraceAttributes::new();
        stamp_agreed(&mut usage, disagreeing);
        assert!(usage.is_empty());
    }
    let mut usage = TraceAttributes::new();
    stamp_agreed(&mut usage, std::iter::empty());
    stamp_agreed(&mut usage, [&none]);
    assert!(usage.is_empty());
}
