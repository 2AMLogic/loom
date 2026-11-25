//! Tests for the live Codex probe (#9233): wire parsing, window
//! classification by duration, not-started windows, and ADR-0017's ownership
//! rule (a session-managed profile never reaches the host transport).

use std::cell::RefCell;
use std::path::Path;
use std::time::Instant;

use chrono::{TimeZone, Utc};

use super::*;
use crate::tokens_pool::session_lifecycle::SESSION_MARKER_FILE;

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 27, 18, 25, 0).unwrap()
}

/// A real codex-cli 0.156 exchange (identity fields removed), as captured in
/// a session container: an init reply, notifications, then ids 2 and 3.
fn reply(rate_limits: &str) -> String {
    format!(
        "{{\"id\":1,\"result\":{{\"platformFamily\":\"unix\"}}}}\n\
         {{\"method\":\"configWarning\",\"params\":{{\"summary\":\"x\"}}}}\n\
         not json at all\n\
         {{\"id\":2,\"result\":{{\"account\":{{\"type\":\"chatgpt\",\"planType\":\"pro\"}},\"requiresOpenaiAuth\":true}}}}\n\
         {{\"id\":3,\"result\":{{\"rateLimits\":{rate_limits}}}}}\n"
    )
}

#[test]
fn a_weekly_primary_is_filed_as_the_long_window() {
    // resets Sep 29 20:19Z, well after `now`: a started window.
    let out = reply(
        r#"{"primary":{"usedPercent":43,"windowDurationMins":10080,"resetsAt":1790713159},"secondary":null}"#,
    );
    let ProbeOutcome::Measured(snap) = parse_probe_output(&out, now()) else {
        panic!("expected a measurement")
    };
    assert!(snap.primary.is_none(), "a 10080-minute window is never the 5h slot");
    let weekly = snap.secondary.expect("weekly window");
    assert!((weekly.used_fraction - 0.43).abs() < 1e-9);
    assert_eq!(weekly.resets_at, Utc.timestamp_opt(1_790_713_159, 0).single());
    assert_eq!(snap.observed_at, now());
}

#[test]
fn a_short_and_a_long_window_land_in_their_own_slots_whatever_order_they_arrive() {
    let out = reply(
        r#"{"primary":{"usedPercent":90,"windowDurationMins":10080,"resetsAt":1790713159},
            "secondary":{"usedPercent":20,"windowDurationMins":300,"resetsAt":1790550000}}"#
            .replace('\n', "")
            .as_str(),
    );
    let ProbeOutcome::Measured(snap) = parse_probe_output(&out, now()) else {
        panic!("expected a measurement")
    };
    assert!(
        (snap.primary.unwrap().used_fraction - 0.20).abs() < 1e-9,
        "300 min is the short slot"
    );
    assert!(
        (snap.secondary.unwrap().used_fraction - 0.90).abs() < 1e-9,
        "10080 min is the long slot"
    );
}

#[test]
fn an_unused_window_reporting_now_plus_its_length_has_no_reset() {
    // resetsAt = now + 7d exactly: the window starts at first use, so this
    // "reset" would move on every probe.
    let not_started = (now() + chrono::Duration::minutes(10080)).timestamp();
    let out = reply(&format!(
        r#"{{"primary":{{"usedPercent":0,"windowDurationMins":10080,"resetsAt":{not_started}}},"secondary":null}}"#
    ));
    let ProbeOutcome::Measured(snap) = parse_probe_output(&out, now()) else {
        panic!("expected a measurement")
    };
    let weekly = snap.secondary.expect("weekly window");
    assert_eq!(weekly.used_fraction, 0.0);
    assert!(weekly.resets_at.is_none(), "a not-started window carries no reset instant");
    // …but a used window with the same reset keeps it.
    assert!(!window_not_started(
        5.0,
        Some(10080),
        Some(Utc.timestamp_opt(not_started, 0).unwrap()),
        now()
    ));
}

#[test]
fn a_null_account_is_not_logged_in_even_when_rate_limits_errors() {
    let out = "{\"id\":2,\"result\":{\"account\":null,\"requiresOpenaiAuth\":true}}\n\
               {\"id\":3,\"error\":{\"code\":-32600,\"message\":\"not logged in\"}}\n";
    assert_eq!(parse_probe_output(out, now()), ProbeOutcome::NotLoggedIn);
}

#[test]
fn a_method_not_found_on_rate_limits_is_capability_absent() {
    let out = "{\"id\":2,\"result\":{\"account\":{\"type\":\"chatgpt\"}}}\n\
               {\"id\":3,\"error\":{\"code\":-32601,\"message\":\"method not found\"}}\n";
    assert_eq!(parse_probe_output(out, now()), ProbeOutcome::CapabilityAbsent);
}

#[test]
fn no_rate_limit_reply_is_a_failure_not_a_measurement() {
    assert_eq!(
        parse_probe_output("{\"id\":1,\"result\":{}}\n", now()),
        ProbeOutcome::Failed("no_rate_limit_reply")
    );
}

/// Records which transport was used; panics on host use for a profile the
/// test marks session-managed.
struct RecordingTransport {
    container_output: Option<ExecOutput>,
    calls: RefCell<Vec<String>>,
    forbid_host: bool,
}

impl ProbeTransport for RecordingTransport {
    fn host(&self, profile: &Path) -> Result<ExecOutput> {
        assert!(
            !self.forbid_host,
            "ADR-0017: a session-managed profile must never be probed host-directly ({})",
            profile.display()
        );
        self.calls.borrow_mut().push("host".into());
        Ok(ExecOutput {
            success: true,
            unavailable: false,
            timed_out: false,
            exit_code: Some(0),
            output: reply(
                r#"{"primary":{"usedPercent":10,"windowDurationMins":10080,"resetsAt":1790713159}}"#,
            ),
        })
    }

    fn container(&self, container: &str) -> Result<Option<ExecOutput>> {
        self.calls
            .borrow_mut()
            .push(format!("container:{container}"));
        Ok(self.container_output.clone())
    }
}

fn mark_session_managed(profile: &Path) {
    std::fs::write(profile.join(SESSION_MARKER_FILE), "{}").unwrap();
}

#[test]
fn missing_runtime_is_distinct_from_a_missing_container() {
    struct MissingRuntime;
    impl ProbeTransport for MissingRuntime {
        fn host(&self, _: &Path) -> Result<ExecOutput> {
            panic!("a session profile entered the host probe")
        }

        fn container(&self, _: &str) -> Result<Option<ExecOutput>> {
            Err(std::io::Error::from(std::io::ErrorKind::NotFound).into())
        }
    }
    let profile = tempfile::tempdir().unwrap();
    mark_session_managed(profile.path());
    let result = probe_account(&MissingRuntime, "agent-1", profile.path(), now());
    assert_eq!(result.detail(), Some("container_runtime_unavailable"));
}

#[test]
fn a_session_managed_profile_with_no_running_container_is_unavailable_and_never_probed_on_the_host()
{
    let profile = tempfile::tempdir().unwrap();
    mark_session_managed(profile.path());
    let transport = RecordingTransport {
        container_output: None,
        calls: RefCell::new(Vec::new()),
        forbid_host: true,
    };
    assert_eq!(
        probe_account(&transport, "agent-1", profile.path(), now()),
        ProbeOutcome::SessionUnavailable
    );
    assert_eq!(
        *transport.calls.borrow(),
        vec!["container:loom-codex-session-agent-1".to_string()]
    );
}

#[test]
fn a_session_managed_profile_is_measured_inside_its_container() {
    let profile = tempfile::tempdir().unwrap();
    mark_session_managed(profile.path());
    let transport = RecordingTransport {
        container_output: Some(ExecOutput {
            success: true,
            unavailable: false,
            timed_out: false,
            exit_code: Some(0),
            output: reply(
                r#"{"primary":{"usedPercent":43,"windowDurationMins":10080,"resetsAt":1790713159}}"#,
            ),
        }),
        calls: RefCell::new(Vec::new()),
        forbid_host: true,
    };
    assert!(matches!(
        probe_account(&transport, "agent-1", profile.path(), now()),
        ProbeOutcome::Measured(_)
    ));
}

#[test]
fn a_host_managed_profile_is_probed_host_directly() {
    let profile = tempfile::tempdir().unwrap();
    let transport = RecordingTransport {
        container_output: None,
        calls: RefCell::new(Vec::new()),
        forbid_host: false,
    };
    assert!(matches!(
        probe_account(&transport, "agent-3", profile.path(), now()),
        ProbeOutcome::Measured(_)
    ));
    assert_eq!(*transport.calls.borrow(), vec!["host".to_string()]);
}

#[test]
fn a_timed_out_probe_is_reported_as_such() {
    let profile = tempfile::tempdir().unwrap();
    mark_session_managed(profile.path());
    let transport = RecordingTransport {
        container_output: Some(ExecOutput {
            success: false,
            unavailable: false,
            timed_out: true,
            exit_code: None,
            output: String::new(),
        }),
        calls: RefCell::new(Vec::new()),
        forbid_host: true,
    };
    assert_eq!(
        probe_account(&transport, "agent-1", profile.path(), now()),
        ProbeOutcome::Failed("probe_timed_out")
    );
}

#[cfg(unix)]
fn fake_probe(script: &str) -> (tempfile::TempDir, Command) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fake-probe");
    std::fs::write(&path, format!("{script}\n")).unwrap();
    let mut command = Command::new("/bin/sh");
    command.arg(path);
    (dir, command)
}

#[test]
fn a_missing_probe_binary_is_unavailable_not_a_failed_probe() {
    let dir = tempfile::tempdir().unwrap();
    let output =
        run_bounded_within(Command::new(dir.path().join("no-such-probe")), PROBE_TIMEOUT).unwrap();
    assert_eq!(
        output,
        ExecOutput {
            success: false,
            unavailable: true,
            timed_out: false,
            exit_code: None,
            output: String::new(),
        }
    );
}

#[test]
#[cfg(unix)]
fn a_probe_that_never_answers_times_out() {
    let started = Instant::now();
    let (_dir, command) = fake_probe("printf 'partial reply\\n'; /bin/sleep 30");
    let output = run_bounded_within(command, Duration::from_millis(300)).unwrap();
    assert!(output.timed_out, "{output:?}");
    assert!(!output.success && !output.unavailable);
    assert_eq!(output.exit_code, None);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "a timed-out probe must return at its deadline, took {:?}",
        started.elapsed()
    );
}

#[test]
#[cfg(unix)]
fn a_timed_out_probe_does_not_wait_for_a_grandchild_holding_the_pipe() {
    let started = Instant::now();
    let (_dir, command) = fake_probe("/bin/sleep 30 & printf 'partial reply\\n'; wait");
    let output = run_bounded_within(command, Duration::from_millis(300)).unwrap();
    assert!(output.timed_out, "{output:?}");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the probe waited on a descendant that outlived its deadline, took {:?}",
        started.elapsed()
    );
}

#[test]
#[cfg(unix)]
fn probe_stdout_is_cut_at_max_output_bytes() {
    const BLOCK: usize = 4096;
    let cap = MAX_OUTPUT_BYTES as usize;
    let block = "x".repeat(BLOCK);
    for (blocks, tail) in [(cap / BLOCK, ""), (cap / BLOCK + 1, "END")] {
        let (_dir, command) = fake_probe(&format!(
            "i=0; while [ $i -lt {blocks} ]; do printf '%s' '{block}'; i=$((i+1)); done; \
             printf '{tail}'"
        ));
        let output = run_bounded_within(command, PROBE_TIMEOUT).unwrap();
        assert_eq!(output.exit_code, Some(0), "{output:?}");
        assert!(output.success && !output.timed_out && !output.unavailable);
        assert_eq!(output.output.len(), cap);
        assert!(output.output.bytes().all(|byte| byte == b'x'));
    }
}

#[test]
#[cfg(unix)]
fn probe_output_decodes_non_utf8_bytes_lossily() {
    let (_dir, command) = fake_probe(r"printf 'a\377b'");
    let output = run_bounded_within(command, PROBE_TIMEOUT).unwrap();
    assert_eq!(output.output, "a\u{FFFD}b");
}

#[test]
fn the_probe_script_pipelines_all_four_messages_and_names_itself() {
    let script = probe_script();
    for needle in [
        "\"method\":\"initialize\"",
        "\"method\":\"initialized\"",
        "\"method\":\"account/read\"",
        "\"method\":\"account/rateLimits/read\"",
        "codex -s read-only -a never app-server",
        "loom-daemon-probe",
    ] {
        assert!(script.contains(needle), "probe script is missing {needle}");
    }
}
