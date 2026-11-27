use super::*;
use std::cell::RefCell;
use std::collections::HashMap;

#[derive(Default)]
struct Fake {
    issues: HashMap<String, String>,
    kinds: HashMap<String, String>,
    commits: String,
    fail_helpers: Vec<&'static str>,
    close_fails: bool,
    timeline: HashMap<String, String>,
    calls: RefCell<Vec<String>>,
}

impl Fake {
    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }
}

impl Host for Fake {
    fn read_issue(&self, _: &str, i: &str) -> String {
        self.calls.borrow_mut().push(format!("read {i}"));
        self.issues.get(i).cloned().unwrap_or_else(|| "{}".into())
    }
    fn commit_messages(&self, _: &str, _: &str) -> String {
        self.commits.clone()
    }
    fn issue_kind(&self, _: &str, i: &str) -> String {
        self.kinds.get(i).cloned().unwrap_or_default()
    }
    fn helper(&self, func: &str, args: &[&str]) -> bool {
        self.calls
            .borrow_mut()
            .push(format!("{func} {}", args[..args.len().min(4)].join(" ")));
        !self.fail_helpers.contains(&func)
    }
    fn close_issue(&self, _: &str, i: &str, _: &str) -> bool {
        self.calls.borrow_mut().push(format!("close {i}"));
        !self.close_fails
    }
    fn label_events(&self, _: &str, i: &str) -> Option<String> {
        self.calls.borrow_mut().push(format!("timeline {i}"));
        self.timeline.get(i).cloned()
    }
    fn timestamp(&self) -> String {
        "T".into()
    }
}

fn inputs(body: &str) -> Inputs {
    Inputs {
        repo: "o/r".into(),
        pr_number: "9".into(),
        pr_branch: "feature/issue-42".into(),
        pr_body: body.into(),
        ..Inputs::default()
    }
}

fn done(o: Outcome) -> Vec<Record> {
    match o {
        Outcome::Done(r) => r,
        Outcome::Need(n) => panic!("unexpected need {n:?}"),
        Outcome::Defer(_) => panic!("unexpected defer"),
    }
}

fn levels(r: &[Record]) -> Vec<&str> {
    r.iter().map(|(l, _)| l.as_str()).collect()
}

const OPEN_BUILDING: &str = r#"{"state":"open","labels":[{"name":"loom:building"}]}"#;
const CLOSED_BUILDING: &str = r#"{"state":"closed","labels":[{"name":"loom:building"}]}"#;

#[test]
fn partial_reset_swaps_and_comments() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), OPEN_BUILDING.into());
    let r = done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f));
    assert_eq!(levels(&r), ["INFO", "SUCCESS"]);
    let c = f.calls();
    assert_eq!(c[0], "read 5");
    assert_eq!(c[1], "forge_gh_swap_label_rl_safe o/r 5 loom:building loom:issue");
    assert!(c[2].starts_with("forge_gh_comment_rl_safe o/r 5 ## Partial Increment Merged"));
    assert!(!c[2].contains("**Reopened**"));
}

#[test]
fn partial_reset_empty_body_does_nothing() {
    let f = Fake::default();
    assert!(done(run(Pass::PartialReset, &inputs(""), &Facts::default(), &f)).is_empty());
    assert!(f.calls().is_empty());
}

#[test]
fn partial_reset_reverts_a_premature_close_before_swapping() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), CLOSED_BUILDING.into());
    let mut i = inputs("Part of #5");
    i.conflicted = "5".into();
    let r = done(run(Pass::PartialReset, &i, &Facts::default(), &f));
    assert_eq!(levels(&r), ["WARNING", "SUCCESS", "INFO", "SUCCESS"]);
    let c = f.calls();
    assert!(c[1].starts_with("forge_gh_reopen_issue_rl_safe o/r 5"));
    assert!(c[2].starts_with("forge_gh_comment_rl_safe o/r 5 ## Premature Auto-Close Reverted"));
    assert!(c[3].starts_with("forge_gh_swap_label_rl_safe"));
    assert!(c[4].contains("**Reopened**"));
}

#[test]
fn partial_reset_stops_when_the_reopen_fails() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), CLOSED_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_reopen_issue_rl_safe"];
    let mut i = inputs("Part of #5");
    i.conflicted = "5".into();
    let r = done(run(Pass::PartialReset, &i, &Facts::default(), &f));
    assert!(r.last().unwrap().1.starts_with("Could not reopen issue #5"));
    assert!(!f.calls().iter().any(|c| c.contains("swap_label")));
}

#[test]
fn partial_reset_never_touches_a_pull_request() {
    let mut f = Fake::default();
    f.issues
        .insert("5".into(), r#"{"pull_request":{},"state":"open"}"#.into());
    assert!(done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f)).is_empty());
    assert_eq!(f.calls(), ["read 5"]);
}

#[test]
fn partial_reset_failed_swap_warns_and_posts_no_comment() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), OPEN_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_swap_label_rl_safe"];
    let r = done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f));
    assert_eq!(levels(&r), ["INFO", "WARNING"]);
    assert!(!f.calls().iter().any(|c| c.contains("comment")));
}

#[test]
fn partial_reset_comment_failure_is_a_warning_after_the_swap() {
    let mut f = Fake::default();
    f.issues.insert("5".into(), OPEN_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_comment_rl_safe"];
    let r = done(run(Pass::PartialReset, &inputs("Part of #5"), &Facts::default(), &f));
    assert_eq!(levels(&r), ["INFO", "SUCCESS", "WARNING"]);
    assert!(r[2].1.contains("label swap still applied"));
}

#[test]
fn branch_issue_is_exact() {
    assert_eq!(branch_issue("feature/issue-42"), Some("42"));
    for b in [
        "feature/issue-",
        "feature/issue-4x",
        "x/feature/issue-4",
        "feature/issue-4-slice",
        "main",
        "",
    ] {
        assert_eq!(branch_issue(b), None, "{b}");
    }
}

#[test]
fn branch_close_asks_for_close_targets_before_anything() {
    let f = Fake::default();
    assert_eq!(
        run(Pass::BranchClose, &inputs("body"), &Facts::default(), &f),
        Outcome::Need(Need::CloseTargets)
    );
    assert!(f.calls().is_empty());
}

fn facts(t: &str) -> Facts {
    Facts {
        close_targets: Some(t.into()),
    }
}

#[test]
fn branch_close_closes_an_open_undeclared_issue() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    let r = done(run(Pass::BranchClose, &inputs("no keyword"), &facts(""), &f));
    assert_eq!(levels(&r), ["SUCCESS"]);
    assert_eq!(f.calls(), ["close 42"]);
}

#[test]
fn branch_close_skips_each_reason_to_leave_it_open() {
    let open = |f: &mut Fake| {
        f.kinds.insert("42".into(), "open".into());
    };
    // partial-increment declaration: decided before the close targets are asked for
    let mut f = Fake::default();
    open(&mut f);
    assert!(done(run(Pass::BranchClose, &inputs("Part of #42"), &Facts::default(), &f)).is_empty());
    // already a close target
    assert!(done(run(Pass::BranchClose, &inputs("x"), &facts("7\n42\n"), &f)).is_empty());
    // closing keyword in the body, then in a commit message
    assert!(done(run(Pass::BranchClose, &inputs("Fixes #42"), &facts(""), &f)).is_empty());
    f.commits = "wip\n\nresolved  #42".into();
    assert!(done(run(Pass::BranchClose, &inputs("x"), &facts(""), &f)).is_empty());
    // not open / a PR / unreadable
    for kind in ["closed", "pr", ""] {
        let mut g = Fake::default();
        g.kinds.insert("42".into(), kind.into());
        assert!(done(run(Pass::BranchClose, &inputs("x"), &facts(""), &g)).is_empty(), "{kind}");
    }
    assert!(!f.calls().iter().any(|c| c.starts_with("close")));
}

#[test]
fn branch_close_failure_is_a_warning() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    f.close_fails = true;
    let r = done(run(Pass::BranchClose, &inputs("x"), &facts(""), &f));
    assert_eq!(levels(&r), ["WARNING"]);
}

const MERGED_AT: &str = "2026-11-21T10:00:00Z";
const AFTER_MERGE: &str = "2026-11-21T11:30:00Z";
const BEFORE_MERGE: &str = "2026-11-20T08:00:00Z";

/// The observation replay's input shape (#15145): `merged_at` set.
fn observed(body: &str) -> Inputs {
    Inputs {
        merged_at: Some(MERGED_AT.into()),
        ..inputs(body)
    }
}

fn reopened_timeline(at: &str) -> String {
    serde_json::json!([{"event": "reopened", "created_at": at}]).to_string()
}

#[test]
fn branch_close_without_merged_at_never_reads_the_timeline() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    // No timeline seeded — and none consulted: the synchronous shell
    // caller's close is exactly what it was (#15145).
    let r = done(run(Pass::BranchClose, &inputs("x"), &facts(""), &f));
    assert_eq!(levels(&r), ["SUCCESS"]);
    assert!(!f.calls().iter().any(|c| c.starts_with("timeline")));
}

#[test]
fn branch_close_with_merged_at_skips_a_reopened_issue() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    f.timeline
        .insert("42".into(), reopened_timeline(AFTER_MERGE));
    let r = done(run(Pass::BranchClose, &observed("x"), &facts(""), &f));
    assert_eq!(levels(&r), ["INFO"]);
    assert!(!f.calls().iter().any(|c| c.starts_with("close")));
}

#[test]
fn branch_close_with_merged_at_closes_when_the_reopen_predates() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    f.timeline
        .insert("42".into(), reopened_timeline(BEFORE_MERGE));
    let r = done(run(Pass::BranchClose, &observed("x"), &facts(""), &f));
    assert_eq!(levels(&r), ["SUCCESS"]);
    assert_eq!(f.calls().iter().filter(|c| c.starts_with("close")).count(), 1);
}

#[test]
fn branch_close_with_merged_at_defers_on_an_unreadable_timeline() {
    let mut f = Fake::default();
    f.kinds.insert("42".into(), "open".into());
    match run(Pass::BranchClose, &observed("x"), &facts(""), &f) {
        Outcome::Defer(recs) => assert_eq!(levels(&recs), ["WARNING"]),
        other => panic!("expected a defer, got {other:?}"),
    }
    assert!(!f.calls().iter().any(|c| c.starts_with("close")));
}

#[test]
fn branch_close_ignores_other_branches() {
    let f = Fake::default();
    let mut i = inputs("x");
    i.pr_branch = "docs/guide".into();
    assert!(done(run(Pass::BranchClose, &i, &Facts::default(), &f)).is_empty());
}

#[test]
fn closing_keyword_matching_is_line_local_and_word_bounded() {
    assert!(has_closing_keyword("CLOSES #42", "42"));
    assert!(has_closing_keyword("fix\t#42.", "42"));
    assert!(!has_closing_keyword("fix\n#42", "42"), "grep never spans lines");
    assert!(!has_closing_keyword("closes #421", "42"));
    assert!(!has_closing_keyword("disclose #42", "42"));
    assert!(!has_closing_keyword("fixes #4", "42"));
}

#[test]
fn strip_asks_first_then_strips_only_closed_claims() {
    let mut f = Fake::default();
    assert_eq!(
        run(Pass::Strip, &inputs(""), &Facts::default(), &f),
        Outcome::Need(Need::CloseTargets)
    );
    f.issues.insert("1".into(), CLOSED_BUILDING.into());
    f.issues.insert("2".into(), OPEN_BUILDING.into());
    f.issues.insert(
        "3".into(),
        r#"{"state":"closed","labels":[{"name":"loom:building"},{"name":"loom:issue"}]}"#.into(),
    );
    let r = done(run(Pass::Strip, &inputs(""), &facts("1\n2\n3\n"), &f));
    assert_eq!(levels(&r), ["SUCCESS", "SUCCESS", "SUCCESS"]);
    assert_eq!(
        f.calls()
            .iter()
            .filter(|c| c.contains("remove_label"))
            .cloned()
            .collect::<Vec<_>>(),
        [
            "forge_gh_remove_label_rl_safe o/r 1 loom:building",
            "forge_gh_remove_label_rl_safe o/r 3 loom:building",
            "forge_gh_remove_label_rl_safe o/r 3 loom:issue",
        ]
    );
}

#[test]
fn strip_with_no_close_targets_is_a_quiet_no_op() {
    let f = Fake::default();
    assert!(done(run(Pass::Strip, &inputs(""), &facts(""), &f)).is_empty());
    assert!(f.calls().is_empty());
}

#[test]
fn strip_failed_removal_warns_with_the_manual_command() {
    let mut f = Fake::default();
    f.issues.insert("1".into(), CLOSED_BUILDING.into());
    f.fail_helpers = vec!["forge_gh_remove_label_rl_safe"];
    let r = done(run(Pass::Strip, &inputs(""), &facts("1"), &f));
    assert_eq!(levels(&r), ["WARNING"]);
    assert!(r[0]
        .1
        .contains("gh issue edit 1 --repo o/r --remove-label loom:building"));
}

// ---------------------------------------------------------------------------
// SystemHost: one host, two callers (#15641)
// ---------------------------------------------------------------------------

/// A `gh` stand-in that logs its working directory and argv and answers the
/// reads the bookkeeping makes. Issue `5` fails with an error body — the
/// shell capture's `|| echo '{}'` case.
fn logging_gh(dir: &std::path::Path, name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let log = dir.join(format!("{name}.log"));
    let gh = crate::gh_invocation::test_support::stub(
        dir,
        name,
        r#"printf '%s\n' "$PWD $*" >> "@LOG@"
case "$1 $2" in
  "api repos/o/r/issues/5") echo '{"message":"Not Found"}'; exit 1 ;;
  "api repos/o/r/issues/6") echo '{"state":"open","pull_request":{}}' ;;
  "api repos/o/r/pulls/9/commits") printf '%s\n' '[{"commit":{"message":"a"}}]' '[{"commit":{"message":"b"}}]' ;;
  "api repos/o/r/issues/6/timeline?per_page=100") echo '[{"event":"labeled"}]' ;;
esac"#
            .replace("@LOG@", &log.display().to_string())
            .as_str(),
    );
    (gh, log)
}

/// Everything the passes ask of the forge through the shared host.
fn exercise(host: &SystemHost) {
    // A failed read is the shell capture: the error body, then `{}`.
    assert_eq!(host.read_issue("o/r", "5"), "{\"message\":\"Not Found\"}\n{}");
    assert_eq!(host.issue_kind("o/r", "5"), "");
    assert_eq!(host.issue_kind("o/r", "6"), "pr");
    assert_eq!(host.commit_messages("o/r", "9"), "a\nb");
    assert_eq!(
        host.label_events("o/r", "6").as_deref().map(str::trim),
        Some(r#"[{"event":"labeled"}]"#)
    );
    assert!(host.close_issue("o/r", "6", "closed by the merge"));
}

fn logged(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

/// A logged call without the `$PWD` the stub prefixed.
fn argv(line: &str) -> &str {
    line.split_once(' ').map_or(line, |(_, rest)| rest)
}

/// Both bindings run the same shared code, and each books it under its own
/// caller's operation names — the shell verb's `post_merge.*` rows and the
/// observation pass's `merged_obs.*` rows are the attribution each always
/// had (#15641).
#[test]
#[serial_test::serial]
fn system_host_books_each_binding_under_its_own_operation_names() {
    use crate::gh_invocation::resolver::test_stub::GhBinGuard;
    use crate::gh_invocation::test_support::{calls, rows_after};

    let tmp = tempfile::tempdir().unwrap();
    let (shell_gh, shell_log) = logging_gh(tmp.path(), "gh-shell-verb");
    let (obs_gh, obs_log) = logging_gh(tmp.path(), "gh-observation");
    let rows = {
        let _shell_gh_bin = GhBinGuard::set(&shell_gh);
        rows_after(|| {
            exercise(&SystemHost::for_merge_pr(String::new()));
            exercise(&SystemHost::for_merge_observation(&obs_gh, tmp.path()));
        })
    };
    for (op, n) in [
        ("post_merge.issue_read", 1),
        ("post_merge.issue_kind", 2),
        ("post_merge.commits", 1),
        ("post_merge.timeline", 1),
        ("post_merge.issue_close", 1),
        ("merged_obs.issue_read", 1),
        ("merged_obs.issue_kind", 2),
        ("merged_obs.commits", 1),
        ("merged_obs.timeline", 1),
        ("merged_obs.issue_close", 1),
    ] {
        assert_eq!(calls(&rows, op), n, "{op}: {rows:?}");
    }

    // The same argv from both bindings, each from its own working
    // directory: the shell verb's caller, never a workspace root.
    let (shell, obs) = (logged(&shell_log), logged(&obs_log));
    let expected = [
        "api repos/o/r/issues/5",
        "api repos/o/r/issues/5",
        "api repos/o/r/issues/6",
        "api repos/o/r/pulls/9/commits --paginate",
        "api repos/o/r/issues/6/timeline?per_page=100 --paginate",
        "issue close 6 --repo o/r --reason completed --comment closed by the merge",
    ];
    assert_eq!(shell.iter().map(|l| argv(l)).collect::<Vec<_>>(), expected);
    assert_eq!(obs.iter().map(|l| argv(l)).collect::<Vec<_>>(), expected);
    let cwd = std::env::current_dir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    assert!(
        shell
            .iter()
            .all(|l| l.starts_with(&cwd.display().to_string())),
        "{shell:?}"
    );
    assert!(
        obs.iter()
            .all(|l| l.starts_with(&root.display().to_string())),
        "{obs:?}"
    );
}

/// The two bindings carry their caller's invocation context (#15641): the
/// shell verb resolves `gh` through the facade, runs from the caller's
/// working directory and names the repository on its reads, because that
/// directory is not a checkout of it; the observation pass binds the
/// daemon's `gh` and the workspace root, whose checkout routes its reads.
#[test]
#[serial_test::serial]
fn the_bindings_carry_their_callers_invocation_context() {
    use crate::gh_invocation::resolver::test_stub::GhBinGuard;
    use std::path::{Path, PathBuf};

    let gh = PathBuf::from("/stub/gh");
    let _gh_bin = GhBinGuard::set(&gh);
    let shell_verb = SystemHost::for_merge_pr("/x/lib/forge-helpers.sh".into());
    assert_eq!(shell_verb.gh_bin, gh);
    assert_eq!(shell_verb.gh_cwd, PathBuf::from("."));
    assert_eq!(shell_verb.forge_helpers, PathBuf::from("/x/lib/forge-helpers.sh"));
    assert_eq!(shell_verb.ops, BookkeepingOps::POST_MERGE);
    assert!(shell_verb.reader_repo);
    assert_eq!(shell_verb.helper_cwd, None);

    let root = Path::new("/w/repo");
    let observation = SystemHost::for_merge_observation(Path::new("gh"), root);
    assert_eq!(observation.gh_bin, PathBuf::from("gh"));
    assert_eq!(observation.gh_cwd, root);
    assert_eq!(
        observation.forge_helpers,
        Path::new("/w/repo/.loom/scripts/lib/forge-helpers.sh")
    );
    assert_eq!(observation.ops, BookkeepingOps::MERGED_OBS);
    assert!(!observation.reader_repo);
    assert_eq!(observation.helper_cwd, Some(root.to_path_buf()));
}

/// `helper` sources the binding's `forge-helpers.sh` and exits with the
/// function's status; only the observation pass runs it from the workspace
/// root — the shell verb's helper inherits its caller's working directory
/// (#15641).
#[test]
fn helper_runs_from_the_workspace_only_for_the_observation_binding() {
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("helper.log");
    let script = |lib: &str| {
        format!(
            "forge_fn() {{\n  printf '%s\\n' \"{lib} arg=$1 PWD=$PWD\" >> '{}'\n{}\n}}",
            log.display(),
            "  [ \"$1\" != fail ]"
        )
    };
    let shell_lib = tmp.path().join("shell-forge-helpers.sh");
    std::fs::write(&shell_lib, script("shell")).unwrap();
    let obs_lib = tmp.path().join(".loom/scripts/lib/forge-helpers.sh");
    std::fs::create_dir_all(obs_lib.parent().unwrap()).unwrap();
    std::fs::write(&obs_lib, script("observation")).unwrap();

    let shell_verb = SystemHost::for_merge_pr(shell_lib.display().to_string());
    assert!(shell_verb.helper("forge_fn", &["a"]));
    assert!(!shell_verb.helper("forge_fn", &["fail"]));
    let observation = SystemHost::for_merge_observation(std::path::Path::new("gh"), tmp.path());
    assert!(observation.helper("forge_fn", &["a"]));
    assert!(!observation.helper("forge_fn", &["fail"]));

    let cwd = std::env::current_dir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let (cwd, root) = (cwd.display().to_string(), root.display().to_string());
    assert_eq!(
        logged(&log),
        [
            format!("shell arg=a PWD={cwd}"),
            format!("shell arg=fail PWD={cwd}"),
            format!("observation arg=a PWD={root}"),
            format!("observation arg=fail PWD={root}"),
        ]
    );
}
