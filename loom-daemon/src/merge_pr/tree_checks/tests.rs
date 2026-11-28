use super::*;
use crate::test_git_helpers::run;

fn cfg(checks: &[&str], secs: u64) -> Config {
    Config {
        checks: checks.iter().map(|s| s.to_string()).collect(),
        timeout: Duration::from_secs(secs),
    }
}

#[test]
fn config_unset_and_empty_are_no_checks() {
    assert!(parse_config("{}").unwrap().checks.is_empty());
    assert!(parse_config(r#"{"merge":{}}"#).unwrap().checks.is_empty());
    assert!(parse_config(r#"{"merge":{"treeChecks":[]}}"#)
        .unwrap()
        .checks
        .is_empty());
}

#[test]
fn config_lists_checks_and_timeout() {
    let c =
        parse_config(r#"{"merge":{"treeChecks":["a"," b "],"treeChecksTimeoutSecs":5}}"#).unwrap();
    assert_eq!(c.checks, vec!["a", "b"]);
    assert_eq!(c.timeout, Duration::from_secs(5));
    assert_eq!(
        parse_config(r#"{"merge":{"treeChecks":["a"]}}"#)
            .unwrap()
            .timeout,
        Duration::from_secs(DEFAULT_TIMEOUT_SECS)
    );
}

#[test]
fn config_malformed_fails_closed() {
    assert!(parse_config("{").is_err());
    assert!(parse_config(r#"{"merge":{"treeChecks":"x"}}"#).is_err());
    assert!(parse_config(r#"{"merge":{"treeChecks":[1]}}"#).is_err());
    assert!(parse_config(r#"{"merge":{"treeChecks":[""]}}"#).is_err());
}

#[test]
fn unset_config_is_clean_without_touching_git() {
    // A path that is not a repo: any git call would make this Unknown.
    let nowhere = Path::new("/nonexistent-loom-tree-checks");
    assert_eq!(evaluate(nowhere, None, "origin", "1", "main", "abc"), Outcome::Clean);
    assert_eq!(evaluate(nowhere, Some("{}"), "origin", "1", "main", "abc"), Outcome::Clean);
    assert_eq!(
        evaluate(nowhere, Some(r#"{"merge":{"treeChecks":[]}}"#), "origin", "1", "main", "abc"),
        Outcome::Clean
    );
}

#[test]
fn run_checks_passes_and_stops_at_first_failure_in_order() {
    let d = tempfile::tempdir().unwrap();
    assert_eq!(run_checks(d.path(), &cfg(&["true", "test -d ."], 10)), Outcome::Clean);
    match run_checks(d.path(), &cfg(&["true", "echo REAL-OUTPUT; exit 3", "echo never > ran"], 10))
    {
        Outcome::Failed { check, output } => {
            assert_eq!(check, "echo REAL-OUTPUT; exit 3");
            assert!(output.contains("REAL-OUTPUT") && output.contains("exit 3"), "{output}");
        }
        o => panic!("{o:?}"),
    }
    assert!(!d.path().join("ran").exists());
}

#[test]
fn run_checks_timeout_is_a_failure_and_env_is_scrubbed() {
    let d = tempfile::tempdir().unwrap();
    match run_checks(d.path(), &cfg(&["sleep 30"], 1)) {
        Outcome::Failed { output, .. } => assert!(output.contains("timed out")),
        o => panic!("{o:?}"),
    }
    std::env::set_var("LOOM_TREE_CHECKS_TEST_SECRET", "x");
    assert_eq!(
        run_checks(d.path(), &cfg(&["test -z \"$LOOM_TREE_CHECKS_TEST_SECRET\""], 10)),
        Outcome::Clean
    );
}

/// origin (bare) with main + refs/pull/7/head, and a clone to run from.
/// Base moves after the PR branched: the PR adds migrations/002_pr.sql, main
/// adds migrations/002_main.sql — each fine alone, a prefix collision merged.
fn fixture() -> (tempfile::TempDir, std::path::PathBuf, String) {
    let t = tempfile::tempdir().unwrap();
    let origin = t.path().join("origin.git");
    let work = t.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    run(
        t.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    );
    run(&work, &["init", "-q", "-b", "main"]);
    run(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
    std::fs::create_dir_all(work.join("migrations")).unwrap();
    std::fs::write(work.join("migrations/001.sql"), "a").unwrap();
    run(&work, &["add", "."]);
    run(&work, &["commit", "-qm", "base"]);
    run(&work, &["checkout", "-qb", "pr"]);
    std::fs::write(work.join("migrations/002_pr.sql"), "p").unwrap();
    run(&work, &["add", "."]);
    run(&work, &["commit", "-qm", "pr"]);
    let head = run(&work, &["rev-parse", "HEAD"]);
    run(&work, &["checkout", "-q", "main"]);
    std::fs::write(work.join("migrations/002_main.sql"), "m").unwrap();
    run(&work, &["add", "."]);
    run(&work, &["commit", "-qm", "sibling"]);
    run(
        &work,
        &[
            "push",
            "-q",
            "origin",
            "main",
            &format!("{head}:refs/pull/7/head"),
        ],
    );
    (t, work, head)
}

const PREFIX_CHECK: &str = "test \"$(ls migrations | cut -c1-3 | sort | uniq -d | wc -l)\" -eq 0";

#[test]
fn merge_tree_collision_is_caught_and_passing_tree_is_clean() {
    let (_t, work, head) = fixture();
    let bad = serde_json::json!({"merge": {"treeChecks": [PREFIX_CHECK]}}).to_string();
    match evaluate(&work, Some(&bad), "origin", "7", "main", &head) {
        Outcome::Failed { check, .. } => assert!(check.contains("uniq -d")),
        o => panic!("{o:?}"),
    }
    let good = r#"{"merge":{"treeChecks":["test -f migrations/002_pr.sql","test -f migrations/002_main.sql"]}}"#;
    assert_eq!(evaluate(&work, Some(good), "origin", "7", "main", &head), Outcome::Clean);
    // The primary checkout was never touched.
    assert!(!work.join("migrations/002_pr.sql").exists());
}

#[test]
fn moved_head_and_unfetchable_pr_fail_closed() {
    let (_t, work, head) = fixture();
    let cfgj = r#"{"merge":{"treeChecks":["true"]}}"#;
    assert!(matches!(
        evaluate(&work, Some(cfgj), "origin", "7", "main", "deadbeef"),
        Outcome::Unknown(_)
    ));
    assert!(matches!(
        evaluate(&work, Some(cfgj), "origin", "99", "main", &head),
        Outcome::Unknown(_)
    ));
}

#[test]
fn failure_comment_fence_outlasts_backticks_in_output() {
    let c = failure_comment("x", "a\n```\nb ```` c");
    assert!(c.contains("\n`````\na\n```\nb ```` c\n`````\n"), "{c}");
    assert!(failure_comment("x", "plain").contains("\n```\nplain\n```\n"));
}
