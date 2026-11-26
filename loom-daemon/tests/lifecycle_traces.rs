//! Owned Rust launch/tool contracts. Real provider canaries are separate evidence.
#[allow(dead_code)]
mod common;

use loom_daemon::{
    observability::{lifecycle, queue::DurableQueue},
    telemetry::{
        trace::{store::TraceStore, SpanName, SpanStatus, TraceContext},
        TelemetryRecord,
    },
};
use serde_json::json;
use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

#[path = "support/worker_cli.rs"]
mod worker_cli;
use worker_cli::fixture;

fn root() -> tempfile::TempDir {
    common::stub_gh_bin_in_process();
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".loom")).unwrap();
    fs::write(dir.path().join(".loom/config.json"), json!({"observability":{"enabled":true,"exporter":"otlp","endpoint":"http://127.0.0.1:4318"}}).to_string()).unwrap();
    dir
}
fn command(root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
    common::deny_real_gh(&mut cmd);
    cmd.current_dir(root)
        .env("LOOM_WORKSPACE", root)
        .env("LOOM_CONFIG_DEFAULTS_FILE", "")
        .env("LOOM_SHARED_API_KEYS_DIR", "")
        .env_remove("LOOM_ROLE")
        .env_remove("LOOM_MODEL")
        .env_remove("LOOM_MODEL_PROFILE")
        .env("LOOM_OBSERVABILITY_ENABLED", "true")
        .env("LOOM_OBSERVABILITY_EXPORTER", "otlp")
        .env("LOOM_OBSERVABILITY_ENDPOINT", "http://127.0.0.1:4318")
        .env("LOOM_PI_BIN", fixture())
        .env("LOOM_OPENCODE_BIN", fixture())
        .env("FIXTURE_VERSION", "1.18.31")
        .env("FIXTURE_PRINT_ENV", "LOOM_TRACEPARENT,LOOM_TRACE_CONTEXT_FILE")
        .env(
            "LOOM_NATIVE_GUARD_DIR",
            concat!(env!("CARGO_MANIFEST_DIR"), "/../defaults/hooks"),
        );
    cmd
}
fn spans(root: &Path) -> Vec<loom_daemon::telemetry::trace::SpanRecord> {
    let queue = DurableQueue::open(root.join("queue.jsonl"), 1000);
    lifecycle::backfill(root, &queue);
    queue
        .peek_batch(1000)
        .into_iter()
        .filter_map(|e| match e.record {
            TelemetryRecord::Span(span) => Some(span),
            _ => None,
        })
        .collect()
}

#[test]
fn failed_spawn_completes_and_retires_the_prepared_root() {
    let dir = root();
    let root = dir.path();
    let execution = "missing-executable";
    let mut cmd = Command::new(root.join("does-not-exist"));
    loom_daemon::observability::tracing::prepare_child(
        &mut cmd,
        root,
        execution,
        &Default::default(),
    );
    assert!(TraceStore::new(root).path(root, execution).exists());
    let error = lifecycle::spawn_child(&mut cmd, root, execution).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    let records = spans(root);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].name, SpanName::Sweep);
    assert_eq!(records[0].status, SpanStatus::Error);
    assert_eq!(records[0].attributes["loom.result"], "spawn_failed");
    assert!(!TraceStore::new(root).path(root, execution).exists());
}

fn git_repo_with_origin(root: &Path, origin: &str) {
    for args in [&["init", "-q"][..], &["remote", "add", "origin", origin]] {
        assert!(Command::new("git")
            .current_dir(root)
            .args(args)
            .status()
            .unwrap()
            .success());
    }
}

fn issue_sweeps(
    root: &Path,
    executions: &[&str],
    issue: u32,
) -> Vec<loom_daemon::telemetry::trace::SpanRecord> {
    let mut sweeps = Vec::new();
    for execution in executions {
        let mut cmd = Command::new(root.join("does-not-exist"));
        loom_daemon::observability::tracing::prepare_child(
            &mut cmd,
            root,
            execution,
            &loom_daemon::observability::lifecycle::StepKeys::issue(Some(issue)),
        );
        lifecycle::spawn_child(&mut cmd, root, execution).unwrap_err();
        sweeps.extend(
            spans(root).into_iter().filter(|s| {
                s.name == SpanName::Sweep && s.attributes["loom.sweep_id"] == *execution
            }),
        );
        // The drained execution retires instead of leaking its context.
        assert!(!TraceStore::new(root).path(root, execution).exists());
    }
    sweeps
}

#[test]
fn issue_execution_is_its_own_root_and_still_retires() {
    let dir = root();
    let root = dir.path();
    git_repo_with_origin(root, "git@github.com:RJWalters/Loom.git");
    loom_daemon::telemetry::repo_identity::seed(
        "RJWalters/Loom",
        loom_daemon::telemetry::repo_identity::parse("1073994527 rjwalters/loom"),
    );
    let sweeps = issue_sweeps(root, &["issue-attempt-1", "issue-attempt-2"], 9038);
    assert_eq!(sweeps.len(), 2);
    for sweep in &sweeps {
        assert_eq!(sweep.parent_span_id, None);
        assert_eq!(sweep.attributes["loom.issue"], "9038");
        assert_eq!(sweep.attributes["loom.repo"], "rjwalters/loom");
        for key in ["loom.story_id", "loom.story", "loom.story.key_version"] {
            assert!(!sweep.attributes.contains_key(key), "{key} must be absent");
        }
    }
    assert_ne!(sweeps[0].context.trace_id, sweeps[1].context.trace_id);
    assert_ne!(sweeps[0].context.span_id, sweeps[1].context.span_id);
}

/// #10637: a sweep's `loom.repo` is GitHub's spelling of the repo (the one
/// `loom.dispatch.*` carries), not the origin's, and never lowercased.
#[test]
fn issue_sweep_names_its_repo_as_github_spells_it() {
    let dir = root();
    let root = dir.path();
    git_repo_with_origin(root, "git@github.com:twoam-fixture/story-case.git");
    loom_daemon::telemetry::repo_identity::seed(
        "twoam-fixture/story-case",
        loom_daemon::telemetry::repo_identity::parse("4242 TwoAM-Fixture/Story-Case"),
    );
    let sweeps = issue_sweeps(root, &["case-attempt-1"], 10637);
    assert_eq!(sweeps.len(), 1);
    assert_eq!(sweeps[0].attributes["loom.repo"], "TwoAM-Fixture/Story-Case");
}

#[test]
fn unresolvable_repo_identity_still_names_the_issue() {
    let dir = root();
    let root = dir.path();
    git_repo_with_origin(root, "git@github.com:unresolvable-owner/no-repo-id.git");
    loom_daemon::telemetry::repo_identity::seed("unresolvable-owner/no-repo-id", None);
    let sweeps = issue_sweeps(root, &["orphan-attempt-1", "orphan-attempt-2"], 7);
    assert_eq!(sweeps.len(), 2);
    for sweep in &sweeps {
        assert_eq!(sweep.parent_span_id, None);
        for key in ["loom.story_id", "loom.story", "loom.story.key_version"] {
            assert!(!sweep.attributes.contains_key(key), "{key} must be absent");
        }
        // #11982: the issue is a plain fact, whatever the identity lookup said.
        assert_eq!(sweep.attributes["loom.issue"], "7");
    }
    assert_ne!(sweeps[0].context.trace_id, sweeps[1].context.trace_id);
}

#[test]
fn successful_process_exit_does_not_imply_independent_task_acceptance() {
    let dir = root();
    let root = dir.path();
    let execution = "independent-verification";
    let sweep = lifecycle::begin(root, execution, SpanName::Sweep, Default::default()).unwrap();
    let runtime = sweep
        .child(SpanName::RuntimeRun, Default::default())
        .unwrap();
    lifecycle::child_spawned(root, execution, std::process::id());
    lifecycle::child_exited(root, execution, "success");
    lifecycle::finish_execution(root, execution, "verification_failed", Default::default());
    let records = spans(root);
    let observed_runtime = records
        .iter()
        .find(|s| s.context == *runtime.context())
        .unwrap();
    assert_eq!(observed_runtime.status, SpanStatus::Ok);
    assert_eq!(observed_runtime.attributes["loom.result"], "success");
    let observed_root = records
        .iter()
        .find(|s| s.context == *sweep.context())
        .unwrap();
    assert_eq!(observed_root.status, SpanStatus::Error);
    assert_eq!(observed_root.attributes["loom.result"], "verification_failed");
}

#[cfg(unix)]
#[test]
fn backfill_after_worker_exit_preserves_the_supervisors_authoritative_result() {
    for adopted in [false, true] {
        let dir = root();
        let root = dir.path();
        let execution = "supervised-exit";
        let sweep = lifecycle::begin(root, execution, SpanName::Sweep, Default::default()).unwrap();
        let runtime = sweep
            .child(SpanName::RuntimeRun, Default::default())
            .unwrap();
        let mut child =
            lifecycle::spawn_child(&mut Command::new("/usr/bin/true"), root, execution).unwrap();
        let context_path = TraceStore::new(root).path(root, execution);
        let journal = loom_daemon::telemetry::trace::journal::Journal::for_context(&context_path);
        journal.set_owner(runtime.context(), child.id()).unwrap();
        assert!(child.wait().unwrap().success());
        if adopted {
            // A previous supervisor is gone; the new authoritative supervisor
            // must renew protection without replacing the observed worker PID.
            journal.set_supervisor(sweep.context(), child.id()).unwrap();
            journal
                .set_supervisor(runtime.context(), child.id())
                .unwrap();
            lifecycle::execution_adopted(root, execution);
        }
        let queue = DurableQueue::open(root.join("supervised-queue.jsonl"), 1000);
        assert_eq!(lifecycle::backfill(root, &queue), 0);
        assert!(context_path.exists(), "backfill must not retire supervised work");
        assert_eq!(journal.active().unwrap().len(), 2);
        lifecycle::child_exited(root, execution, "success");
        // Independent verification can take time after the child was reaped.
        lifecycle::backfill(root, &queue);
        assert!(context_path.exists());
        lifecycle::finish_execution(root, execution, "verification_failed", Default::default());
        lifecycle::backfill(root, &queue);
        let records: Vec<_> = queue
            .peek_batch(1000)
            .into_iter()
            .filter_map(|e| match e.record {
                TelemetryRecord::Span(span) => Some(span),
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 2);
        let child_record = records
            .iter()
            .find(|s| s.context == *runtime.context())
            .unwrap();
        assert_eq!(child_record.status, SpanStatus::Ok);
        let result = records
            .iter()
            .find(|s| s.context == *sweep.context())
            .unwrap();
        assert_eq!(result.status, SpanStatus::Error);
        assert_eq!(result.attributes["loom.result"], "verification_failed");
        assert!(records
            .iter()
            .all(|s| !s.attributes.contains_key("loom.recovered")));
        assert!(!context_path.exists(), "retirement follows the actual terminal result");
    }
}

#[test]
fn pi_and_opencode_launch_and_native_read_preserve_authoritative_context() {
    for runtime in ["pi", "opencode"] {
        let dir = root();
        let root = dir.path();
        assert!(Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .arg(root)
            .status()
            .unwrap()
            .success());
        let sweep = lifecycle::begin(
            root,
            "fixture-sweep",
            SpanName::Sweep,
            lifecycle::attributes(&[("loom.issue", "18")]),
        )
        .unwrap();
        let mut cmd = command(root);
        cmd.args(["spawn-worker", "--", "-p", "PRIVATE_PROMPT_SENTINEL"])
            .env("LOOM_RUNTIME", runtime);
        sweep.command(&mut cmd);
        let output = cmd.output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8(output.stdout).unwrap();
        let inherited = text
            .lines()
            .find_map(|l| l.strip_prefix("child_env LOOM_TRACEPARENT="))
            .unwrap();
        let context = TraceContext::parse(inherited).unwrap();
        assert_eq!(context.trace_id, sweep.context().trace_id);
        assert_ne!(context.span_id, sweep.context().span_id);
        fs::write(root.join("sample.txt"), "PRIVATE_SOURCE_SENTINEL").unwrap();
        let mut tool = command(root);
        tool.args(["runtime-tool", "--workspace"])
            .arg(root)
            .arg("--cwd")
            .arg(root)
            .env("LOOM_TRACEPARENT", inherited)
            .env("LOOM_TRACE_CONTEXT_FILE", TraceStore::new(root).path(root, "fixture-sweep"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = tool.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(
                json!({"tool":"read","input":{"path":"sample.txt"}})
                    .to_string()
                    .as_bytes(),
            )
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        lifecycle::finish_execution(root, "fixture-sweep", "success", Default::default());
        let records = spans(root);
        assert_eq!(records.len(), 4, "{records:?}");
        let run = records
            .iter()
            .find(|s| s.name == SpanName::RuntimeRun)
            .unwrap();
        assert_eq!(run.context, context);
        assert_eq!(run.attributes["loom.runtime"], runtime);
        assert_eq!(run.attributes["loom.model"], "glm-5.3-flash");
        let tool = records.iter().find(|s| s.name == SpanName::Tool).unwrap();
        assert_eq!(tool.parent_span_id.as_ref(), Some(&context.span_id));
        assert_eq!(tool.attributes["loom.tool.name"], "read");
        let bytes = serde_json::to_string(&records).unwrap();
        assert!(!bytes.contains("PRIVATE_PROMPT_SENTINEL"));
        assert!(!bytes.contains("PRIVATE_SOURCE_SENTINEL"));
        assert!(
            !TraceStore::new(root).path(root, "fixture-sweep").exists(),
            "completed context retires only after durable backfill"
        );
    }
}

#[test]
fn rejected_preflight_has_no_runtime_run_and_distinct_attempt_failure() {
    let dir = root();
    let root = dir.path();
    let roles = root.join(".loom/roles");
    fs::create_dir_all(&roles).unwrap();
    fs::write(roles.join("builder.json"), r#"{"runtimeRequirements":["mcp"]}"#).unwrap();
    let sweep = lifecycle::begin(root, "rejection", SpanName::Sweep, Default::default()).unwrap();
    let mut cmd = command(root);
    cmd.args(["spawn-worker", "--", "-p", "/loom:builder 18"])
        .env("LOOM_RUNTIME", "pi");
    sweep.command(&mut cmd);
    assert_eq!(cmd.output().unwrap().status.code(), Some(78));
    lifecycle::finish_execution(root, "rejection", "failure", Default::default());
    let records = spans(root);
    assert!(!records.iter().any(|s| s.name == SpanName::RuntimeRun));
    assert!(records
        .iter()
        .any(|s| s.name == SpanName::RuntimePreflight && s.status == SpanStatus::Error));
    assert!(records
        .iter()
        .any(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.result"] == "rejected"));
}

#[test]
fn identical_issue_numbers_in_distinct_roots_keep_distinct_trace_ids_after_reload() {
    let a = root();
    let b = root();
    let one = lifecycle::begin(a.path(), "issue-18", SpanName::Sweep, Default::default()).unwrap();
    let two = lifecycle::begin(b.path(), "issue-18", SpanName::Sweep, Default::default()).unwrap();
    assert_ne!(one.context().trace_id, two.context().trace_id);
    let restored =
        lifecycle::begin(a.path(), "issue-18", SpanName::Sweep, Default::default()).unwrap();
    assert_eq!(one.context(), restored.context());
    lifecycle::finish_execution(a.path(), "issue-18", "cancelled", Default::default());
    lifecycle::finish_execution(b.path(), "issue-18", "crashed", Default::default());
    assert_eq!(spans(a.path())[0].attributes["loom.result"], "cancelled");
    assert_eq!(spans(b.path())[0].attributes["loom.result"], "crashed");
}

#[test]
fn actual_checkpoint_cli_preserves_rapid_judge_doctor_repair_waterfall() {
    let dir = root();
    let root = dir.path();
    // The scope `prepare_execution` stamps on a sweep root (#10637).
    let scope = lifecycle::attributes(&[
        ("loom.repo", "TwoAM-Fixture/Repair"),
        ("loom.sweep_id", "repair"),
    ]);
    let sweep = lifecycle::begin(root, "repair", SpanName::Sweep, scope.clone()).unwrap();
    for (phase, attempt) in [
        ("builder-done", "1"),
        ("judge-rejected", "1"),
        ("doctor-done", "2"),
        ("judge-done", "2"),
        ("merge-done", "2"),
    ] {
        let mut cmd = command(root);
        cmd.args([
            "sweep-checkpoint",
            "write",
            "18",
            phase,
            "--pr-number",
            "42",
            "--attempt",
            attempt,
        ]);
        sweep.command(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    lifecycle::finish_execution(root, "repair", "success", Default::default());
    let records = spans(root);
    assert_eq!(records.len(), 11);
    let judges: Vec<_> = records
        .iter()
        .filter(|s| s.name == SpanName::RoleAttempt && s.attributes["loom.role"] == "judge")
        .collect();
    assert_eq!(judges.len(), 2);
    assert_eq!(judges[0].status, SpanStatus::Error);
    assert_eq!(judges[1].status, SpanStatus::Ok);
    assert_ne!(judges[0].context.span_id, judges[1].context.span_id);
    assert!(judges.iter().all(|s| s.started_at == s.ended_at
        && s.attributes["loom.timing_source"] == "checkpoint_write_observed"));
    // #10637: every checkpoint span names its execution's repo and sweep.
    for span in records.iter().filter(|s| s.name != SpanName::Sweep) {
        for (key, value) in &scope {
            assert_eq!(span.attributes.get(key), Some(value), "{key} on {:?}", span.name);
        }
    }
}

/// #14167: with no inherited context (tracing disabled), the scope a child
/// can stamp is the repo alone — the item stays absent rather than guessed.
#[test]
fn a_child_without_an_inherited_context_stamps_only_the_repo() {
    let dir = root();
    let root = dir.path();
    let scope = lifecycle::inherited_scope(root);
    assert!(scope.contains_key("loom.repo"));
    assert!(!scope.contains_key("loom.issue"));
    assert!(!scope.contains_key("loom.pr_number"));
    assert!(!scope.contains_key("loom.sweep_id"));
    assert!(!scope.contains_key("loom.role"));
}

// Fail this test binary on any spawn of the real `gh` (#10225, extends #10138).
#[allow(dead_code)]
#[path = "../src/live_gh_guard.rs"]
mod live_gh_guard;

/// #14167: every runtime's `loom.runtime.run` span carries the identity facts
/// — role, repo, item and sweep id, stamped from the inherited execution
/// root and the pinned environment — and only the launch facts its runtime
/// actually resolves: a native harness its profile selection, a legacy
/// adapter its documented precedence (the handed-over `--model`/`--effort`
/// argv, then the `LOOM_MODEL` / `LOOM_CODEX_MODEL` / `LOOM_EFFORT` env
/// tier). A runtime that reports no launched model leaves `loom.model`
/// ABSENT. Only the Codex seat attribute may name an account (#15291).
#[test]
fn runtime_run_spans_carry_identity_and_only_reported_launch_facts() {
    // Native (opencode): identity plus the selection it launched with.
    {
        let dir = root();
        let root = dir.path();
        // A role-tagged launch is admitted against its role manifest; the
        // fixture role carries no runtime requirements.
        let roles = root.join(".loom/roles");
        fs::create_dir_all(&roles).unwrap();
        fs::write(roles.join("builder.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
        let sweep = lifecycle::begin(
            root,
            "identity-sweep",
            SpanName::Sweep,
            lifecycle::attributes(&[
                ("loom.repo", "acme/widget"),
                ("loom.issue", "18"),
                ("loom.sweep_id", "identity-sweep"),
                ("loom.role", "builder"),
            ]),
        )
        .unwrap();
        let mut cmd = command(root);
        cmd.args([
            "spawn-worker",
            "--",
            "-p",
            "hello",
            "--model",
            "example/model-v2",
            "--effort",
            "high",
        ])
        .env("LOOM_RUNTIME", "opencode")
        .env("LOOM_ROLE", "builder")
        .env("LOOM_SWEEP_ID", "identity-sweep")
        .env_remove("LOOM_EFFORT");
        sweep.command(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        lifecycle::finish_execution(root, "identity-sweep", "success", Default::default());
        let run = spans(root)
            .into_iter()
            .find(|s| s.name == SpanName::RuntimeRun)
            .unwrap();
        assert_eq!(run.attributes["loom.runtime"], "opencode");
        assert_eq!(run.attributes["loom.role"], "builder");
        assert_eq!(run.attributes["loom.repo"], "acme/widget");
        assert_eq!(run.attributes["loom.issue"], "18");
        assert_eq!(run.attributes["loom.sweep_id"], "identity-sweep");
        // The opencode adapter splits `provider/model`; the span names the
        // model half it launched (the provider rides `loom.provider`).
        assert_eq!(run.attributes["loom.model"], "model-v2");
        assert_eq!(run.attributes["loom.provider"], "example");
        assert_eq!(run.attributes["loom.effort"], "high");
    }
    // Legacy claude/codex: the handed-over argv, the env tier, or absence.
    // `env_effort` is `LOOM_EFFORT`; `expect_effort` is `None` for "absent".
    type LegacyRow = (
        &'static str,
        bool,
        &'static str,
        Option<&'static str>,
        &'static str,
        Option<&'static str>,
        &'static [&'static str],
    );
    const UNRELATED_CONFIG: &str =
        r#"--config=developer_instructions="model_reasoning_effort=low""#;
    let legacy_rows: [LegacyRow; 14] = [
        ("claude", true, "", Some("sonnet"), "", Some("high"), &[]),
        ("codex", true, "", Some("gpt-5.1"), "", Some("high"), &[]),
        // Nothing on argv and nothing in the env tier: absent, never the
        // configured value (there is none to fill it with here).
        ("claude", false, "", None, "", None, &[]),
        // Codex's static per-seat default is the env tier of last resort.
        ("codex", false, "LOOM_CODEX_MODEL", Some("seat-model"), "", None, &[]),
        // #14302: the adapter splits `model@effort` on the first `@`; the
        // span records the base as the model and the suffix as the effort.
        (
            "codex",
            false,
            "LOOM_CODEX_MODEL=gpt-5-codex@high",
            Some("gpt-5-codex"),
            "",
            Some("high"),
            &[],
        ),
        // `LOOM_EFFORT` outranks the suffix, as in the adapter.
        (
            "codex",
            false,
            "LOOM_CODEX_MODEL=gpt-5-codex@high",
            Some("gpt-5-codex"),
            "low",
            Some("low"),
            &[],
        ),
        // An empty suffix is no effort; an empty base is no model.
        (
            "codex",
            false,
            "LOOM_CODEX_MODEL=gpt-5-codex@",
            Some("gpt-5-codex"),
            "",
            None,
            &[],
        ),
        ("codex", false, "LOOM_CODEX_MODEL=@high", None, "", None, &[]),
        // An explicit model arg suppresses the env split (and its suffix).
        (
            "codex",
            true,
            "LOOM_CODEX_MODEL=seat@low",
            Some("gpt-5.1"),
            "",
            Some("high"),
            &[],
        ),
        // #14414: an explicit `-c model_reasoning_effort=` outranks `--effort`
        // and `LOOM_EFFORT`, with or without a model arg, and the span records
        // what the adapter launches rather than the lower-tier value.
        (
            "codex",
            true,
            "",
            Some("gpt-5.1"),
            "",
            Some("low"),
            &["-c", "model_reasoning_effort=low"],
        ),
        // An unrelated inline value suppresses the launcher's fallback but
        // supplies no effort of its own. A genuine assignment still wins.
        ("codex", true, "", Some("gpt-5.1"), "", None, &[UNRELATED_CONFIG]),
        (
            "codex",
            true,
            "",
            Some("gpt-5.1"),
            "",
            Some("medium"),
            &["-c", "model_reasoning_effort=medium", UNRELATED_CONFIG],
        ),
        (
            "codex",
            true,
            "",
            Some("gpt-5.1"),
            "",
            Some("medium"),
            &[
                UNRELATED_CONFIG,
                "--config",
                "model_reasoning_effort=medium",
            ],
        ),
        (
            "codex",
            false,
            "LOOM_CODEX_MODEL=gpt-5-codex@high",
            Some("gpt-5-codex"),
            "medium",
            Some("low"),
            &["-c", "model_reasoning_effort=low"],
        ),
    ];
    for (runtime, model_arg, env_tier, expect_model, env_effort, expect_effort, extra_args) in
        legacy_rows
    {
        let dir = root();
        let root = dir.path();
        let scripts = root.join(".loom/scripts");
        fs::create_dir_all(&scripts).unwrap();
        fs::copy(fixture(), scripts.join(format!("spawn-{runtime}.sh"))).unwrap();
        let roles = root.join(".loom/roles");
        fs::create_dir_all(&roles).unwrap();
        fs::write(roles.join("builder.json"), r#"{"runtimeRequirements":[]}"#).unwrap();
        let sweep = lifecycle::begin(
            root,
            "legacy-sweep",
            SpanName::Sweep,
            lifecycle::attributes(&[
                ("loom.repo", "acme/widget"),
                ("loom.issue", "18"),
                ("loom.sweep_id", "legacy-sweep"),
            ]),
        )
        .unwrap();
        let mut cmd = command(root);
        cmd.arg("spawn-worker").arg("--").arg("-p").arg("hello");
        if model_arg {
            let model = if runtime == "codex" {
                "gpt-5.1"
            } else {
                "sonnet"
            };
            cmd.args(["--model", model, "--effort", "high"]);
        }
        cmd.args(extra_args);
        cmd.env("LOOM_RUNTIME", runtime)
            .env("LOOM_ROLE", "builder")
            .env("LOOM_SWEEP_ID", "legacy-sweep")
            // The explicitly pinned fixture names its seat. A span
            // carries the pool KIND, never this name -- with one sanctioned
            // exception (#15291): a Codex launch stamps it as the value of
            // `loom.codex.account` and nowhere else.
            .env("LOOM_ACCOUNT_PROVIDER", "codex")
            .env("LOOM_ACCOUNT_NAME", "SECRET-ACCOUNT-NAME")
            .env_remove("LOOM_MODEL")
            .env_remove("LOOM_EFFORT");
        if !env_effort.is_empty() {
            cmd.env("LOOM_EFFORT", env_effort);
        }
        if runtime == "codex" {
            // #8787's private-clone containment is pool machinery no fixture
            // has; the adapter's explicit-credential env bypasses it.
            cmd.env("LOOM_CODEX_NO_EXEC", "1")
                .env_remove("CODEX_HOME")
                .env_remove("LOOM_CODEX_HOME")
                .env_remove("LOOM_SPAWN_NO_EXPORT")
                .env("LOOM_CODEX_PROFILE", "SECRET-ACCOUNT-NAME");
        }
        if env_tier == "LOOM_CODEX_MODEL" {
            cmd.env("LOOM_CODEX_MODEL", "seat-model");
        } else if let Some(value) = env_tier.strip_prefix("LOOM_CODEX_MODEL=") {
            cmd.env("LOOM_CODEX_MODEL", value);
        } else {
            cmd.env_remove("LOOM_CODEX_MODEL");
        }
        sweep.command(&mut cmd);
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{runtime}: {}", String::from_utf8_lossy(&out.stderr));
        lifecycle::finish_execution(root, "legacy-sweep", "success", Default::default());
        let records = spans(root);
        let run = records
            .iter()
            .find(|s| s.name == SpanName::RuntimeRun)
            .unwrap();
        assert_eq!(run.attributes["loom.runtime"], runtime, "{runtime}");
        assert_eq!(run.attributes["loom.role"], "builder", "{runtime}");
        assert_eq!(run.attributes["loom.repo"], "acme/widget", "{runtime}");
        assert_eq!(run.attributes["loom.issue"], "18", "{runtime}");
        assert_eq!(run.attributes["loom.sweep_id"], "legacy-sweep", "{runtime}");
        assert_eq!(run.attributes.get("loom.model").map(String::as_str), expect_model, "{runtime}");
        assert_eq!(
            run.attributes.get("loom.effort").map(String::as_str),
            expect_effort,
            "{runtime} {env_tier} effort"
        );
        // The name may appear only as the value of `loom.codex.account`, and
        // only on a Codex launch: strip that one attribute, then the name must
        // be gone everywhere else (any other attribute, any other span).
        let mut value = serde_json::to_value(&records).unwrap();
        let mut seat_spans = 0;
        for record in value.as_array_mut().unwrap() {
            if let Some(seat) = record["attributes"]
                .as_object_mut()
                .and_then(|attrs| attrs.remove("loom.codex.account"))
            {
                assert_eq!(runtime, "codex", "{runtime} carries a Codex seat");
                assert_eq!(seat, "SECRET-ACCOUNT-NAME", "{runtime} seat value");
                seat_spans += 1;
            }
        }
        if runtime == "codex" {
            assert!(seat_spans > 0, "codex carries no seat");
        } else {
            assert_eq!(seat_spans, 0, "{runtime} carries a Codex seat");
        }
        assert!(
            !value.to_string().contains("SECRET-ACCOUNT-NAME"),
            "{runtime} leaks an account name outside loom.codex.account"
        );
    }
}

/// Compare the account seen by the CLI behind the shipped adapter to all
/// three exported span types, on both sides of the private-inventory branch.
#[test]
fn runtime_spans_follow_the_account_launched_by_the_codex_adapter() {
    use loom_daemon::observability::runtime_usage::journal_usage;
    use loom_daemon::script_helpers::sweep_experiment::ModelUsageTotals;
    use std::os::unix::fs::symlink;

    for binding in ["pool", "mixed", "profile", "home"] {
        for parent in [None, Some("agent-9")] {
            let repo = root();
            let root = repo.path();
            let home = tempfile::tempdir().unwrap();
            let profiles = home.path().join("profiles");
            let profile = profiles.join("credential-dir");
            fs::create_dir_all(&profile).unwrap();
            fs::write(profile.join("auth.json"), r#"{"synthetic":true}"#).unwrap();
            let mut accounts = vec![
                json!({"provider":"codex","name":"agent-2","credential_kind":"codex_home","credential_reference":"credential-dir","enabled":true}),
            ];
            if binding == "mixed" {
                accounts.push(json!({"provider":"codex","name":"private","credential_kind":"codex_home","credential_reference":"private","enabled":true}));
                // A private sibling makes Selection::prepare resolve the
                // named shared seat; the private seat itself is never used.
                fs::create_dir(profiles.join("private")).unwrap();
                fs::write(profiles.join("private/auth.json"), r#"{"synthetic":true}"#).unwrap();
                let state = profiles.join(".private-sessions/private");
                fs::create_dir_all(&state).unwrap();
                fs::write(state.join("workspace.json"), "{}").unwrap();
            }
            fs::write(
                root.join(".loom/accounts.json"),
                json!({"version":1,"accounts":accounts}).to_string(),
            )
            .unwrap();
            symlink(fixture(), home.path().join("codex")).unwrap();
            let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts");
            let execution = "codex-seat-test";
            let sweep =
                lifecycle::begin(root, execution, SpanName::Sweep, Default::default()).unwrap();
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_loom-daemon"));
            cmd.current_dir(root)
                .env_clear()
                .args(["spawn-worker", "--scripts-dir"])
                .arg(&scripts)
                .args(["--", "-p", "hello"])
                .env("HOME", home.path())
                .env(
                    "PATH",
                    std::env::join_paths(
                        std::iter::once(home.path().to_owned())
                            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
                    )
                    .unwrap(),
                )
                .env("LOOM_WORKSPACE", root)
                .env("LOOM_CONFIG_DEFAULTS_FILE", "")
                .env("LOOM_RUNTIME", "codex")
                .env("LOOM_CODEX_PROFILE_ROOT", &profiles)
                .env("LOOM_DAEMON_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
                .env("LOOM_DAEMON_SELF_BIN", env!("CARGO_BIN_EXE_loom-daemon"))
                .env("LOOM_SWEEP_NICE", "0")
                .env("LOOM_CODEX_NO_CAPTURE", "1")
                .env("FIXTURE_PRINT_ENV", "CODEX_HOME,LOOM_ACCOUNT_NAME,LOOM_ACCOUNT_PROVIDER");
            match binding {
                "mixed" => {
                    cmd.env("LOOM_CODEX_PROFILE", "agent-2");
                }
                "profile" => {
                    cmd.env("LOOM_CODEX_PROFILE", "credential-dir");
                }
                "home" => {
                    cmd.env("CODEX_HOME", &profile);
                }
                _ => {}
            }
            if let Some(name) = parent {
                cmd.env("LOOM_ACCOUNT_NAME", name)
                    .env("LOOM_ACCOUNT_PROVIDER", "codex");
            }
            sweep.command(&mut cmd);
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let stdout = String::from_utf8(out.stdout).unwrap();
            let child = stdout
                .lines()
                .find_map(|line| line.strip_prefix("child_env LOOM_ACCOUNT_NAME="))
                .unwrap();
            assert_eq!(child, "agent-2", "binding {binding}, parent {parent:?}");
            assert!(stdout
                .lines()
                .any(|line| line == format!("child_env CODEX_HOME={}", profile.display())));
            assert!(stdout
                .lines()
                .any(|line| line == "child_env LOOM_ACCOUNT_PROVIDER=codex"));

            lifecycle::finish_execution(root, execution, "success", Default::default()).unwrap();
            let now = chrono::Utc::now();
            let totals = ModelUsageTotals {
                model: "gpt-test".into(),
                input: 1,
                ..Default::default()
            };
            journal_usage(root, execution, (now, now), Some(&[totals]), Some("codex")).unwrap();
            let records = spans(root);
            for name in [
                SpanName::RuntimePreflight,
                SpanName::RuntimeRun,
                SpanName::RuntimeUsage,
            ] {
                let matching: Vec<_> = records.iter().filter(|s| s.name == name).collect();
                assert_eq!(matching.len(), 1, "{name:?}");
                let span = matching[0];
                if name == SpanName::RuntimePreflight {
                    assert_eq!(span.attributes["loom.result"], "accepted");
                }
                assert_eq!(
                    span.attributes
                        .get("loom.codex.account")
                        .map(String::as_str),
                    Some(child),
                    "{name:?}, binding {binding}, parent {parent:?}"
                );
            }
        }
    }
}
