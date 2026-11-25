//! Fleet-host membership for the remote-fleet-exec guard (#11568).
//!
//! A host is a fleet host when it is a tailnet address or name (`*.ts.net`,
//! an IPv4 address in Tailscale's `100.64.0.0/10`, or an IPv6 address in its
//! `fd7a:115c:a1e0::/48`), or when it names a host on a roster: the Loom
//! fleet registry (`LOOM_FLEET_PATH`, else `~/.loom/fleet.json` — `ssh_host`,
//! `tailnet_name`, `provider_instance_id`), the local `tailscale status
//! --json` peer list, or the extend-only `guards.remoteFleetHosts` config
//! array (where an operator lists hosts a roster file such as a fleet-gitops
//! `hosts.yml` names). An `ssh` alias is resolved through `ssh -G` first, so
//! `loom-worker-1` → `100.x.y.z` is caught even with no roster at all.
//!
//! Every source is read lazily and only when the guard has already found a
//! non-read-only payload, so an ordinary `ssh host 'git status'` never pays
//! for a roster read or a subprocess.

use std::cell::OnceCell;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// Test seam and operator override: read `tailscale status --json` output
/// from this file instead of running the CLI. An empty value or a missing
/// file means "no tailnet peers".
pub const TAILSCALE_STATUS_FILE_ENV: &str = "LOOM_GUARD_TAILSCALE_STATUS_FILE";
/// The extend-only config array of extra fleet host names.
pub const HOSTS_KEY: &str = "guards.remoteFleetHosts";
/// Repo-local list of hosts (exact names/addresses and CIDRs) that are outside
/// the guard's scope. It removes only the range-based tailnet classification;
/// a host on a roster (registry, tailscale peers, [`HOSTS_KEY`]) stays a fleet
/// host even when listed here.
pub const UNMANAGED_KEY: &str = "guards.remoteUnmanagedHosts";

/// Which rule classified a host as a fleet host (named in the ask text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FleetRule {
    /// The Loom fleet registry (`LOOM_FLEET_PATH` / `~/.loom/fleet.json`).
    Registry,
    /// A `tailscale status --json` peer.
    TailscalePeer,
    /// The extend-only [`HOSTS_KEY`] config array.
    ConfigHosts,
    /// A tailnet address or name (`*.ts.net`, `100.64.0.0/10`, `fd7a:115c:a1e0::/48`).
    TailnetRange,
    /// A roster that does not say which list named the host.
    Roster,
}

impl FleetRule {
    /// Operator-facing name of the rule.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            Self::Registry => "the fleet registry",
            Self::TailscalePeer => "a tailscale peer",
            Self::ConfigHosts => "`guards.remoteFleetHosts`",
            Self::TailnetRange => "the tailnet address range",
            Self::Roster => "the fleet roster",
        }
    }
}

/// A host's fleet classification: the rule, and the `ssh -G` hostname the
/// rule matched when it matched the resolved name rather than the spelling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetMatch {
    pub rule: FleetRule,
    pub resolved: Option<String>,
}

/// Answers "is this a fleet host?".
pub trait Roster {
    fn is_fleet_host(&self, host: &str) -> bool;
    /// The rule that makes `host` a fleet host, or `None`. `ssh_config` is the
    /// invocation's `-F` file, used for the `ssh -G` hostname lookup. The
    /// default defers to [`Roster::is_fleet_host`].
    fn fleet_match(&self, host: &str, _ssh_config: Option<&Path>) -> Option<FleetMatch> {
        self.is_fleet_host(host).then_some(FleetMatch {
            rule: FleetRule::Roster,
            resolved: None,
        })
    }
    fn is_fleet_instance(&self, instance_id: &str) -> bool;
}

/// Tailnet address/name test that needs no roster.
#[must_use]
pub fn is_tailnet_literal(host: &str) -> bool {
    let h = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if h.ends_with(".ts.net") {
        return true;
    }
    match h.parse::<IpAddr>() {
        Ok(IpAddr::V4(v4)) => {
            let o = v4.octets();
            o[0] == 100 && (64..=127).contains(&o[1])
        }
        Ok(IpAddr::V6(v6)) => {
            let s = v6.segments();
            s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
        }
        Err(_) => false,
    }
}

fn parse_host_ip(host: &str) -> Option<IpAddr> {
    host.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .parse()
        .ok()
}

/// Does `ip` fall inside `net/prefix`? Mixed address families never match.
fn cidr_contains(net: IpAddr, prefix: u32, ip: IpAddr) -> bool {
    match (net, ip) {
        (IpAddr::V4(n), IpAddr::V4(i)) if prefix <= 32 => {
            let mask = u32::MAX.checked_shl(32 - prefix).unwrap_or(0);
            u32::from(n) & mask == u32::from(i) & mask
        }
        (IpAddr::V6(n), IpAddr::V6(i)) if prefix <= 128 => {
            let mask = u128::MAX.checked_shl(128 - prefix).unwrap_or(0);
            u128::from(n) & mask == u128::from(i) & mask
        }
        _ => false,
    }
}

/// Is `host` named by an `entries` item: an exact name/address, or a CIDR
/// containing it? A malformed CIDR entry matches nothing (never fail-open).
#[must_use]
pub fn unmanaged_matches(entries: &[String], host: &str) -> bool {
    let ip = parse_host_ip(host);
    entries.iter().any(|e| {
        let e = e.trim();
        if let Some((net, prefix)) = e.split_once('/') {
            return match (parse_host_ip(net), prefix.trim().parse::<u32>(), ip) {
                (Some(n), Ok(p), Some(i)) => cidr_contains(n, p, i),
                _ => false,
            };
        }
        names_match(e, host)
    })
}

/// Names compare case-insensitively, and a dotless name matches the first
/// label of a dotted one (`loom-worker-1` ≡ `loom-worker-1.tail1234.ts.net`).
#[must_use]
pub fn names_match(a: &str, b: &str) -> bool {
    let a = a.trim().trim_end_matches('.').to_ascii_lowercase();
    let b = b.trim().trim_end_matches('.').to_ascii_lowercase();
    if a.is_empty() || b.is_empty() {
        return false;
    }
    if a == b {
        return true;
    }
    let first = |s: &str| s.split('.').next().unwrap_or("").to_string();
    let a_ip = a.parse::<IpAddr>().is_ok();
    let b_ip = b.parse::<IpAddr>().is_ok();
    if a_ip || b_ip {
        return false;
    }
    (!a.contains('.') || !b.contains('.')) && first(&a) == first(&b)
}

/// The real roster: fleet registry, tailscale peers, config list, `ssh -G`.
pub struct ProcessRoster {
    workspace: PathBuf,
    names: OnceCell<Vec<(String, FleetRule)>>,
    instances: OnceCell<Vec<String>>,
    unmanaged: OnceCell<Vec<String>>,
}

impl ProcessRoster {
    #[must_use]
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_path_buf(),
            names: OnceCell::new(),
            instances: OnceCell::new(),
            unmanaged: OnceCell::new(),
        }
    }

    fn registry(&self) -> crate::fleet::FleetRegistry {
        crate::fleet::FleetRegistry::load_default().unwrap_or_default()
    }

    fn names(&self) -> &[(String, FleetRule)] {
        self.names.get_or_init(|| {
            let mut out: Vec<(String, FleetRule)> = Vec::new();
            for w in self.registry().workers {
                out.push((w.ssh_host.clone(), FleetRule::Registry));
                out.extend(w.tailnet_name.clone().map(|n| (n, FleetRule::Registry)));
            }
            out.extend(
                tailscale_names(&tailscale_status())
                    .into_iter()
                    .map(|n| (n, FleetRule::TailscalePeer)),
            );
            let config = crate::config_resolver::resolve_effective_config(&self.workspace);
            if let Some(Value::Array(items)) = crate::config_resolver::get_path(&config, HOSTS_KEY)
            {
                out.extend(
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(|n| (n.to_string(), FleetRule::ConfigHosts)),
                );
            }
            out.retain(|(n, _)| !n.trim().is_empty());
            out
        })
    }

    fn unmanaged(&self) -> &[String] {
        self.unmanaged.get_or_init(|| {
            let config = crate::config_resolver::resolve_effective_config(&self.workspace);
            match crate::config_resolver::get_path(&config, UNMANAGED_KEY) {
                Some(Value::Array(items)) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .filter(|n| !n.trim().is_empty())
                    .collect(),
                _ => Vec::new(),
            }
        })
    }

    fn named(&self, host: &str) -> Option<FleetRule> {
        self.names()
            .iter()
            .find(|(n, _)| names_match(n, host))
            .map(|(_, rule)| *rule)
    }
}

impl Roster for ProcessRoster {
    fn is_fleet_host(&self, host: &str) -> bool {
        self.fleet_match(host, None).is_some()
    }

    fn fleet_match(&self, host: &str, ssh_config: Option<&Path>) -> Option<FleetMatch> {
        let found = |rule, resolved: Option<&str>| {
            Some(FleetMatch {
                rule,
                resolved: resolved.map(str::to_string),
            })
        };
        // Named rosters always win: the unmanaged list can never un-protect
        // a host the fleet actually knows.
        if let Some(rule) = self.named(host) {
            return found(rule, None);
        }
        let scoped = |h: &str| unmanaged_matches(self.unmanaged(), h);
        if is_tailnet_literal(host) && self.unmanaged().is_empty() {
            return found(FleetRule::TailnetRange, None); // no `ssh -G` needed
        }
        let resolved =
            ssh_resolved_hostname(host, ssh_config).filter(|r| !r.eq_ignore_ascii_case(host));
        if let Some(r) = resolved.as_deref() {
            if let Some(rule) = self.named(r) {
                return found(rule, Some(r));
            }
        }
        if scoped(host) || resolved.as_deref().is_some_and(scoped) {
            return None;
        }
        if is_tailnet_literal(host) {
            return found(FleetRule::TailnetRange, None);
        }
        match resolved.as_deref() {
            Some(r) if is_tailnet_literal(r) => found(FleetRule::TailnetRange, Some(r)),
            _ => None,
        }
    }

    fn is_fleet_instance(&self, instance_id: &str) -> bool {
        self.instances
            .get_or_init(|| {
                self.registry()
                    .workers
                    .into_iter()
                    .filter_map(|w| w.provider_instance_id)
                    .collect()
            })
            .iter()
            .any(|i| i.eq_ignore_ascii_case(instance_id))
    }
}

/// Every name and address a `tailscale status --json` document lists for
/// `Self` and each `Peer`.
#[must_use]
pub fn tailscale_names(status: &Value) -> Vec<String> {
    let mut nodes: Vec<&Value> = Vec::new();
    if let Some(me) = status.get("Self") {
        nodes.push(me);
    }
    if let Some(Value::Object(peers)) = status.get("Peer") {
        nodes.extend(peers.values());
    }
    let mut out = Vec::new();
    for node in nodes {
        for key in ["HostName", "DNSName"] {
            if let Some(s) = node.get(key).and_then(Value::as_str) {
                out.push(s.to_string());
            }
        }
        if let Some(Value::Array(ips)) = node.get("TailscaleIPs") {
            out.extend(ips.iter().filter_map(Value::as_str).map(str::to_string));
        }
    }
    out
}

fn tailscale_status() -> Value {
    if let Some(path) = std::env::var_os(TAILSCALE_STATUS_FILE_ENV) {
        return std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or(Value::Null);
    }
    for bin in [
        "tailscale",
        "/Applications/Tailscale.app/Contents/MacOS/Tailscale",
    ] {
        if let Some(out) = run_with_timeout(bin, &["status", "--json"]) {
            if let Ok(v) = serde_json::from_str(&out) {
                return v;
            }
        }
    }
    Value::Null
}

/// `ssh [-F <config>] -G <host>`'s resolved `hostname` (no connection is
/// made). A missing or unreadable `-F` file makes `ssh` fail, which yields
/// `None`: the lookup then falls back to the host's spelling, never an extra allow.
fn ssh_resolved_hostname(host: &str, ssh_config: Option<&Path>) -> Option<String> {
    if host.starts_with('-') {
        return None;
    }
    let cfg = ssh_config.map(|p| p.to_string_lossy().into_owned());
    let mut args: Vec<&str> = Vec::new();
    if let Some(cfg) = cfg.as_deref() {
        args.extend(["-F", cfg]);
    }
    args.extend(["-G", "--", host]);
    let out = run_with_timeout("ssh", &args)?;
    out.lines()
        .find_map(|l| l.strip_prefix("hostname "))
        .map(|h| h.trim().to_string())
}

fn run_with_timeout(bin: &str, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null());
    match crate::proc_exec::run_bounded(cmd, Duration::from_secs(3)) {
        Ok(crate::proc_exec::Completion::Exited(out)) if out.status.success() => {
            // Strict UTF-8, as the old `read_to_string` reader: a document
            // with invalid bytes is `None`, not lossily decoded.
            String::from_utf8(out.stdout).ok()
        }
        _ => None,
    }
}

#[cfg(test)]
mod run_with_timeout_tests {
    //! #15007 slice C: the executor-backed `run_with_timeout` behind the
    //! roster probes. Pins its strict-UTF-8 `Option<String>` contract, the
    //! `|| true`-style `None` on failure/timeout, and group termination.

    use super::run_with_timeout;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// An executable stub binary running `body`.
    fn stub(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        let bin = dir.join(name);
        std::fs::write(&bin, format!("#!/bin/sh\n{body}\n")).expect("write stub");
        let mut perms = std::fs::metadata(&bin).expect("stat stub").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod stub");
        bin
    }

    #[test]
    #[cfg(unix)]
    fn returns_the_document_of_a_fast_command() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "probe", r"printf 'line1\nline2\n'");
        assert_eq!(
            run_with_timeout(bin.to_str().unwrap(), &["status", "--json"]).as_deref(),
            Some("line1\nline2\n")
        );
    }

    #[test]
    #[cfg(unix)]
    fn none_on_nonzero_exit_missing_binary_and_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let failing = stub(dir.path(), "failing", "exit 3");
        assert!(run_with_timeout(failing.to_str().unwrap(), &[]).is_none());
        assert!(run_with_timeout("/nonexistent/loom-canary-roster", &[]).is_none());
        // Strict UTF-8, as the old `read_to_string` reader: the JSON the
        // probes parse must never be lossily decoded.
        let binary = stub(dir.path(), "binary", r"printf 'a\377b'");
        assert!(run_with_timeout(binary.to_str().unwrap(), &[]).is_none());
    }

    #[test]
    #[cfg(unix)]
    fn collects_a_document_larger_than_the_pipe_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let bin = stub(dir.path(), "large", "head -c 262144 /dev/zero | tr '\\0' 'x'");
        let out =
            run_with_timeout(bin.to_str().unwrap(), &["status", "--json"]).expect("must complete");
        assert_eq!(out.len(), 262_144, "every byte must be collected, not a timeout");
    }

    #[test]
    #[cfg(unix)]
    fn timeout_returns_none_and_kills_the_descendants_too() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let bin = stub(
            dir.path(),
            "hung",
            &format!("/bin/sleep 120 & echo $! > {}; exec /bin/sleep 120", pidfile.display()),
        );
        assert!(run_with_timeout(bin.to_str().unwrap(), &[]).is_none());

        std::thread::sleep(std::time::Duration::from_millis(300));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("grandchild must have recorded its pid")
            .trim()
            .parse()
            .expect("pid must parse");
        // SAFETY: signal 0 performs error checking only; it never delivers a signal.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        if alive {
            // SAFETY: cleanup kill of a process this test spawned.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        assert!(!alive, "grandchild {pid} outlived the roster timeout");
    }
}
