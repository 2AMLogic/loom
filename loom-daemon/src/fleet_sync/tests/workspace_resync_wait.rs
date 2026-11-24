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
        if pass_n == 0 {
            assert!(pass.probes > 0, "the outage was detected during this pass");
        } else {
            assert_eq!(network(&pass), (0, 0, 0), "the existing outage hold prevented calls");
        }
        let offline = lines
            .iter()
            .filter(|(_, m)| m.contains("remote checks are suspended after unanswered probes"))
            .count();
        assert_eq!(offline, 1, "pass {pass_n}: {logs:?}");
        // The report carries the current-pass reason too.
        let held = &pass.workspaces[3];
        assert_eq!(held.state, WState::W3, "{held:?}");
        assert_eq!(
            reason(held),
            "not attempted this pass: remote checks are suspended after unanswered probes"
        );
        assert_eq!(fx.origin_commits(), commits, "pass {pass_n}: nothing resynced");
        host.advance(INTERVAL / 2);
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
fn a_host_gate_refusal_warns_per_stale_workspace_on_each_write_pass() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    let roots = [host.root.clone(), fx.clone_as("other")];
    let commits = fx.origin_commits();
    for gate in [NotCurrent::Draining, NotCurrent::ReleaseUnverified] {
        for _ in 0..2 {
            let (logs, pass) = capture_logs_pass(|| {
                host.pass_over(&roots, Mode::Write, &|| Err(gate), Some("0.19.850"))
            });
            assert_eq!(network(&pass), (0, 0, 0));
            assert_eq!(pass.workspaces.len(), roots.len());
            let lines = waits(&logs);
            assert_eq!(lines.len(), roots.len(), "{logs:?}");
            for report in &pass.workspaces {
                assert_eq!(report.state, WState::W3);
                assert_eq!(reason(report), gate.host_note());
            }
            assert!(
                lines.iter().all(|(level, message)| {
                    *level == log::Level::Warn && message.contains(&gate.host_note())
                }),
                "{logs:?}"
            );
        }
    }
    assert!(fx.forge.calls.borrow().is_empty());
    assert_eq!(fx.origin_commits(), commits);
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
        let lines: Vec<_> = logs
            .iter()
            .filter(|(level, message)| {
                *level == log::Level::Warn
                    && message.contains("the credential has READ on acme/app, not WRITE")
            })
            .collect();
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

#[test]
fn an_open_breaker_is_warned_as_the_breaker_not_as_unanswered_probes() {
    let fx = Fixture::new(STALE);
    let host = Host::new(&fx, "host-a");
    host.heads.breaker.set(true);
    for pass_n in 0..3 {
        let (logs, pass) = capture_logs_pass(|| host.pass());
        let lines = waits(&logs);
        assert_eq!(lines.len(), 1, "pass {pass_n}: {logs:?}");
        assert!(
            lines[0].1.contains("the forge rate-limit breaker is open"),
            "pass {pass_n}: {logs:?}"
        );
        assert!(!lines[0].1.contains("unanswered"), "pass {pass_n}: {logs:?}");
        assert_eq!(network(&pass), (0, 0, 0));
        assert_eq!(only(&pass).state, WState::W1, "{pass:?}");
        host.advance(INTERVAL);
    }
    assert_eq!(host.heads.requests.get(), 0);
}

// ----------------------------------------------------------------------------
// A temporary W3 refusal ends in the resync PR once it clears (#14848)
// ----------------------------------------------------------------------------

/// Does the origin hold branch `name`, and at what commit?
fn origin_branch(fx: &Fixture, name: &str) -> Option<String> {
    let out = crate::test_git_helpers::git_out(
        &fx.origin,
        &["rev-parse", "--verify", &format!("refs/heads/{name}")],
    );
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// One temporary refusal of a W3 resync on a protected default branch
/// (#14848), and the recovery the daemon owes the moment it clears: the
/// same host, workspace, floor and pass memory carry the workspace from a
/// declined attempt to its automatic pull request. `refused_pass` is the
/// gate the declined pass runs under, `refusal` the reason it must report,
/// `refuse` installs the refusal and `clear` lifts only that refusal. Each
/// half is asserted where it happens: nothing written while refused, the
/// versioned branch and exactly one PR on the first eligible pass, the same
/// branch head and PR on a recheck past the backoff, and W0 once the PR is
/// merged.
fn w3_refusal_recovery(
    refused_pass: &dyn Fn() -> std::result::Result<(), NotCurrent>,
    refusal: &str,
    refuse: impl FnOnce(&Fixture, &Host<'_>),
    clear: impl FnOnce(&Fixture, &Host<'_>),
) {
    // An old install below the floor, a real payload difference and no
    // recorded `requires_daemon`: install-incompatible whatever the host's
    // standing, on a default branch a rule keeps off direct pushes.
    let fx = Fixture::new(Seed {
        requires: None,
        ..STALE
    });
    refuse_main_only(&fx);
    let host = Host::new(&fx, "host-a");
    let floor = Some("0.19.850");
    let branch = format!("chore/loom-resync-v{RUNNING}");
    let before = fx.origin_head();

    // The declined pass: W3 with this refusal, and nothing written.
    refuse(&fx, &host);
    let (logs, pass) = capture_logs_pass(|| host.pass_with(Mode::Write, refused_pass, floor));
    let report = only(&pass);
    assert_eq!(report.state, WState::W3, "{report:?}");
    assert_eq!(report.installed.as_deref(), Some("0.19.800"), "{report:?}");
    assert_eq!(reason(report), refusal, "{report:?}");
    let lines = waits(&logs);
    assert_eq!(lines.len(), 1, "{logs:?}");
    assert!(lines[0].1.contains(refusal), "{logs:?}");
    assert_eq!(fx.origin_head(), before, "the protected default head is intact");
    assert_eq!(origin_branch(&fx, &branch), None, "no resync branch");
    assert!(fx.forge.pulls.borrow().is_empty(), "no resync PR");
    assert!(pass.alerts.is_empty(), "a temporary refusal is not an alert");
    assert!(host.memory.borrow().backoff.is_empty(), "nor a failure");
    assert!(host.resync_worktrees().is_empty());

    // Only the refusal clears: the first eligible pass resumes the attempt,
    // which the rule answers with the branch and its one PR.
    clear(&fx, &host);
    let pass = host.pass_with(Mode::Write, &|| Ok(()), floor);
    let report = only(&pass);
    assert_eq!(report.state, WState::W3, "{report:?}");
    assert!(reason(report).contains("resync pull request open: #1 "), "{report:?}");
    assert_eq!(
        fx.origin_head(),
        before,
        "the protected default head still has its original SHA"
    );
    let pushed = origin_branch(&fx, &branch).expect("the versioned resync branch");
    assert_eq!(fx.forge.pulls.borrow().len(), 1, "exactly one PR");
    assert_eq!(fx.forge.pulls.borrow()[0].0, branch, "for that branch");
    assert_eq!(pass.alerts.len(), 1, "branch-protection stays raised while the PR is open");
    assert_eq!(pass.alerts[0].kind, "branch-protection");
    assert_eq!(fx.forge.ref_sha(CLAIM_REF), None, "claim released");
    assert!(host.resync_worktrees().is_empty());

    // Past the backoff the open PR is still the answer: neither the branch
    // head nor the PR changes.
    host.now.set(t0() + ChronoDuration::hours(7));
    let again = host.pass_with(Mode::Write, &|| Ok(()), floor);
    assert!(reason(only(&again)).contains("resync pull request open: #1 "), "{again:?}");
    assert_eq!(fx.forge.pulls.borrow().len(), 1, "still one PR");
    assert_eq!(origin_branch(&fx, &branch).as_deref(), Some(pushed.as_str()));
    assert!(again.alerts.is_empty(), "alerted once per repo");

    // The merge queue lands the PR, by the simulation of
    // `a_merged_resync_pull_request_makes_the_repo_current_and_clears_the_alert`.
    let branch_ref = format!("refs/heads/{branch}");
    let sha = git(&fx.origin, &["rev-parse", &branch_ref]);
    git(&fx.origin, &["update-ref", "-d", &branch_ref]);
    fx.forge.pulls.borrow_mut().clear();
    fs::remove_file(fx.origin.join("hooks/pre-receive")).unwrap();
    git(&fx.seed, &["fetch", "origin", "--", &sha]);
    git(&fx.seed, &["merge", "--ff-only", &sha]);
    git(&fx.seed, &["push", "origin", "HEAD:main"]);
    host.now.set(t0() + ChronoDuration::hours(14));
    let merged = host.pass_with(Mode::Write, &|| Ok(()), floor);
    assert_eq!(only(&merged).state, WState::W0, "{merged:?}");
    assert!(host.memory.borrow().backoff.is_empty(), "the alert's backoff is gone");
}

#[test]
fn a_w3_refused_while_the_release_tag_was_unverified_recovers_to_its_pr() {
    // The refusal is the host gate itself, so its clearing is the gate the
    // recovery pass runs under (above): the release verification completed.
    let unverified = || Err(NotCurrent::ReleaseUnverified);
    w3_refusal_recovery(
        &unverified,
        "host not H0: this daemon's release tag is not verified yet",
        |_, _| (),
        |_, _| (),
    );
}

#[test]
fn a_w3_refused_by_a_read_only_credential_recovers_to_its_pr() {
    w3_refusal_recovery(
        &|| Ok(()),
        "not written from this host: the credential has READ on acme/app, not WRITE",
        |_, host| write(&host.root.join(".read-only-credential"), ""),
        |_, host| fs::remove_file(host.root.join(".read-only-credential")).unwrap(),
    );
}

#[test]
fn a_w3_refused_by_a_peer_claim_recovers_to_its_pr_once_the_peer_releases_it() {
    let theirs = RefCell::new(None);
    w3_refusal_recovery(
        &|| Ok(()),
        "claim held by host-b since 2026-10-08T12:00:00Z",
        |fx, _| {
            let sha = fx
                .forge
                .add_commit(&claim_message("host-b", RUNNING, t0()), &[]);
            fx.forge.set_ref(CLAIM_REF, &sha);
            *theirs.borrow_mut() = Some(sha);
        },
        |fx, _| {
            assert_eq!(
                fx.forge.ref_sha(CLAIM_REF),
                *theirs.borrow(),
                "the peer retains its claim while it is active"
            );
            fx.forge.refs.borrow_mut().remove(CLAIM_REF);
        },
    );
}
