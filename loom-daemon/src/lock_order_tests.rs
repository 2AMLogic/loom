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
//! Three mechanical properties keep the order consistent crate-wide:
//!
//! * Rule A (generalizes #14404's single-key gate): a function that carries a
//!   named key in a `serial`/`parallel` attribute must itself be a test
//!   (`#[test]`, `#[tokio::test]`, …), i.e. named keys are only ever taken by
//!   the outermost frame. A helper holding a named key can be called from
//!   inside the default group — the reverse order.
//! * Rule B: on one function's attribute stack, a bare `#[serial]` /
//!   `#[parallel]` (the default group, no keys) must not appear textually
//!   before an attribute that names a key — the textually first attribute
//!   expands first, so that stacking inverts the order against every
//!   named-outer site.
//! * Rule C: the keys of the named attributes on one function's attribute
//!   stack, flattened in textual order, must be strictly increasing. Each
//!   attribute's own keys count sorted — the lexicographic order
//!   `serial_test` itself applies inside a multi-key attribute — so one
//!   lexicographic order covers a `#[serial(a, b)]` and a stack alike.
//!   Nothing orders two stacked attributes but their source position, so
//!   `a` outer / `b` inner at one site and the reverse at another is the
//!   same lock cycle, one level above Rule B's default-group case.
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

/// A function that breaks the named-key lock-order rules.
#[derive(Debug, PartialEq, Eq)]
struct Violation {
    /// 1-based line of the offending `serial`/`parallel` attribute.
    line: usize,
    /// The attribute text, whitespace-collapsed.
    attribute: String,
    /// The named lock key the violation is about (for a Rule B violation: the
    /// key of the named attribute the default group was stacked outside of;
    /// for a Rule C violation: the key that does not sort after the one
    /// before it).
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
/// the stack, and if it declares a `fn`, the stack is judged.
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
            // Rule B anchor: the first default-group (bare) lock attribute on
            // the stack, if any. A named attribute stacked after it puts the
            // default group OUTER and the named key INNER.
            let mut default_group: Option<(usize, String)> = None;
            // Rule C anchor: the key before the one being judged, in the
            // flattened named-key sequence of the stack.
            let mut last_key: Option<String> = None;
            for (attr_line, attr) in &stack {
                let Some(keys) = lock_attribute_keys(attr) else {
                    continue;
                };
                if keys.is_empty() {
                    default_group.get_or_insert((*attr_line, attr.clone()));
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
                if let Some((line, attribute)) = &default_group {
                    // Rule B: the default group textually before a named key.
                    scan.violations.push(Violation {
                        line: *line,
                        attribute: attribute.clone(),
                        key: keys[0].clone(),
                    });
                }

                // Rule C: `serial_test` sorts the keys inside one attribute
                // only, so across stacked ones the textual order is the lock
                // order and must itself be strictly increasing.
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
/// puts the crate-default `#[serial]` group textually before an attribute
/// naming a key (Rule B) — the reverse of the crate-wide order "named keys
/// OUTER, default group INNER" — or if the named keys on one stack are not
/// strictly increasing in textual order, each attribute's own keys sorted
/// (Rule C). Every one of these shapes is a lock cycle that hangs the test
/// binary instead of failing it.
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
         `#[parallel]` must not sit textually before an attribute naming a key — the outer \
         attribute expands first, so that inverts the order against every named-outer site; \
         (3) the named attributes on one function's attribute stack must be in strictly \
         increasing key order, each attribute's own keys sorted — nothing else orders two \
         stacked named keys. Offending site(s), key in brackets:\n  {}",
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
    // The reverse of every existing site: the bare (default-group) attribute
    // is textually first, so the default lock is OUTER and the named key
    // INNER — a lock cycle the attribute scan can and does see.
    let src = "#[test]\n#[serial]\n#[serial(opencode_db_env)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 2,
            attribute: "#[serial]".to_string(),
            key: "opencode_db_env".to_string(),
        }]
    );
}

#[test]
fn the_named_outside_default_order_passes() {
    let src = "#[test]\n#[serial(opencode_db_env)]\n#[serial]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 1);
    assert!(scan.violations.is_empty());
}

#[test]
fn stacked_named_keys_out_of_order_fail_the_scan() {
    // Two named attributes are ordered by nothing but their source position,
    // so this takes `opencode_db_env` OUTER: the reverse of the in-order
    // stack in the test below, and a lock cycle against it.
    let src = "#[test]\n#[serial(opencode_db_env)]\n#[serial(codex_home_env)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 3,
            attribute: "#[serial(codex_home_env)]".to_string(),
            key: "codex_home_env".to_string(),
        }]
    );
}

#[test]
fn stacked_named_keys_in_lexicographic_order_pass() {
    let src = "#[test]\n#[serial(codex_home_env)]\n#[serial(opencode_db_env)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert!(scan.violations.is_empty());
}

#[test]
fn a_multi_key_attribute_is_judged_on_its_sorted_keys() {
    // `serial_test` sorts the keys inside one attribute, so `#[serial(b, a)]`
    // takes `a` then `b`: in order under `#[serial(c)]`, not under a later
    // `#[serial(a)]`.
    let src = "#[test]\n#[serial(b, a)]\n#[serial(c)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 2);
    assert!(scan.violations.is_empty());

    let src = "#[test]\n#[serial(b, a)]\n#[serial(c)]\n#[serial(a)]\nfn t() {}\n";
    let scan = scan_source(src);
    assert_eq!(scan.attributes, 3);
    assert_eq!(
        scan.violations,
        vec![Violation {
            line: 4,
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
            line: 3,
            attribute: "#[serial(opencode_db_env)]".to_string(),
            key: "opencode_db_env".to_string(),
        }]
    );
}

#[test]
fn rule_c_reads_the_split_path_and_parallel_forms() {
    // Rule C judges the key list `lock_attribute_keys` already derives, so an
    // attribute split across lines, the `serial_test::` path form and
    // `parallel` stacked with `serial` are all ordered by it.
    let inner = "#[serial(opencode_db_env)]";
    for outer in [
        "#[serial(\n    codex_home_env,\n)]",
        "#[serial_test::serial(codex_home_env)]",
        "#[parallel(codex_home_env)]",
    ] {
        let src = format!("#[test]\n{outer}\n{inner}\nfn t() {{}}\n");
        let scan = scan_source(&src);
        assert_eq!(scan.attributes, 2, "{outer} then {inner}");
        assert!(scan.violations.is_empty(), "{outer} then {inner}");

        let src = format!("#[test]\n{inner}\n{outer}\nfn t() {{}}\n");
        let scan = scan_source(&src);
        assert_eq!(scan.violations.len(), 1, "{inner} then {outer}");
        assert_eq!(scan.violations[0].line, 3, "{inner} then {outer}");
        assert_eq!(scan.violations[0].key, "codex_home_env", "{inner} then {outer}");
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
