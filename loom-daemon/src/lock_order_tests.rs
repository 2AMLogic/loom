//! Gate for the named-key lock-order invariant (#14404, generalized in #15189).
//!
//! `loom-daemon`'s lib-test binary stacks `serial_test` locks in many places:
//! every NAMED key (`loom_config_env`, `opencode_db_env`, …) is an **outer**
//! lock and the crate-default unkeyed `#[serial]` group is the **inner** one.
//! The order is mandatory — a site that acquired them the other way round would
//! form a lock cycle, and a cycle makes the test binary *hang*, not fail.
//!
//! The key set is not listed anywhere in this file: it is derived from the
//! scanned attributes themselves, so a newly-introduced key is covered without
//! an edit here.
//!
//! # A stack is acquired bottom-up
//!
//! Two lock attributes stacked on one function do not run in the order they
//! are written. Attribute macros expand first-to-last, and each
//! `serial_test_derive` expansion wraps the function *body* while re-emitting
//! the attributes below it onto the new outer function (`fn_setup` in 4.0.1).
//! The textually LAST lock attribute therefore expands outermost and is
//! acquired FIRST:
//!
//! ```text
//! #[serial]                  // textually first: expands innermost, acquired last
//! #[serial(loom_config_env)] // textually last: expands outermost, acquired first
//! fn t() {}
//! ```
//!
//! That is the order the crate's prevailing two-function shape has by
//! nesting — `#[serial(loom_config_env)] fn t() { t_body() }` over `#[serial]
//! fn t_body()`. A stack written the other way up takes `""` outer and the
//! named key inner, a cycle against every such helper, and it hung the
//! multi-threaded binary instead of failing it (#15674). This scan read
//! stacks top-down until then, i.e. required the cycle.
//! `serial_test_derive_expands_the_last_attribute_outermost` below now asks
//! `serial_test` itself, so a derive change fails here instead of silently
//! re-inverting what the scan enforces.
//!
//! Three mechanical properties keep the order consistent crate-wide:
//!
//! * Rule A (generalizes #14404's single-key gate): a function that carries a
//!   named key in a `serial`/`parallel` attribute must itself be a test
//!   (`#[test]`, `#[tokio::test]`, …), i.e. named keys are only ever taken by
//!   the outermost frame. A helper holding a named key can be called from
//!   inside the default group — the reverse order.
//! * Rule B: on one function's attribute stack, a bare `#[serial]` /
//!   `#[parallel]` (the default group, no keys) must sit textually ABOVE every
//!   attribute that names a key — it then expands innermost, so the default
//!   group is INNER there, as at every named-outer site.
//! * Rule C: the keys of the named attributes on one function's attribute
//!   stack, flattened in acquisition order (textually last attribute first),
//!   must be strictly increasing. Each attribute's own keys count sorted — the
//!   lexicographic order `serial_test_derive` itself applies inside a
//!   multi-key attribute — so one lexicographic order covers a `#[serial(a, b)]`
//!   and a stack alike. Nothing orders two stacked attributes but their source
//!   position, so `a` outer / `b` inner at one site and the reverse at another
//!   is the same lock cycle, one level above Rule B's default-group case.
//!
//! This module scans `loom-daemon/src` for violations, so a reverse-order
//! nesting fails here — loudly, on the PR that adds it — instead of hanging CI.
//!
//! Out of scope (documented blind spots): a body-level `fn inner()` nesting is
//! invisible to an attribute scan, and so is anything a macro generates beyond
//! the attributes it spells out (the `env_test!` macro in
//! `host_identity_tests.rs` is covered because its attributes are literal
//! source lines). Rule B is likewise same-function-only: a default-group outer
//! stack on one `fn` that calls a named-locked `fn` in its body is not seen.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// A function that breaks the named-key lock-order rules.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    /// 1-based line of the offending `serial`/`parallel` attribute.
    line: usize,
    /// The attribute text, whitespace-collapsed.
    attribute: String,
    /// The named lock key the violation is about (for a Rule B violation: the
    /// key of the named attribute stacked textually above the default group;
    /// for a Rule C violation: the key that does not sort after the one
    /// acquired before it).
    key: String,
}

/// Result of scanning one source file.
#[derive(Debug, Default)]
struct Scan {
    /// How many named (key-carrying) `serial`/`parallel` attributes were seen.
    attributes: usize,
    violations: Vec<Violation>,
}

/// The keys a `serial`/`parallel` lock attribute names, or `None` if the
/// attribute is not a lock attribute at all.
///
/// Matches the bare, `serial_test::`-pathed, single-key, multi-key and
/// `parallel` forms, and tolerates the attribute being split across lines
/// (the caller has already collapsed whitespace). A bare `#[serial]` /
/// `#[parallel]` is the crate-default lock group and yields an empty vector.
fn lock_attribute_keys(attr: &str) -> Option<Vec<String>> {
    let inner = attr.strip_prefix("#[").and_then(|a| a.strip_suffix(']'))?;
    let name = inner.split('(').next().unwrap_or("").trim();
    let name = name.strip_prefix("serial_test::").unwrap_or(name);
    if name != "serial" && name != "parallel" {
        return None;
    }
    let Some((_, args)) = inner.split_once('(') else {
        return Some(Vec::new());
    };
    Some(
        args.trim_end_matches(')')
            .split(',')
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect(),
    )
}

/// Is this attribute a test marker (`#[test]`, `#[tokio::test]`,
/// `#[tokio::test(flavor = "...")]`, …)?
fn is_test_attribute(attr: &str) -> bool {
    let Some(inner) = attr.strip_prefix("#[").and_then(|a| a.strip_suffix(']')) else {
        return false;
    };
    let name = inner.split('(').next().unwrap_or("").trim();
    name == "test" || name.ends_with("::test")
}

/// Does this line declare a function (`fn`, `pub fn`, `async fn`, …)?
fn declares_fn(line: &str) -> bool {
    let mut words = line.split(|c: char| !(c.is_alphanumeric() || c == '_'));
    words.any(|w| w == "fn")
}

/// Scan one file's source for lock-order violations.
///
/// Pure over `&str`, so the bite tests below need no files. Line-wise: attribute
/// lines (joined across line breaks until their brackets balance) accumulate into
/// a stack; comments and blank lines are transparent; the first other line ends
/// the stack, and if it declares a `fn`, the stack is judged — in acquisition
/// order, the textually last attribute first (see the module docs).
fn scan_source(src: &str) -> Scan {
    let mut scan = Scan::default();
    // (1-based line, collapsed attribute text) for the attribute stack in progress.
    let mut stack: Vec<(usize, String)> = Vec::new();
    // An attribute still being read across lines: (start line, text, bracket depth).
    let mut open: Option<(usize, String, i32)> = None;

    for (idx, raw) in src.lines().enumerate() {
        let lineno = idx + 1;
        let line = raw.trim();

        if let Some((start, mut text, mut depth)) = open.take() {
            text.push(' ');
            text.push_str(line);
            depth += bracket_delta(line);
            if depth > 0 {
                open = Some((start, text, depth));
            } else {
                stack.push((start, collapse(&text)));
            }
            continue;
        }

        if line.starts_with("#[") {
            let depth = bracket_delta(line);
            if depth > 0 {
                open = Some((lineno, line.to_string(), depth));
            } else {
                stack.push((lineno, collapse(line)));
            }
            continue;
        }

        if line.is_empty() || line.starts_with("//") {
            continue;
        }

        if declares_fn(line) {
            let is_test = stack.iter().any(|(_, a)| is_test_attribute(a));
            // Rule B anchor: has the default group (a bare attribute) been
            // acquired on the way in?
            let mut default_group_acquired = false;
            // Rule C anchor: the key acquired before the one being judged, in
            // the flattened named-key sequence of the stack.
            let mut last_key: Option<String> = None;
            // Acquisition order, not source order: `serial_test_derive` expands
            // the textually last attribute outermost, so the stack is walked
            // bottom-up.
            for (attr_line, attr) in stack.iter().rev() {
                let Some(keys) = lock_attribute_keys(attr) else {
                    continue;
                };
                if keys.is_empty() {
                    default_group_acquired = true;
                    continue;
                }
                scan.attributes += 1;
                if !is_test {
                    // Rule A: a named key on a function that is not a test.
                    scan.violations.push(Violation {
                        line: *attr_line,
                        attribute: attr.clone(),
                        key: keys[0].clone(),
                    });
                }
                if default_group_acquired {
                    // Rule B: the default group is already held, so this named
                    // key is taken inside it.
                    scan.violations.push(Violation {
                        line: *attr_line,
                        attribute: attr.clone(),
                        key: keys[0].clone(),
                    });
                }

                // Rule C: `serial_test_derive` sorts the keys inside one
                // attribute only, so across stacked ones the acquisition
                // order is the lock order and must itself be strictly
                // increasing.
                let mut sorted = keys;
                sorted.sort();
                for key in &sorted {
                    if last_key.as_ref().is_some_and(|last| last >= key) {
                        scan.violations.push(Violation {
                            line: *attr_line,
                            attribute: attr.clone(),
                            key: key.clone(),
                        });
                    }
                    last_key = Some(key.clone());
                }
            }
        } else {
            // An attribute stack that does not lead to a `fn` (a `mod`, a `use`,
            // a struct field, …) still names keys; count it so the sanity
            // floor measures every occurrence, but it is not a function lock.
            scan.attributes += stack
                .iter()
                .filter(|(_, a)| lock_attribute_keys(a).is_some_and(|k| !k.is_empty()))
                .count();
        }
        stack.clear();
    }
    scan
}

/// Net `[` minus `]` on a line; a positive total means the attribute is unfinished.
fn bracket_delta(line: &str) -> i32 {
    line.chars().fold(0, |d, c| match c {
        '[' => d + 1,
        ']' => d - 1,
        _ => d,
    })
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The repository root: `loom-daemon/`'s parent.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("loom-daemon/ has a parent")
        .to_path_buf()
}

fn tracked_files(root: &Path) -> Option<Vec<String>> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("ls-files")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

/// The gate, run against every tracked `loom-daemon/src/**/*.rs` file.
///
/// Fails if a function that is not a test carries any NAMED key in a
/// `serial`/`parallel` attribute (Rule A), if one function's attribute stack
/// puts the crate-default `#[serial]` group textually after an attribute
/// naming a key (Rule B) — the textually last attribute expands outermost, so
/// that inverts the crate-wide order "named keys OUTER, default group INNER" —
/// or if the named keys on one stack are not strictly increasing in
/// acquisition order, each attribute's own keys sorted (Rule C). Every one of
/// these shapes is a lock cycle that hangs the test binary instead of failing
/// it.
///
/// The test's name preserves the #14404 incident's spelling; it is cited in
/// comments across the crate
/// (`git grep loom_config_env_is_only_ever_the_outer_lock`).
#[test]
fn loom_config_env_is_only_ever_the_outer_lock() {
    let root = repo_root();
    if !root.join(".git").exists() {
        // A source tarball has no git index to enumerate; say so rather than
        // passing silently on an unmeasured tree.
        eprintln!("skipping: {} is not a git checkout", root.display());
        return;
    }
    let Some(files) = tracked_files(&root) else {
        eprintln!("skipping: `git ls-files` unavailable");
        return;
    };
    assert!(files.len() > 100, "`git ls-files` returned only {} path(s)", files.len());

    let mut attributes = 0;
    let mut problems = Vec::new();
    for rel in files
        .iter()
        .filter(|f| f.starts_with("loom-daemon/src/") && f.ends_with(".rs"))
    {
        // A tracked file deleted in the working tree is not this gate's concern.
        let Ok(src) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        let scan = scan_source(&src);
        attributes += scan.attributes;
        for v in scan.violations {
            problems.push(format!("{rel}:{} [{}] {}", v.line, v.key, v.attribute));
        }
    }

    assert!(
        attributes > 500,
        "scanned only {attributes} named `serial`/`parallel` attribute(s) — the scan is broken, not the tree"
    );
    assert!(
        problems.is_empty(),
        "named `serial`/`parallel` keys must stay OUTER and the crate-default `#[serial]` \
         group INNER. Three mechanical rules: (1) a function carrying a NAMED key must itself \
         be a test (`#[test]` / `#[tokio::test]`) — a helper holding the key can be reached \
         from inside the default group, which is a lock cycle that hangs the test binary \
         instead of failing it; (2) on one function's attribute stack, a bare `#[serial]` / \
         `#[parallel]` must sit textually ABOVE every attribute naming a key — \
         `serial_test_derive` expands the textually LAST attribute outermost, so a named \
         attribute stacked above the bare one takes the default group OUTER and the named \
         key INNER (#15674); (3) the named attributes on one function's attribute stack must \
         be in strictly increasing key order read bottom-up, each attribute's own keys \
         sorted — nothing else orders two stacked named keys. Offending site(s), key in \
         brackets:\n  {}",
        problems.join("\n  ")
    );
}

// ============================================================================
// The scan bites: pure rule tests over in-memory source
// ============================================================================

#[test]
fn a_reverse_order_nesting_fails_the_scan() {
    // Single-line literals on purpose: a raw multi-line string here would put a
    // line starting with `#[serial(...)]` in THIS file, which the gate above
    // would then (correctly) flag.
    let src =
        "#[serial]\nfn outer() {\n    inner();\n}\n\n#[serial(loom_config_env)]\nfn inner() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 6,
            attribute: "#[serial(loom_config_env)]".to_string(),
            key: "loom_config_env".to_string(),
        }]
    );
}

#[test]
fn a_normal_test_site_passes() {
    let src = "#[test]\n#[serial(loom_config_env)]\nfn ok() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}

#[test]
fn attribute_order_and_intervening_docs_do_not_hide_the_test_marker() {
    let src = "#[serial(loom_config_env)]\n/// doc\n#[test]\nfn ok() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}

#[test]
fn path_multi_key_and_parallel_forms_are_all_seen() {
    for attr in [
        "#[serial_test::serial(loom_config_env)]",
        "#[serial_test::serial(codesign_identity_env, loom_config_env)]",
        "#[serial(loom_config_env, other)]",
        "#[parallel(loom_config_env)]",
        "#[serial_test::parallel(loom_config_env)]",
        // Keys other than `loom_config_env` are seen too — the key set is
        // derived from the attributes, not listed in this file.
        "#[serial(opencode_db_env)]",
        "#[parallel(role_tick_ring)]",
    ] {
        let helper = scan_source(&format!("{attr}\nfn helper() {{}}\n"));
        assert_eq!(helper.violations.len(), 1, "{attr} on a helper must be flagged");
        let test = scan_source(&format!("#[test]\n{attr}\nfn t() {{}}\n"));
        assert!(test.violations.is_empty(), "{attr} on a test must pass");
        assert_eq!(test.attributes, 1);
    }
}

#[test]
fn named_keys_on_helpers_are_flagged_and_the_default_group_alone_is_not() {
    // Rewritten from `other_keys_and_the_default_group_are_not_flagged` when
    // the gate generalized from one key to every named key (#15189): "other
    // keys" are no longer ignored.
    let src = "#[serial]\nfn a() {}\n#[serial(other_env)]\nfn b() {}\n#[serial(codesign_identity_env)]\nfn c() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert_eq!(
        scan.violations,
        vec![
            Violation {
                line: 3,
                attribute: "#[serial(other_env)]".to_string(),
                key: "other_env".to_string(),
            },
            Violation {
                line: 5,
                attribute: "#[serial(codesign_identity_env)]".to_string(),
                key: "codesign_identity_env".to_string(),
            },
        ]
    );
}

#[test]
fn a_default_group_stacked_outside_a_named_key_fails_the_scan() {
    // The shape that hung the binary (#15674): the named attribute textually
    // above the bare one. `serial_test_derive` expands the textually last
    // attribute outermost, so the default group is OUTER and the named key
    // INNER here — the reverse of every `*_body()` helper, and a lock cycle
    // the attribute scan can and does see.
    let src = "#[test]\n#[serial(opencode_db_env)]\n#[serial]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 2,
            attribute: "#[serial(opencode_db_env)]".to_string(),
            key: "opencode_db_env".to_string(),
        }]
    );
}

#[test]
fn the_named_outside_default_order_passes() {
    // The bare attribute textually first expands innermost: named key OUTER,
    // default group INNER, the order every `*_body()` helper has by nesting.
    let src = "#[test]\n#[serial]\n#[serial(opencode_db_env)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}

#[test]
fn rule_b_reads_the_path_and_parallel_forms() {
    // Rule B judges the key list `lock_attribute_keys` already derives, so the
    // `serial_test::` path form and `parallel` are read like a bare
    // `#[serial]`: textually first, the default group is inner; textually
    // last, it is outer.
    for bare in [
        "#[serial_test::serial]",
        "#[parallel]",
        "#[serial_test::parallel]",
    ] {
        for named in [
            "#[serial(opencode_db_env)]",
            "#[serial_test::serial(opencode_db_env)]",
            "#[parallel(opencode_db_env)]",
        ] {
            let src = format!("#[test]\n{bare}\n{named}\nfn t() {{}}\n");
            let scan = scan_source(&src);
            assert!(scan.violations.is_empty(), "{bare} then {named}");

            let src = format!("#[test]\n{named}\n{bare}\nfn t() {{}}\n");
            let scan = scan_source(&src);
            assert_eq!(scan.violations.len(), 1, "{named} then {bare}");
            assert_eq!(scan.violations[0].line, 2, "{named} then {bare}");
            assert_eq!(scan.violations[0].attribute, named, "{named} then {bare}");
        }
    }
}

#[test]
fn stacked_named_keys_out_of_order_fail_the_scan() {
    // Two named attributes are ordered by nothing but their source position,
    // and read bottom-up: this takes `opencode_db_env` OUTER and
    // `codex_home_env` INNER — the reverse of the in-order stack in the test
    // below, and a lock cycle against it.
    let src = "#[test]\n#[serial(codex_home_env)]\n#[serial(opencode_db_env)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 2,
            attribute: "#[serial(codex_home_env)]".to_string(),
            key: "codex_home_env".to_string(),
        }]
    );
}

#[test]
fn stacked_named_keys_in_lexicographic_order_pass() {
    let src = "#[test]\n#[serial(opencode_db_env)]\n#[serial(codex_home_env)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert!(scan.violations.is_empty());
}

#[test]
fn a_multi_key_attribute_is_judged_on_its_sorted_keys() {
    // The textually last `#[serial(b, a)]` is the outer lock (module docs),
    // and `serial_test_derive` sorts the keys inside one attribute, so it
    // takes `a` then `b`: in order over a textually earlier `#[serial(c)]`,
    // which it wraps (`c` innermost) — not over a textually earlier
    // `#[serial(a)]`, which re-takes `a` inside.
    let src = "#[test]\n#[serial(c)]\n#[serial(b, a)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert!(scan.violations.is_empty());

    let src = "#[test]\n#[serial(a)]\n#[serial(b, a)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 2,
            attribute: "#[serial(a)]".to_string(),
            key: "a".to_string(),
        }]
    );
}

#[test]
fn the_same_named_key_twice_fails_the_scan() {
    // Strictly increasing rejects a repeated key as well: whether
    // `serial_test` re-enters a key it already holds was never traced, so
    // the scan does not bet on it.
    let src = "#[test]\n#[serial(opencode_db_env)]\n#[serial(opencode_db_env)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 2,
            attribute: "#[serial(opencode_db_env)]".to_string(),
            key: "opencode_db_env".to_string(),
        }]
    );
}

#[test]
fn rule_c_reads_the_split_path_and_parallel_forms() {
    // Rule C judges the key list `lock_attribute_keys` already derives, so an
    // attribute split across lines, the `serial_test::` path form and
    // `parallel` stacked with `serial` are all ordered by it. In every form
    // the textually LAST attribute is the outer lock: `codex_home_env` there
    // is in order over `opencode_db_env`, and out of order textually first.
    let opencode = "#[serial(opencode_db_env)]";
    for codex in [
        "#[serial(\n    codex_home_env,\n)]",
        "#[serial_test::serial(codex_home_env)]",
        "#[parallel(codex_home_env)]",
    ] {
        let src = format!("#[test]\n{opencode}\n{codex}\nfn t() {{}}\n");
        let scan = scan_source(&src);
        assert_eq!(scan.attributes, 2, "{opencode} then {codex}");
        assert!(scan.violations.is_empty(), "{opencode} then {codex}");

        let src = format!("#[test]\n{codex}\n{opencode}\nfn t() {{}}\n");
        let scan = scan_source(&src);
        assert_eq!(scan.violations.len(), 1, "{codex} then {opencode}");
        assert_eq!(scan.violations[0].line, 2, "{codex} then {opencode}");
        assert_eq!(scan.violations[0].key, "codex_home_env", "{codex} then {opencode}");
    }
}

#[test]
fn an_attribute_split_across_lines_is_still_seen() {
    let src = "#[test]\n#[serial(\n    loom_config_env\n)]\nfn ok() {}\n\n#[serial(\n    loom_config_env,\n)]\nfn helper() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert_eq!(scan.violations.len(), 1);
    assert_eq!(scan.violations[0].line, 7);
}

#[test]
fn tokio_test_counts_as_a_test() {
    for marker in [
        "#[tokio::test]",
        "#[tokio::test(flavor = \"multi_thread\")]",
    ] {
        let src = format!("{marker}\n#[serial(loom_config_env)]\nasync fn t() {{}}\n");
        let scan = scan_source(&src);
        assert_eq!(scan.attributes, 1);
        assert!(scan.violations.is_empty(), "{marker}");
    }
}

#[test]
fn a_macro_generated_test_with_literal_attributes_passes() {
    // The shape of `env_test!` in host_identity_tests.rs.
    let src = "$(#[$m])*\n#[test]\n#[serial(loom_config_env)]\nfn $name() {\n    #[serial]\n    fn inner() $body\n    inner();\n}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}

// ============================================================================
// The order the scan reads off the source is serial_test's real one
// ============================================================================
//
// The scan is static: it takes "the textually last attribute is acquired
// first" off the source, and that holds only while `serial_test_derive` keeps
// expanding the way 4.0.1 does. It was read the other way round until #15674,
// for exactly as long as nobody asked the library. The probe below asks.

/// How long one attempt on the outer probe key may take before it counts as
/// blocked behind the probe.
const PROBE_ATTEMPT: Duration = Duration::from_millis(250);
/// How long the probe threads may take to reach their locks on a loaded host.
const PROBE_DEADLINE: Duration = Duration::from_secs(30);
/// Attempts blocked in a row that count as "the probe holds the outer key".
/// One can be a thread that was merely descheduled; two in a row cannot.
const PROBE_CONFIRMATIONS: usize = 2;
/// A pause after an attempt that found the outer key free, so the failure
/// path retries at a human pace instead of spawning a thread per iteration.
const PROBE_PACE: Duration = Duration::from_millis(20);

/// Set by `lock_order_probe`'s body: it got both keys.
static PROBE_BODY_RAN: AtomicBool = AtomicBool::new(false);
/// Set by `lock_order_probe_hold_inner` once it holds the inner key.
static PROBE_INNER_HELD: AtomicBool = AtomicBool::new(false);
/// Set by the test; `lock_order_probe_hold_inner` returns once it closes.
static PROBE_INNER_RELEASE: Mutex<Option<mpsc::Receiver<()>>> = Mutex::new(None);

// The probe functions carry named keys, so Rule A requires `#[test]` on each,
// and `#[serial]` only wraps a function that takes no arguments — so they are
// ignored tests the test below calls directly, like the `live_gh_guard`
// fixture. Run under a bare `--ignored` they take their keys and return.

/// The shape under test. The keys are named by acquisition position:
/// `…_1_outer` is the textually LAST attribute and must be acquired first,
/// `…_2_inner` the textually first and acquired second. Only this probe uses
/// them, and the crate-default group is never touched, so it neither waits on
/// nor holds up any other test.
#[test]
#[ignore = "called directly by serial_test_derive_expands_the_last_attribute_outermost"]
#[serial_test::serial(lock_order_probe_2_inner)]
#[serial_test::serial(lock_order_probe_1_outer)]
fn lock_order_probe() {
    PROBE_BODY_RAN.store(true, Ordering::SeqCst);
}

/// Holds the inner key until the test closes the release channel.
#[test]
#[ignore = "called directly by serial_test_derive_expands_the_last_attribute_outermost"]
#[serial_test::serial(lock_order_probe_2_inner)]
fn lock_order_probe_hold_inner() {
    PROBE_INNER_HELD.store(true, Ordering::SeqCst);
    // Under a bare `--ignored` run nobody holds the other end: return.
    let release = PROBE_INNER_RELEASE.lock().unwrap().take();
    if let Some(release) = release {
        let _ = release.recv();
    }
}

/// Takes the outer key and releases it at once: the attempt the test times.
#[test]
#[ignore = "called directly by serial_test_derive_expands_the_last_attribute_outermost"]
#[serial_test::serial(lock_order_probe_1_outer)]
fn lock_order_probe_take_outer() {}

/// What one attempt on the outer key reported.
#[derive(Debug, PartialEq, Eq)]
enum Attempt {
    /// The attempt thread is running and about to take the key.
    Started,
    /// The key was free: taken and released.
    Free,
}

/// The scan's one assumption, checked against `serial_test` itself.
///
/// Another thread holds the inner key, so `lock_order_probe` blocks on it.
/// If the textually last attribute is acquired first — what the scan assumes —
/// the blocked probe already holds the outer key, and an attempt to take it
/// blocks too. If `serial_test_derive` ever expands the other way round, the
/// blocked probe holds nothing, every attempt takes the key at once, and this
/// fails at the deadline instead of the gate silently enforcing a lock cycle.
#[test]
fn serial_test_derive_expands_the_last_attribute_outermost() {
    PROBE_BODY_RAN.store(false, Ordering::SeqCst);
    PROBE_INNER_HELD.store(false, Ordering::SeqCst);
    let (release, release_rx) = mpsc::channel::<()>();
    *PROBE_INNER_RELEASE.lock().unwrap() = Some(release_rx);

    let deadline = Instant::now() + PROBE_DEADLINE;
    let holder = thread::spawn(lock_order_probe_hold_inner);
    while !PROBE_INNER_HELD.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the holder thread never took the inner probe key");
        thread::sleep(Duration::from_millis(1));
    }
    let probe = thread::spawn(lock_order_probe);

    // Each attempt announces that it is running before it tries the key, so
    // an attempt still silent for `PROBE_ATTEMPT` is blocked, not merely
    // unscheduled. Only a blocked attempt outlives its iteration.
    let mut attempts: Vec<JoinHandle<()>> = Vec::new();
    let mut blocked_in_a_row = 0;
    while blocked_in_a_row < PROBE_CONFIRMATIONS {
        assert!(
            Instant::now() < deadline,
            "the outer probe key stayed free for {PROBE_DEADLINE:?} while the probe was blocked \
             on the inner key: `serial_test_derive` acquired the textually FIRST attribute \
             outermost, the reverse of what the scan reads off an attribute stack"
        );
        let (report, reports) = mpsc::channel();
        let attempt = thread::spawn(move || {
            let _ = report.send(Attempt::Started);
            lock_order_probe_take_outer();
            let _ = report.send(Attempt::Free);
        });
        assert_eq!(
            reports.recv_timeout(PROBE_DEADLINE),
            Ok(Attempt::Started),
            "an outer-key attempt never started"
        );
        match reports.recv_timeout(PROBE_ATTEMPT) {
            Ok(Attempt::Free) => {
                attempt.join().expect("outer-key attempt thread panicked");
                blocked_in_a_row = 0;
                thread::sleep(PROBE_PACE);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                attempts.push(attempt);
                blocked_in_a_row += 1;
            }
            other => panic!("unexpected report from an outer-key attempt: {other:?}"),
        }
    }
    assert!(
        !PROBE_BODY_RAN.load(Ordering::SeqCst),
        "the probe body ran while another thread held the inner key"
    );

    // Let go of the inner key: only now may the probe run and release the
    // outer key to the attempts still blocked on it.
    drop(release);
    holder.join().expect("holder thread panicked");
    probe.join().expect("probe thread panicked");
    assert!(
        PROBE_BODY_RAN.load(Ordering::SeqCst),
        "the probe body never ran once the inner key was released"
    );
    for attempt in attempts {
        attempt.join().expect("outer-key attempt thread panicked");
    }
}
