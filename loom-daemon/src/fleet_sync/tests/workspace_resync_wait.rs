//! Tests for the per-pass disposition of a stale workspace the pass did not
//! resync (#14848): every W1/W3 workspace a Write pass leaves behind gets
//! exactly one WARN per pass carrying why — the deferral that kept the write
//! half from it, or the reason its attempt, backoff or skip already recorded
//! — so a dispatch hold and its cause read together.

use super::*;
use crate::test_log_capture::capture_logs;

/// The WARN lines, of one pass's capture, that name a workspace as not
/// resynced this pass.
fn waits(logs: &[(log::Level, String)]) -> Vec<&(log::Level, String)> {
    logs.iter()
        .filter(|(_, m)| m.contains("not resynced this pass"))
        .collect()
}

#[test]
fn an_offline_pass_warns_for_every_stale_workspace_each_pass_and_resynces_when_back() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let floor = Some("0.19.850");
    // Three remotes that do not answer end the pass's asking mid-classify;
    // the stale workspace behind them is classified from its clone and never
    // offered the attempt — the shape of the four held repos.
    let dead: Vec<PathBuf> = (0..3)
        .map(|n| {
            let root = fx.clone_as(&format!("dead-{n}"));
            git(&root, &["remote", "set-url", "origin", "/nonexistent/origin.git"]);
            root
        })
        .collect();
    let mut roots = dead.clone();
    roots.push(host.root.clone());
    let commits = fx.origin_commits();

    for pass_n in 0..2 {
        let (logs, pass) =
            capture_logs_pass(|| host.pass_over(&roots, Mode::Write, &|| Ok(()), floor));
        // The three unreachable ones carry their backoff; the fourth names
        // the offline scan. One WARN each, every pass.
        let lines = waits(&logs);
        assert_eq!(lines.len(), 4, "pass {pass_n}: {logs:?}");
        assert!(lines.iter().all(|(l, _)| *l == log::Level::Warn), "{logs:?}");
        assert_eq!(
            lines
                .iter()
                .filter(|(_, m)| m.contains("backoff until"))
                .count(),
            3,
            "pass {pass_n}: {logs:?}"
        );
        let offline = lines
            .iter()
            .filter(|(_, m)| m.contains("the pass asked no remote"))
            .count();
        assert_eq!(offline, 1, "pass {pass_n}: {logs:?}");
        // The report carries the current-pass reason too.
        let held = &pass.workspaces[3];
        assert_eq!(held.state, WState::W3, "{held:?}");
        assert_eq!(reason(held), "not attempted this pass: the pass asked no remote");
        assert_eq!(fx.origin_commits(), commits, "pass {pass_n}: nothing resynced");
        host.advance(INTERVAL);
    }

    // The remotes answer again, past every backoff and the outage hold: the
    // pass is online, and the stale workspace is attempted — one resync
    // commit lands.
    host.advance(Duration::from_secs(7 * 60 * 60));
    for root in &dead {
        git(root, &["remote", "set-url", "origin", &fx.origin.to_string_lossy()]);
    }
    let (logs, pass) = capture_logs_pass(|| host.pass_over(&roots, Mode::Write, &|| Ok(()), floor));
    let states: Vec<WState> = pass.workspaces.iter().map(|w| w.state).collect();
    assert_eq!(states, vec![WState::W0, WState::W3, WState::W3, WState::W3], "{pass:?}");
    assert_eq!(fx.origin_commits(), commits + 1, "the resync was attempted");
    // One attempt per pass: the rest name the slot it took.
    let lines = waits(&logs);
    assert_eq!(lines.len(), 3, "{logs:?}");
    assert!(
        lines
            .iter()
            .all(|(_, m)| m.contains("one attempt went to an earlier stale workspace")),
        "{logs:?}"
    );
    for deferred in &pass.workspaces[1..] {
        assert_eq!(
            reason(deferred),
            "not attempted this pass: the pass's one attempt went to an earlier stale workspace",
            "{deferred:?}"
        );
    }
}

#[test]
fn a_pass_past_its_deadline_names_it_for_each_stale_workspace() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let commits = fx.origin_commits();

    host.overdue.set(true);
    let (logs, pass) =
        capture_logs_pass(|| host.pass_with(Mode::Write, &|| Ok(()), Some("0.19.850")));
    let lines = waits(&logs);
    assert_eq!(lines.len(), 1, "{logs:?}");
    assert!(lines[0].1.contains("the pass was past its deadline"), "{logs:?}");
    let report = only(&pass);
    assert_eq!(report.state, WState::W3, "{report:?}");
    assert_eq!(reason(report), "not attempted this pass: the pass was past its deadline");
    assert!(fx.forge.calls.borrow().is_empty(), "no claim past the deadline");
    assert_eq!(fx.origin_commits(), commits);

    // The next pass is within the deadline and resyncs it.
    host.overdue.set(false);
    let (logs, pass) =
        capture_logs_pass(|| host.pass_with(Mode::Write, &|| Ok(()), Some("0.19.850")));
    assert!(waits(&logs).is_empty(), "nothing left stale: {logs:?}");
    assert_eq!(only(&pass).state, WState::W0, "{pass:?}");
    assert_eq!(fx.origin_commits(), commits + 1);
}

#[test]
fn a_stale_workspace_the_classification_budget_skipped_carries_the_deferral() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let commits = fx.origin_commits();

    // A Check pass settles the stale verdict without touching the write half.
    let check = host.pass_with(Mode::Check, &|| Ok(()), None);
    assert_eq!(only(&check).state, WState::W1);
    assert_eq!(reason(only(&check)), "");

    // The next Write pass spends its classification budget before the
    // workspace: the carried verdict gets a current-pass reason and one WARN.
    host.budget.set(0);
    let (logs, pass) = capture_logs_pass(|| host.pass());
    let lines = waits(&logs);
    assert_eq!(lines.len(), 1, "{logs:?}");
    assert!(
        lines[0]
            .1
            .contains("the classification budget was spent before it"),
        "{logs:?}"
    );
    let report = only(&pass);
    assert_eq!(report.state, WState::W1, "{report:?}");
    assert_eq!(
        reason(report),
        "not checked this pass: the classification budget was spent before it; it keeps its \
         last verdict and is first next pass"
    );
    assert_eq!(fx.origin_commits(), commits, "nothing resynced from a carried verdict");

    // With the budget back, the same workspace reaches the push path.
    host.budget.set(u32::MAX);
    let (logs, pass) = capture_logs_pass(|| host.pass());
    assert!(waits(&logs).is_empty(), "{logs:?}");
    assert_eq!(only(&pass).state, WState::W0, "{pass:?}");
    assert_eq!(fx.origin_commits(), commits + 1);
}

#[test]
fn an_attempt_decline_is_warned_with_its_reason_each_pass() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    write(&host.root.join(".read-only-credential"), "");
    for pass_n in 0..2 {
        let (logs, pass) = capture_logs_pass(|| host.pass());
        let lines = waits(&logs);
        assert_eq!(lines.len(), 1, "pass {pass_n}: {logs:?}");
        assert!(lines[0].1.contains("not written from this host"), "{logs:?}");
        let report = only(&pass);
        assert_eq!(report.state, WState::W1, "{report:?}");
        assert_eq!(
            reason(report),
            "not written from this host: the credential has READ on acme/app, not WRITE"
        );
    }
    assert!(fx.forge.calls.borrow().is_empty());
}

#[test]
fn a_claim_held_by_another_host_is_warned_with_its_reason() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let held = fx
        .forge
        .add_commit(&claim_message("host-x", RUNNING, host.now.get()), &[]);
    fx.forge.set_ref(CLAIM_REF, &held);
    let (logs, pass) = capture_logs_pass(|| host.pass());
    let lines = waits(&logs);
    assert_eq!(lines.len(), 1, "{logs:?}");
    assert!(lines[0].1.contains("claim held by host-x since "), "{logs:?}");
    assert_eq!(reason(only(&pass)), "claim held by host-x since 2026-10-08T12:00:00Z");
}

#[test]
fn a_loop_bound_refusal_keeps_its_once_only_alert_and_warns_each_pass() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    host.memory
        .borrow_mut()
        .resynced
        .insert(host.root.clone(), (RUNNING.to_string(), "a".repeat(40)));
    for pass_n in 0..2 {
        let (logs, pass) = capture_logs_pass(|| host.pass());
        let lines = waits(&logs);
        assert_eq!(lines.len(), 1, "pass {pass_n}: {logs:?}");
        assert!(lines[0].1.contains("resync-loop"), "pass {pass_n}: {logs:?}");
        let errors = logs
            .iter()
            .filter(|(l, m)| *l == log::Level::Error && m.contains("resync-loop"))
            .count();
        assert_eq!(errors, usize::from(pass_n == 0), "the alert stays once-only");
        assert!(reason(only(&pass)).starts_with("resync-loop: "), "{pass:?}");
    }
    assert!(fx.forge.calls.borrow().is_empty(), "the bound is read before the claim");
}

/// Run one pass and capture what it logged.
fn capture_logs_pass(
    f: impl FnOnce() -> WorkspacePass,
) -> (Vec<(log::Level, String)>, WorkspacePass) {
    let mut pass = None;
    let logs = capture_logs(|| pass = Some(f()));
    (logs, pass.expect("a pass ran"))
}
