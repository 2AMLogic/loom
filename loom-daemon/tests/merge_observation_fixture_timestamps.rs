//! Merge-observation fixtures must stamp `observed_at` from the clock (#15799).
//!
//! `run_pass` compares a stored record's `observed_at` with the real wall
//! clock and drops an incomplete record more than seven days old (#15145). A
//! fixture that pins that field to a calendar date passes on the day it is
//! written and fails everywhere at once once the date has aged out — on
//! `main` and on every PR branch, where no rerun can clear it (#15794). A
//! record reclaimed before the pass resumes it fails the other way: still
//! green, no longer exercising the path it exists for.
//!
//! This is therefore an allow-list, not a search for date-shaped strings (the
//! original fixtures used a named constant). Every `observed_at` in the two
//! fixture modules must be either a struct-field initializer of a shape
//! [`is_now_relative`] recognizes or a read of the field. Anything else fails,
//! whatever today's date is, naming the module, the line and the initializer.
//! Literals are scanned too: a record written as JSON with an `observed_at`
//! key is a pinned stamp as well.
//!
//! Out of scope, as in the issue: a production clock seam, the TTL itself and
//! a repository-wide timestamp lint. `merged_at` may stay pinned; nothing
//! compares it with the clock.

use std::fs;
use std::path::Path;

/// The modules that build a `MergeRecord` for `run_pass`, relative to the
/// crate root.
const FIXTURES: &[&str] = &[
    "src/claim_reconciliation/pass_loop/merge_observation_tests.rs",
    "src/claim_reconciliation/pass_loop/merge_observation_git_tests.rs",
];

const FIELD: &str = "observed_at";

/// How one occurrence of the field name is used.
enum Use {
    /// `observed_at: <initializer>` whose value comes from the clock.
    ClockInit,
    /// `observed_at: <initializer>` with any other value.
    PinnedInit,
    /// `record.observed_at`, read.
    Read,
    /// A shorthand initializer, a binding or parameter, a field write, or the
    /// name inside a string literal.
    NotAnInitializer,
}

/// One occurrence of the field name.
struct Occurrence {
    line: usize,
    /// The initializer, or the source line when the use is not an initializer.
    text: String,
    kind: Use,
}

impl Occurrence {
    fn offends(&self) -> bool {
        matches!(self.kind, Use::PinnedInit | Use::NotAnInitializer)
    }
}

fn fixture(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The index just past the string literal whose opening quote is `chars[quote]`.
fn string_end(chars: &[(usize, char)], quote: usize) -> usize {
    // `r"…"`, `r#"…"#`, `br#"…"#` end at a quote followed by as many `#` as
    // they opened with.
    let mut raw = quote;
    while raw > 0 && chars[raw - 1].1 == '#' {
        raw -= 1;
    }
    if raw > 0 && chars[raw - 1].1 == 'r' {
        let hashes = quote - raw;
        return (quote + 1..chars.len())
            .find(|&k| {
                chars[k].1 == '"'
                    && (0..hashes).all(|n| chars.get(k + 1 + n).is_some_and(|(_, c)| *c == '#'))
            })
            .map_or(chars.len(), |k| k + 1 + hashes);
    }
    let mut end = quote + 1;
    while end < chars.len() && chars[end].1 != '"' {
        end += if chars[end].1 == '\\' { 2 } else { 1 };
    }
    (end + 1).min(chars.len())
}

/// The index just past the character literal at `chars[at]`, or `at + 1` when
/// the quote opens a lifetime instead.
fn char_end(chars: &[(usize, char)], at: usize) -> usize {
    let peek = |n: usize| chars.get(at + n).map_or('\0', |(_, c)| *c);
    if peek(1) == '\\' {
        let mut end = at + 3;
        while end < chars.len() && chars[end].1 != '\'' {
            end += 1;
        }
        (end + 1).min(chars.len())
    } else if peek(2) == '\'' {
        at + 3
    } else {
        at + 1
    }
}

/// `src` with its comments blanked; newlines are kept, so positions and line
/// numbers are unchanged.
///
/// Literals are skipped rather than blanked, so their content stays visible
/// to the scan and a `//`, `/*` or quote inside one cannot open a comment.
fn without_comments(src: &str) -> String {
    let chars: Vec<(usize, char)> = src.char_indices().collect();
    let mut out = src.as_bytes().to_vec();
    let mut i = 0;
    while i < chars.len() {
        let (start, c) = chars[i];
        let next = chars.get(i + 1).map_or('\0', |(_, c)| *c);
        let (end, comment) = match c {
            '/' if next == '/' => (
                chars[i + 1..]
                    .iter()
                    .position(|(_, c)| *c == '\n')
                    .map_or(chars.len(), |p| i + 1 + p),
                true,
            ),
            '/' if next == '*' => {
                let mut depth = 1;
                let mut j = i + 2;
                while j < chars.len() && depth > 0 {
                    match (chars[j].1, chars.get(j + 1).map_or('\0', |(_, c)| *c)) {
                        ('/', '*') => {
                            depth += 1;
                            j += 2;
                        }
                        ('*', '/') => {
                            depth -= 1;
                            j += 2;
                        }
                        _ => j += 1,
                    }
                }
                (j, true)
            }
            '"' => (string_end(&chars, i), false),
            '\'' => (char_end(&chars, i), false),
            _ => (i + 1, false),
        };
        if comment {
            let stop = chars.get(end).map_or(src.len(), |&(at, _)| at);
            for b in &mut out[start..stop] {
                if *b != b'\n' {
                    *b = b' ';
                }
            }
        }
        i = end;
    }
    String::from_utf8(out).expect("blanking only substitutes spaces")
}

/// The initializer after `observed_at:`: up to the comma that ends the field,
/// across lines and past nested brackets, with whitespace collapsed.
fn initializer(code: &str) -> String {
    let mut depth = 0usize;
    let mut end = code.len();
    for (at, c) in code.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' if depth > 0 => depth -= 1,
            ',' | ';' | ')' | ']' | '}' => {
                end = at;
                break;
            }
            _ => {}
        }
    }
    code[..end].split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether every bracket in `expr` is closed in order.
fn balanced(expr: &str) -> bool {
    let mut depth = 0i32;
    for c in expr.chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

/// Whether `expr` is an age: a `Duration` binding, or a
/// `[chrono::]Duration::<unit>(…)` constructor call.
fn is_age(expr: &str) -> bool {
    let identifier = |s: &str| {
        s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    };
    if identifier(expr) {
        return true;
    }
    let call = expr.strip_prefix("chrono::").unwrap_or(expr);
    call.strip_prefix("Duration::")
        .and_then(|c| c.split_once('('))
        .is_some_and(|(unit, args)| {
            matches!(
                unit,
                "nanos"
                    | "micros"
                    | "milliseconds"
                    | "seconds"
                    | "minutes"
                    | "hours"
                    | "days"
                    | "weeks"
            ) && args
                .strip_suffix(')')
                .is_some_and(|a| !a.is_empty() && balanced(a))
        })
}

/// Whether an initializer reads the clock as the fixture is built:
/// `now_timestamp()` (what production stamps a fresh record with),
/// `chrono::Utc::now().to_rfc3339()`, or that minus an age. Whitespace is
/// ignored, so formatting cannot change the verdict.
fn is_now_relative(expr: &str) -> bool {
    let tight: String = expr.chars().filter(|c| !c.is_whitespace()).collect();
    if tight == "now_timestamp()" {
        return true;
    }
    let aged = tight
        .strip_prefix("(chrono::Utc::now()-")
        .and_then(|rest| rest.strip_suffix(").to_rfc3339()"));
    match aged {
        Some(age) => is_age(age),
        None => tight == "chrono::Utc::now().to_rfc3339()",
    }
}

/// Every occurrence of the field name in comment-free code, classified.
fn occurrences(code: &str) -> Vec<Occurrence> {
    let is_ident = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_');
    let mut out = Vec::new();
    for (at, _) in code.match_indices(FIELD) {
        let before = code[..at].chars().next_back();
        let rest = &code[at + FIELD.len()..];
        if is_ident(before) || is_ident(rest.chars().next()) {
            continue;
        }
        let tail = rest.trim_start();
        // A read (`record.observed_at`, `MergeRecord::observed_at`) is fine;
        // a field write is a stamp the initializer scan never sees.
        let init = tail.strip_prefix(':').map(initializer);
        let kind = match before {
            Some('.') | Some(':') if tail.starts_with('=') && !tail.starts_with("==") => {
                Use::NotAnInitializer
            }
            Some('.') | Some(':') => Use::Read,
            _ if is_now_relative(init.as_deref().unwrap_or_default()) => Use::ClockInit,
            _ if init.is_some() => Use::PinnedInit,
            _ => Use::NotAnInitializer,
        };
        let line = code[..at].matches('\n').count() + 1;
        let text = match (&init, &kind) {
            (Some(expr), Use::ClockInit | Use::PinnedInit) => expr.clone(),
            _ => code
                .lines()
                .nth(line - 1)
                .unwrap_or_default()
                .trim()
                .to_string(),
        };
        out.push(Occurrence { line, text, kind });
    }
    out
}

/// One line per offending occurrence, each naming its module.
fn offenses(rel: &str, found: &[Occurrence]) -> Vec<String> {
    found
        .iter()
        .filter(|o| o.offends())
        .map(|o| format!("{rel}:{}: `{}`", o.line, o.text))
        .collect()
}

/// The guard's report over the real fixture modules; empty when both pass.
fn guard() -> Vec<String> {
    let mut report = Vec::new();
    for rel in FIXTURES {
        let found = occurrences(&without_comments(&fixture(rel)));
        assert!(
            found
                .iter()
                .any(|o| matches!(o.kind, Use::ClockInit | Use::PinnedInit)),
            "no `{FIELD}:` initializer in {rel}; the guard is checking nothing"
        );
        report.extend(offenses(rel, &found));
    }
    report
}

fn offending_texts(src: &str) -> Vec<String> {
    occurrences(&without_comments(src))
        .into_iter()
        .filter(Occurrence::offends)
        .map(|o| o.text)
        .collect()
}

#[test]
fn both_fixture_modules_stamp_observed_at_from_the_clock() {
    let report = guard();
    assert!(
        report.is_empty(),
        "merge-observation fixtures stamp `{FIELD}` without the clock (#15799):\n{}\n\
         `run_pass` drops an incomplete record whose `{FIELD}` is seven days old (#15145), so a \
         calendar date — a literal, a named constant, a value passed in as text — passes today \
         and fails everywhere once it ages out. Stamp the record where the fixture builds it: \
         `now_timestamp()`, or `(chrono::Utc::now() - age).to_rfc3339()`.",
        report
            .iter()
            .map(|line| format!("  {line}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn a_pinned_stamp_restored_in_either_module_fails_naming_the_module() {
    for rel in FIXTURES {
        let src = fixture(rel);
        for pinned in ["MERGED_AT.into()", "\"2026-11-21T10:00:00Z\".into()"] {
            let mutated = src.replacen(
                "observed_at: now_timestamp(),",
                &format!("observed_at: {pinned},"),
                1,
            );
            let pinned_line = format!("observed_at: {pinned}");
            let Some(line) = mutated
                .lines()
                .position(|l| l.contains(&pinned_line))
                .map(|p| p + 1)
            else {
                panic!(
                    "{rel}: no `observed_at: now_timestamp(),` left to pin; repoint this test \
                     at a fresh initializer"
                );
            };
            let found = occurrences(&without_comments(&mutated));
            assert_eq!(offenses(rel, &found), [format!("{rel}:{line}: `{pinned}`")], "{rel}");
        }
    }
}

#[test]
fn clock_derived_initializers_are_accepted_in_any_formatting() {
    for expr in [
        "now_timestamp()",
        "chrono::Utc::now().to_rfc3339()",
        "(chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339()",
        "(chrono::Utc::now() - Duration::seconds(INCOMPLETE_RECORD_TTL_SECS)).to_rfc3339()",
        "(chrono::Utc::now() - age).to_rfc3339()",
        "(chrono::Utc::now()\n            - chrono::Duration::days(8))\n            .to_rfc3339()",
        " ( chrono::Utc::now ()\t- age ) . to_rfc3339 ( ) ",
    ] {
        assert!(is_now_relative(expr), "rejected: {expr:?}");
    }
}

#[test]
fn a_multiline_initializer_is_recognized_through_the_scan() {
    let src = r#"let rec = MergeRecord {
    observed_at: (chrono::Utc::now()
        - chrono::Duration::days(8))
        .to_rfc3339(),
    done: Vec::new(),
};
"#;
    assert!(offending_texts(src).is_empty(), "{src}");
}

#[test]
fn calendar_dates_are_rejected_whatever_today_is() {
    let today = chrono::Utc::now().to_rfc3339();
    for expr in [
        "MERGED_AT.into()",
        "\"2026-11-21T10:00:00Z\".into()",
        &format!("\"{today}\".into()"),
        "observed_at.to_string()",
        "&str",
        "stamp",
        "now",
        "week_ago",
        "chrono::Local::now().to_rfc3339()",
        "MERGED_AT.into() . to_string()",
    ] {
        assert!(!is_now_relative(expr), "accepted: {expr:?}");
    }
}

#[test]
fn every_other_way_of_naming_the_field_is_reported() {
    let json = r#"{"observed_at":"2026-11-21T10:00:00Z"}"#;
    let cases = [
        ("MergeRecord { observed_at: MERGED_AT.into() }", "MERGED_AT.into()"),
        (
            "MergeRecord { observed_at: \"2026-11-21T10:00:00Z\".into() }",
            "\"2026-11-21T10:00:00Z\".into()",
        ),
        ("MergeRecord {\n    observed_at: stamp,\n}", "stamp"),
        ("fn rec_for(pr: u64, observed_at: &str) -> MergeRecord", "&str"),
        ("let observed_at = MERGED_AT.into();", "let observed_at = MERGED_AT.into();"),
        ("MergeRecord { observed_at }", "MergeRecord { observed_at }"),
        ("rec.observed_at = MERGED_AT.into();", "rec.observed_at = MERGED_AT.into();"),
        (json, json),
    ];
    for (src, expected) in cases {
        assert_eq!(offending_texts(src), [expected.to_string()], "{src}");
    }
}

#[test]
fn a_pinned_merge_time_and_field_reads_are_left_alone() {
    let src = r#"
        const MERGED_AT: &str = "2026-11-21T10:00:00Z";
        let d = MergeRecord { merged_at: MERGED_AT.into(), observed_at: now_timestamp() };
        let e = MergeRecord { observed_at: now_timestamp(), merged_at: "2026-11-21T10:00:00Z".into() };
        assert_eq!(d.observed_at, e.observed_at);
        let at = &d.observed_at;
    "#;
    assert!(offending_texts(src).is_empty(), "{src}");
}

#[test]
fn comments_are_ignored_and_literals_cannot_hide_an_initializer() {
    let src = r##"
        // observed_at: MERGED_AT.into()
        /* observed_at: MERGED_AT.into() /* nested observed_at: x */ still a comment */
        /// observed_at: MERGED_AT.into()
        fn f<'a>(x: &'a str) {} // observed_at: MERGED_AT.into()
        let a = "not // a comment"; let p = MergeRecord { observed_at: MERGED_AT.into() };
        let b = "not /* a comment"; let q = MergeRecord { observed_at: "2026-11-21T10:00:00Z".into() };
        let c = '"'; let r = MergeRecord { observed_at: MERGED_AT.into() };
        let d = "it's \" quoted"; /* a real comment observed_at: x */ let s = MergeRecord { observed_at: MERGED_AT.into() };
        let t = r#"raw " quote // and /* not comments"#; let u = MergeRecord { observed_at: MERGED_AT.into() };
        let v = MergeRecord { observed_at: now_timestamp() }; // observed_at: MERGED_AT.into()
    "##;
    assert_eq!(
        offending_texts(src),
        [
            "MERGED_AT.into()",
            "\"2026-11-21T10:00:00Z\".into()",
            "MERGED_AT.into()",
            "MERGED_AT.into()",
            "MERGED_AT.into()",
        ],
        "{src}"
    );
}

#[test]
fn only_comments_are_blanked() {
    let src = "a // line\n/* block /* nested */ */ b \"// kept\" '\"' r#\"/* kept */\"# '\\''; c\n";
    let expected = format!(
        "a{}\n{} b \"// kept\" '\"' r#\"/* kept */\"# '\\''; c\n",
        " ".repeat(8),
        " ".repeat(24)
    );
    assert_eq!(without_comments(src), expected);
}
