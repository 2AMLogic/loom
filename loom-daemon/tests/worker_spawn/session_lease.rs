//! #15455: a self-claim made from a guarded native launch's tool shell
//! outlives that tool call.
//!
//! The launcher exports its own pid as both session markers, so the pid the
//! worktree helper's chain selects
//! (`${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}`) is the launch owner,
//! never the tool shell's short-lived parent. Everything on the path is the
//! real thing — `spawn-worker`, `lease ensure`, and the publish, renew and
//! fence scripts. Standing in: the harness (the shared fake CLI, running each
//! tool as a short-lived process), the forge (a stateful `gh` whose clock the
//! scenario advances past the lease TTL) and the daemon verbs the scripts
//! call back into (`lib/trust-stub.sh`).

use super::{builder_role, one_live_launch_owner, tool_calls, worker};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// The issue the scenarios claim. Only the stub forge ever hears of it.
const ISSUE: &str = "4210";
/// One renewal cycle per second, the lease suites' own short interval.
const INTERVAL: Duration = Duration::from_secs(1);
/// The lease TTL every reader ships with (`LOOM_LEASE_TTL_MINUTES`, 15).
const TTL_SECS: i64 = 15 * 60;
/// How far the fixture's clock moves past the original publication.
const CLOCK_ADVANCE_SECS: i64 = 16 * 60;
/// Variables of the session running this test that would steer a scenario.
const AMBIENT: &[&str] = &[
    "LOOM_SWEEP_LEASE_RENEW_DISPATCHED",
    "LOOM_SWEEP_ID",
    "LOOM_SWEEP_RUN_ID",
    "LOOM_SWEEP_CLAIM_OWNED",
    "LOOM_TERMINAL_ID",
    "LOOM_AGENT_SESSION_PID",
    "LOOM_LEASE_HOST_ID",
    "LOOM_LEASE_PUBLISH_HOSTNAME",
    "LOOM_LEASE_PUBLISH_NOW",
    "LOOM_LEASE_FENCE_NOW",
    "LOOM_LEASE_TTL_MINUTES",
    "LOOM_SWEEP_LEASE_RENEW_SOURCE",
    "SWEEP_LEASE_RENEW_MAX_AGE_SECS",
    "LEASE_RENEWER_DAEMON",
    "WRITE_SCOPE_DAEMON",
    "LOOM_TEST_NO_TRUST_VERB",
    "LOOM_REPO",
    "LOOM_MODE",
    "LOOM_DAEMON_BIN",
    "LOOM_DAEMON_SELF_BIN",
    "LOOM_TOOLS_BIN",
    "LOOM_GH_GATEWAY_DIR",
    "LOOM_GH_READER_ROOT",
    "CLAUDE_PID",
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "GH_REPO",
    "GH_CONFIG_DIR",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "LOOM_PERSONAL_GH_TOKEN",
];

/// A stateful stand-in for the forge. Comments live in `comments.json`: a
/// POST appends one, a PATCH refreshes its `updated_at`, a GET applies the
/// caller's own jq program and `since` window. The clock is the wall clock
/// plus `clock-advance`, so a scenario moves past a lease's TTL without
/// waiting it out. Every POST and PATCH is recorded in `writes.log`, every
/// call in `calls.log`.
const STUB_FORGE: &str = r##"#!/usr/bin/env bash
D="${LOOM_TEST_FORGE:?}"
[[ "$1" == "api" ]] || { echo "stub gh: unhandled: $*" >&2; exit 3; }
shift
method=GET path="" filter="" body=""
while [[ $# -gt 0 ]]; do
    case "$1" in
        --method|-X) method="$2"; shift 2 ;;
        --paginate|--include|-i) shift ;;
        --jq) filter="$2"; shift 2 ;;
        -f|-F) body="${2#body=}"; shift 2 ;;
        -H|--header|--hostname) shift 2 ;;
        *) [[ -n "$path" ]] || path="$1"; shift ;;
    esac
done
[[ "$body" == @* ]] && body="$(cat "${body#@}")"
query=""
if [[ "$path" == *\?* ]]; then query="${path#*\?}"; path="${path%%\?*}"; fi
echo "$method $path${query:+?$query}" >> "$D/calls.log"
[[ -f "$D/comments.json" ]] || echo '[]' > "$D/comments.json"
now() {
    local epoch=$(( $(date -u +%s) + $(cat "$D/clock-advance" 2> /dev/null || echo 0) ))
    date -u -d "@$epoch" +%Y-%m-%dT%H:%M:%SZ 2> /dev/null || date -u -r "$epoch" +%Y-%m-%dT%H:%M:%SZ
}
case "$method $path" in
    "GET repos/"*"/issues/$LOOM_TEST_ISSUE/comments")
        listing="$(cat "$D/comments.json")"
        if [[ "$query" == *since=* ]]; then
            since="${query#*since=}"; since="${since%%&*}"
            listing="$(jq --arg since "$since" '[.[] | select(.updated_at >= $since)]' <<< "$listing")"
        fi
        if [[ -n "$filter" ]]; then jq -c "$filter" <<< "$listing"; else printf '%s\n' "$listing"; fi
        exit 0 ;;
    "GET repos/"*"/issues/$LOOM_TEST_ISSUE")
        if [[ "$filter" == ".state" ]]; then echo open; else echo '{"state":"open"}'; fi
        exit 0 ;;
    "POST repos/"*"/issues/$LOOM_TEST_ISSUE/comments")
        [[ -n "$body" ]] || { echo "stub gh: POST without a body" >&2; exit 1; }
        id=$(( $(jq 'length' "$D/comments.json") + 1000 ))
        stamp="$(now)"
        jq --arg body "$body" --argjson id "$id" --arg at "$stamp" \
            '. + [{id: $id, body: $body, created_at: $at, updated_at: $at, user: {login: "loom-fleet-dispatch[bot]", type: "Bot"}, author_association: "CONTRIBUTOR"}]' \
            "$D/comments.json" > "$D/comments.tmp" && mv "$D/comments.tmp" "$D/comments.json"
        echo "POST $id $stamp" >> "$D/writes.log"
        echo "{\"id\": $id}"
        exit 0 ;;
    "PATCH repos/"*"/issues/comments/"*)
        id="${path##*/}"
        stamp="$(now)"
        jq --argjson id "$id" --arg at "$stamp" \
            'map(if .id == $id then .updated_at = $at else . end)' \
            "$D/comments.json" > "$D/comments.tmp" && mv "$D/comments.tmp" "$D/comments.json"
        echo "PATCH $id $stamp advanced=$([[ -f "$D/clock-advance" ]] && echo 1 || echo 0)" >> "$D/writes.log"
        echo '{}'
        exit 0 ;;
esac
echo "stub gh: unhandled: $method $path" >&2
exit 3
"##;

/// Tool 1: the worktree helper's own claim call (`defaults/scripts/
/// worktree.sh`, `_wt_lease_claim`), made from a tool shell whose `$PPID` is
/// the short-lived tool process that ran it.
const CLAIM: &str = r#"#!/usr/bin/env bash
cd "$LOOM_TEST_LEASE_ROOT" || exit 1
"$LOOM_TEST_DAEMON" lease ensure "$LOOM_TEST_ISSUE" --watch-pid "${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}}" > /dev/null 3>&- 2> "$LOOM_TEST_FORGE/ensure.err"
echo "tool_chain chain_pid=${LOOM_AGENT_SESSION_PID:-${CLAUDE_PID:-$PPID}} ppid=$PPID"
"#;

/// Tool 2: the publishing tool has exited, so the original lease must renew
/// on its own. Then the fixture's clock moves past the publication's TTL.
const RENEW: &str = r#"#!/usr/bin/env bash
D="$LOOM_TEST_FORGE"
# A zombie (state Z) is an exited renewer awaiting its parent's reap: `kill -0`
# succeeds for it, so ask `ps` for the state, as the production check does.
live() { st="$(ps -o state= -p "$1" 2> /dev/null | tr -d '[:space:]')"; [ -n "$st" ] && [ "${st:0:1}" != "Z" ]; }
lease_id() { sed -n '1s/^POST \([0-9]*\) .*/\1/p' "$D/writes.log" 2> /dev/null; }
loop_pid="$(sed -n 's/.*renewal loop \([0-9]*\) is running.*/\1/p' "$D/ensure.err")"
n=0
until grep -q "^PATCH $(lease_id) " "$D/writes.log" 2> /dev/null; do
    n=$((n + 1)); [ "$n" -ge 100 ] && break
    sleep 0.2
done
if grep -q "^PATCH $(lease_id) " "$D/writes.log" 2> /dev/null; then
    echo "tool_renewed_after_publish=true"
elif live "$loop_pid"; then
    echo "tool_renewed_after_publish=loop-alive-without-a-patch"
else
    echo "tool_renewed_after_publish=renewer-dead"
fi
echo "$LOOM_TEST_CLOCK_ADVANCE" > "$D/clock-advance"
"#;

/// Tool 3: the builder's own pre-push fence, once the lease has been renewed
/// under the advanced clock. The builder recipe passes `--sweep-id` and lets
/// the host resolve as the publisher did.
const FENCE: &str = r#"#!/usr/bin/env bash
D="$LOOM_TEST_FORGE"
# A zombie (state Z) is an exited renewer awaiting its parent's reap: `kill -0`
# succeeds for it, so ask `ps` for the state, as the production check does.
live() { st="$(ps -o state= -p "$1" 2> /dev/null | tr -d '[:space:]')"; [ -n "$st" ] && [ "${st:0:1}" != "Z" ]; }
lease_id="$(sed -n '1s/^POST \([0-9]*\) .*/\1/p' "$D/writes.log" 2> /dev/null)"
sweep="$(sed -n 's/.*lease published\/held (host=[^ ]* sweep=\([^)]*\)).*/\1/p' "$D/ensure.err")"
loop_pid="$(sed -n 's/.*renewal loop \([0-9]*\) is running.*/\1/p' "$D/ensure.err")"
n=0
until grep -q "^PATCH $lease_id .* advanced=1" "$D/writes.log" 2> /dev/null; do
    n=$((n + 1)); [ "$n" -ge 100 ] && break
    sleep 0.2
done
if live "$loop_pid"; then echo "tool_renewer_alive_at_fence=true"; else echo "tool_renewer_alive_at_fence=false"; fi
now=$(( $(date -u +%s) + LOOM_TEST_CLOCK_ADVANCE ))
echo "$now" > "$D/fence.now"
cd "$LOOM_TEST_LEASE_ROOT" || exit 1
LOOM_LEASE_FENCE_NOW="$now" ./.loom/scripts/sweep-lease-fence.sh check "$LOOM_TEST_ISSUE" --sweep-id "$sweep" > /dev/null 2> "$D/fence.err"
echo "tool_fence_rc=$?"
"#;

/// Where a scenario keeps its stand-ins and the records they write.
struct Stage {
    /// First on `PATH`: the stub forge and the trust stub for daemon verbs.
    bin: PathBuf,
    /// The stub forge's records.
    forge: PathBuf,
    /// A checkout carrying the real lease scripts, where the tools run them.
    root: PathBuf,
    /// The tools the harness runs, `<call>.sh`.
    tools: PathBuf,
}

impl Stage {
    fn new(space: &Path) -> Self {
        let stage = Self {
            bin: space.join("bin"),
            forge: space.join("forge"),
            root: space.join("lease"),
            tools: space.join("tools"),
        };
        for dir in [&stage.bin, &stage.forge, &stage.tools] {
            std::fs::create_dir_all(dir).unwrap();
        }
        write_executable(&stage.bin.join("gh"), STUB_FORGE);
        install_trust_stub(&stage.bin);
        install_lease_scripts(&stage.root);
        for args in [
            &["init", "-q"][..],
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/acme/widget.git",
            ][..],
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&stage.root)
                    .output()
                    .is_ok_and(|out| out.status.success()),
                "git {}",
                args.join(" ")
            );
        }
        stage
    }

    fn read(&self, name: &str) -> String {
        std::fs::read_to_string(self.forge.join(name)).unwrap_or_default()
    }

    fn tool(&self, call: u32, body: &str) {
        std::fs::write(self.tools.join(format!("{call}.sh")), body).unwrap();
    }
}

/// The environment of every process in a scenario: the stand-ins first on
/// `PATH` and answering every daemon verb the lease scripts call, a short
/// renewal interval, a fixed host id, and none of the variables of the
/// session running this test.
fn scenario(command: &mut Command, stage: &Stage) {
    let path = std::env::join_paths(
        std::iter::once(stage.bin.clone())
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())),
    )
    .unwrap();
    for key in AMBIENT {
        command.env_remove(key);
    }
    command
        .env("PATH", path)
        .env("LOOM_DAEMON_BIN", stage.bin.join("loom-daemon"))
        .env("LOOM_GH_SHIM", "0")
        .env("LOOM_TEST_ISSUE", ISSUE)
        .env("LOOM_TEST_FORGE", &stage.forge)
        .env("LOOM_TEST_STUB_DIR", &stage.forge)
        .env("LOOM_TEST_LEASE_ROOT", &stage.root)
        .env("LOOM_TEST_DAEMON", env!("CARGO_BIN_EXE_loom-daemon"))
        .env("LOOM_TEST_CLOCK_ADVANCE", CLOCK_ADVANCE_SECS.to_string())
        .env("LOOM_LEASE_RENEW_STATE_DIR", stage.forge.join("renew-state"))
        .env("SWEEP_LEASE_RENEW_INTERVAL_SECS", INTERVAL.as_secs().to_string())
        .env("LOOM_HOST_ID", "native-session-lease")
        .env("LOOM_FORGE_CALL_STATS_DIR", stage.forge.join("call-stats"));
}

/// A guarded native launch whose harness runs `calls` successive tools, in
/// the environment a daemon hands a worker: a terminal id, and the dispatch
/// marker only when the daemon already published and renews this lease.
fn launch(space: &Path, stage: &Stage, calls: u32, dispatched: bool) -> Output {
    let mut command = worker(space, "opencode");
    scenario(&mut command, stage);
    command
        .env("LOOM_ROLE", "builder")
        .env("LOOM_TERMINAL_ID", "daemon-native-session-lease")
        .env("FIXTURE_TOOL_CALLS", calls.to_string())
        .env("FIXTURE_TOOL_DIR", &stage.tools)
        .env("FIXTURE_TOOL_GAP", "1");
    if dispatched {
        command.env("LOOM_SWEEP_LEASE_RENEW_DISPATCHED", ISSUE);
    }
    command.args(["-p", "hello"]).output().unwrap()
}

/// The fence a builder runs before its push, at the fixture's `now`.
fn fence(stage: &Stage, now: i64) -> Output {
    let mut command = Command::new(stage.root.join(".loom/scripts/sweep-lease-fence.sh"));
    command.args(["check", ISSUE]).current_dir(&stage.root);
    scenario(&mut command, stage);
    command
        .env("LOOM_LEASE_FENCE_NOW", now.to_string())
        .output()
        .unwrap()
}

/// One POST or PATCH the stub forge took.
#[derive(Debug)]
struct Write {
    method: &'static str,
    id: u32,
    /// A PATCH stamped after the fixture's clock was advanced.
    advanced: bool,
}

fn writes(stage: &Stage) -> Vec<Write> {
    stage
        .read("writes.log")
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(Write {
                method: match fields.next()? {
                    "POST" => "POST",
                    "PATCH" => "PATCH",
                    _ => return None,
                },
                id: fields.next()?.parse().ok()?,
                advanced: fields.nth(1) == Some("advanced=1"),
            })
        })
        .collect()
}

/// The lease identity and the renewer `lease ensure` reported.
struct Renewing {
    host: String,
    sweep: String,
    loop_pid: u32,
}

fn renewing(ensured: &str) -> Option<Renewing> {
    let (host, rest) = ensured
        .split("lease published/held (host=")
        .nth(1)?
        .split_once(" sweep=")?;
    let (sweep, rest) = rest.split_once(')')?;
    let loop_pid = rest
        .split("renewal loop ")
        .nth(1)?
        .split(' ')
        .next()?
        .parse()
        .ok()?;
    Some(Renewing {
        host: host.to_string(),
        sweep: sweep.to_string(),
        loop_pid,
    })
}

/// The pid of the tool process that ran tool `call`.
fn runner_of(stdout: &str, call: u32) -> String {
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(&format!("tool_end={call} ")))
        .and_then(|rest| rest.split_once(" runner="))
        .and_then(|(_, rest)| rest.split(' ').next())
        .unwrap()
        .to_string()
}

/// Live means "exists and is not a zombie": `kill -0` succeeds for an exited
/// child until its parent reaps it, which a detached renewer may wait on.
fn alive(pid: u32) -> bool {
    Command::new("ps")
        .args(["-o", "state=", "-p", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .output()
        .is_ok_and(|out| {
            let state = String::from_utf8_lossy(&out.stdout);
            state.trim().chars().next().is_some_and(|c| c != 'Z')
        })
}

fn wait_for(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + 30 * INTERVAL;
    while !predicate() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// `lib/trust-stub.sh`'s `loom-daemon` stand-in for the verbs the lease
/// scripts call back into.
fn install_trust_stub(bin: &Path) {
    let lib = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts/tests/lib");
    let out = Command::new("bash")
        .args(["-c", "source \"$1\" && loom_trust_stub \"$2\"", "bash"])
        .arg(lib.join("trust-stub.sh"))
        .arg(bin)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(bin.join("loom-daemon").is_file());
}

/// The real publish, renew and fence scripts, with their libraries, into a
/// checkout's `.loom/scripts` — where `lease ensure` looks for them.
fn install_lease_scripts(root: &Path) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../defaults/scripts");
    let installed = root.join(".loom/scripts");
    copy_dir(&source.join("lib"), &installed.join("lib"));
    for name in [
        "sweep-lease-publish.sh",
        "sweep-lease-renew.sh",
        "sweep-lease-fence.sh",
    ] {
        std::fs::copy(source.join(name), installed.join(name)).unwrap();
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let path = entry.unwrap().path();
        let target = to.join(path.file_name().unwrap());
        if path.is_dir() {
            copy_dir(&path, &target);
        } else {
            std::fs::copy(&path, &target).unwrap();
        }
    }
}

fn epoch_of(stamp: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(stamp)
        .unwrap()
        .timestamp()
}

/// #15455: a self-claim's lease is renewed after the publishing tool exits,
/// passes its fence past the original publication's TTL, and stops renewing
/// when the launch ends. A renewer that dies while its launch is still alive
/// — what a short-lived watch pid causes — is reported as its own failure.
#[test]
fn a_self_claim_from_a_tool_shell_outlives_that_tool() {
    let space = tempfile::tempdir().unwrap();
    builder_role(space.path());
    let stage = Stage::new(space.path());
    for (call, body) in [(1, CLAIM), (2, RENEW), (3, FENCE)] {
        stage.tool(call, body);
    }
    let out = launch(space.path(), &stage, 3, false);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let launch_pid = stdout
        .lines()
        .find_map(|line| line.strip_prefix("pid="))
        .unwrap();

    let ensured = stage.read("ensure.err");
    let lease = renewing(&ensured)
        .unwrap_or_else(|| panic!("the self-claim published no renewing lease: {ensured:?}"));
    let published = writes(&stage);
    assert_eq!(
        published
            .iter()
            .filter(|write| write.method == "POST")
            .count(),
        1,
        "one lease comment: {published:?}"
    );
    let comment_id = published[0].id;

    // Renewed once the publishing tool had exited.
    let renewal = stdout
        .lines()
        .find_map(|line| line.strip_prefix("tool_renewed_after_publish="))
        .unwrap();
    if renewal == "renewer-dead" {
        panic!(
            "the renewer ({}) died while its launch (pid {launch_pid}) was still alive: the \
             publishing tool's exit ended renewal, so lease {comment_id} ages out ({stdout})",
            lease.loop_pid
        );
    }
    assert_eq!(
        renewal, "true",
        "the renewer ({}) never renewed lease {comment_id} ({stdout})",
        lease.loop_pid
    );
    // Past the original publication's TTL, the fence passes on the renewal.
    assert!(
        stdout.contains("tool_renewer_alive_at_fence=true"),
        "the renewer ({}) died before the fence while its launch (pid {launch_pid}) was still \
         alive ({stdout})",
        lease.loop_pid
    );
    assert!(
        writes(&stage)
            .iter()
            .any(|write| write.method == "PATCH" && write.id == comment_id && write.advanced),
        "no renewal landed after the clock advanced: {:?}",
        writes(&stage)
    );
    assert!(stdout.contains("tool_fence_rc=0"), "{}", stage.read("fence.err"));
    let verdict = stage.read("fence.err");
    assert!(
        verdict.contains(&format!(
            "PASS: lease fence OK for issue #{ISSUE} -- freshest lease (host={} sweep={}) ",
            lease.host, lease.sweep
        )) && verdict.contains("host and sweep both match this sweep"),
        "{verdict}"
    );
    let comments: serde_json::Value = serde_json::from_str(&stage.read("comments.json")).unwrap();
    let comment = comments
        .as_array()
        .unwrap()
        .iter()
        .find(|comment| comment["id"].as_u64() == Some(u64::from(comment_id)))
        .unwrap();
    let fenced_at: i64 = stage.read("fence.now").trim().parse().unwrap();
    let published_at = epoch_of(comment["created_at"].as_str().unwrap());
    let renewed_at = epoch_of(comment["updated_at"].as_str().unwrap());
    assert!(
        fenced_at - published_at > TTL_SECS,
        "the fence did not run past the original publication's TTL"
    );
    assert!(
        fenced_at - renewed_at <= TTL_SECS,
        "the fence passed on something other than a fresh renewal"
    );

    // The launch has ended: renewal stops with it, and the lease expires.
    wait_for("the renewer to stop with its launch", || !alive(lease.loop_pid));
    let before = writes(&stage).len();
    std::thread::sleep(3 * INTERVAL);
    assert_eq!(writes(&stage).len(), before, "PATCHes continued after the launch ended");
    let expired =
        fence(&stage, chrono::Utc::now().timestamp() + CLOCK_ADVANCE_SECS + TTL_SECS + 60);
    let verdict = String::from_utf8_lossy(&expired.stderr);
    assert_eq!(expired.status.code(), Some(3), "{verdict}");
    assert!(
        verdict.contains(&format!(
            "ABORT: EXPIRED -- lease fence failed for issue #{ISSUE}. Freshest lease comment \
             (host={} sweep={}) ",
            lease.host, lease.sweep
        )),
        "{verdict}"
    );

    // Both markers named the launch owner throughout, so that is the pid the
    // helper's chain selected: never the tool shell's short-lived parent.
    assert_eq!(one_live_launch_owner(&stdout, 3), launch_pid);
    assert!(
        stdout
            .contains(&format!("tool_chain chain_pid={launch_pid} ppid={}", runner_of(&stdout, 1))),
        "{stdout}"
    );
    assert_ne!(runner_of(&stdout, 1), launch_pid);
    assert!(!tool_calls(&stdout)[1].prev_alive, "{stdout}");
}

/// #15455: for a lease the daemon already published and renews, a tool's
/// self-claim starts no second publisher and no second renewer.
#[test]
fn a_daemon_dispatched_lease_gets_no_second_publisher_or_renewer() {
    let space = tempfile::tempdir().unwrap();
    builder_role(space.path());
    let stage = Stage::new(space.path());
    stage.tool(1, CLAIM);
    let out = launch(space.path(), &stage, 1, true);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let launch_pid = one_live_launch_owner(&stdout, 1);
    assert!(stdout.contains(&format!("tool_chain chain_pid={launch_pid} ")), "{stdout}");
    let ensured = stage.read("ensure.err");
    assert!(
        ensured.contains(&format!(
            "issue #{ISSUE} is daemon-dispatched (LOOM_SWEEP_LEASE_RENEW_DISPATCHED names it) — \
             publish + renewal are already running for it; nothing to do"
        )),
        "{ensured}"
    );
    assert!(stage.read("calls.log").is_empty(), "the forge was called");
    assert!(writes(&stage).is_empty(), "a lease was written");
    assert!(!stage
        .read("renewer-args.log")
        .contains("lease renewer claim"));
}
