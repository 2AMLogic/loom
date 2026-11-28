//! Out-of-band changes to a managed checkout (#11566).
//!
//! # Why
//!
//! On 2026-10-12 at 00:44:39, 00:44:57 and 00:45:16 a managed checkout was
//! `reset` three times by something that was not Loom, discarding unpushed
//! commits. Nothing in the daemon noticed; the evidence was the reflog, read by
//! hand hours later. This module makes the `checkout_ff` pass read it.
//!
//! # How
//!
//! * **Loom tags its own git.** Every daemon git child that can move a ref in a
//!   managed checkout runs with `GIT_REFLOG_ACTION=loom:<op>` ([`tag`]), so git
//!   writes `loom:ff: Fast-forward` rather than `merge origin/main: ...`.
//!   Porcelain (`merge`, `reset`, `checkout`, `pull`) honours the preset; the
//!   plumbing a caller adds later must pass `-m "loom:<op> ..."` itself.
//! * **The pass compares reflogs.** For each root it reads `logs/HEAD` and
//!   `logs/refs/heads/<default>` ([`read_logs`]) and remembers, per file, how
//!   many lines it saw and the last one ([`Mark`]). The entries after the mark
//!   are new; each history-moving one that is not `loom:`-tagged becomes a
//!   `checkout.external_change` record ([`classify`]).
//! * **Unknown is not a change.** The first sight of a root, a reflog that has
//!   shrunk or been rewritten (`gc`, `reflog expire`), or one that cannot be
//!   read seeds the mark and emits nothing.
//! * **Only moves count.** An entry whose old and new sha are equal
//!   (`reset --hard HEAD`) moved nothing. Plain `commit`, `merge` and `pull`
//!   entries only add history and are not reported; `reset`, `checkout`,
//!   `rebase (start|finish|abort)` and `branch: Reset` are.
//!
//! The discarded commits are the ones reachable from the old tip and from no
//! ref now (`git rev-list <from> --not --all`), counted at the time the pass
//! reads the entry.
//!
//! # The pass's own move
//!
//! The pass's `merge --ff-only` is the one Loom write that moves a managed
//! checkout forward, and until #15729 nothing in the store said so: its entry
//! is `loom:`-tagged, so it is never an external change, and
//! `checkout.freshness` shows only where HEAD was per pass. A merge that
//! succeeds is followed by a lookup of the `loom:ff` entry the attempt added
//! to `logs/HEAD` ([`fast_forwarded`]) and one `checkout.fast_forward` record
//! for it, stamped at the entry's time. The lookup is bounded to the lines the
//! attempt added and to the commit the merge began from, so a stale entry, an
//! external move or a merge git had nothing to do cannot claim credit. A
//! reflog that cannot be read emits nothing.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::Duration;

use chrono::{DateTime, TimeZone, Utc};
use sha2::{Digest, Sha256};

use crate::cmd_out::{run_command, CmdOutcome};
use crate::telemetry::kinds::checkout_external_change::{
    is_loom_reflog_message, loom_reflog_action, CheckoutExternalChangeRecord, DiscardedCommits,
    MAX_DISCARDED_SUBJECTS, REFLOG_ACTION_SEPARATOR,
};
use crate::telemetry::kinds::checkout_fast_forward::CheckoutFastForwardRecord;
use crate::telemetry::provenance::Provenance;
use crate::telemetry::{RepoVisibility, TelemetryRecord};

/// Bound for each local git read.
const TIMEOUT: Duration = Duration::from_secs(15);

/// How many citations per root the hold-clear path can name.
const KEEP_CITED: usize = 8;

/// The operation the pass's own merge runs under
/// ([`crate::fleet_sync::checkout_ff::git`]): the reflog entry it writes is
/// the one [`fast_forwarded`] looks for.
pub(super) const FF_OP: &str = "ff";

/// The value of `GIT_REFLOG_ACTION` for a Loom git operation `op`.
#[must_use]
pub(crate) fn tag(op: &str) -> String {
    loom_reflog_action(op)
}

/// `git <args>` in `dir`, tagged as Loom's `op`, with stdin closed.
#[must_use]
pub(crate) fn git_cmd(op: &str, dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .env("GIT_REFLOG_ACTION", tag(op));
    cmd
}

// ============================================================================
// Reading a reflog
// ============================================================================

/// One reflog line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The commit before the move.
    pub from: String,
    /// The commit after it.
    pub to: String,
    /// When git wrote the line.
    pub at: DateTime<Utc>,
    /// The message, as git wrote it.
    pub message: String,
}

/// Parse `<old> <new> <name> <<email>> <epoch> <tz>\t<message>`.
#[must_use]
pub fn parse_line(line: &str) -> Option<Entry> {
    let (head, message) = line.split_once('\t')?;
    let mut words = head.split_whitespace();
    let (from, to) = (words.next()?, words.next()?);
    let rest: Vec<&str> = words.collect();
    // The identity is free text; the epoch and zone are always the last two.
    let epoch: i64 = rest.get(rest.len().checked_sub(2)?)?.parse().ok()?;
    let is_sha = |s: &str| s.len() >= 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
    (is_sha(from) && is_sha(to)).then_some(())?;
    Some(Entry {
        from: from.to_string(),
        to: to.to_string(),
        at: Utc.timestamp_opt(epoch, 0).single()?,
        message: message.trim_end().to_string(),
    })
}

/// What the pass last saw of one reflog file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mark {
    count: usize,
    last: String,
}

impl Mark {
    fn of(lines: &[&str]) -> Self {
        Self {
            count: lines.len(),
            last: lines.last().map(|l| (*l).to_string()).unwrap_or_default(),
        }
    }
}

/// The lines of `text` after `mark`, and the mark that now stands. The new
/// lines are `None` when there is nothing to compare with: no mark yet, or a
/// log that no longer extends what was seen.
fn since<'a>(text: &'a str, mark: Option<&Mark>) -> (Option<Vec<&'a str>>, Mark) {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let now = Mark::of(&lines);
    let new = mark.and_then(|m| {
        let extends = m.count <= lines.len()
            && (m.count == 0 || lines.get(m.count - 1).is_some_and(|l| *l == m.last));
        extends.then(|| lines[m.count..].to_vec())
    });
    (new, now)
}

/// What a history-moving entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `reset: ...` or `branch: Reset to ...`.
    Reset,
    /// `checkout: moving from A to B`.
    Checkout,
    /// `rebase (start|finish|abort)`.
    Rebase,
}

impl Kind {
    /// The wire name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Reset => "reset",
            Kind::Checkout => "checkout",
            Kind::Rebase => "rebase",
        }
    }
}

/// The kind of an entry that moved history and was not Loom's; `None` for
/// Loom's own, for a no-op, and for entries that only add history.
#[must_use]
pub fn classify(entry: &Entry) -> Option<Kind> {
    let m = entry.message.as_str();
    if is_loom_reflog_message(m) || entry.from == entry.to {
        return None;
    }
    if m.starts_with("reset:") || m.starts_with("branch: Reset") {
        Some(Kind::Reset)
    } else if m.starts_with("checkout: moving from") {
        Some(Kind::Checkout)
    } else if m.starts_with("rebase")
        && ["(start)", "(finish)", "(abort)"]
            .iter()
            .any(|s| m.contains(s))
    {
        Some(Kind::Rebase)
    } else {
        None
    }
}

// ============================================================================
// What a host remembers
// ============================================================================

/// The reflog marks of one root, by reflog file (`HEAD`, `refs/heads/<b>`).
#[derive(Debug, Default)]
pub(super) struct Marks(HashMap<String, Mark>);

/// What a scan of one root found.
#[derive(Debug, Default)]
pub(super) struct Scan {
    /// Out-of-band entries, oldest first, as `(ref, entry, kind)`.
    pub found: Vec<(String, Entry, Kind)>,
}

impl Marks {
    /// Compare the logs `(ref, text)` with the marks, update the marks, and
    /// return what is new and external. A ref with no usable mark is seeded.
    pub(super) fn scan(&mut self, logs: &[(String, String)]) -> Scan {
        let mut out = Scan::default();
        let mut seen: HashSet<(String, String, i64)> = HashSet::new();
        for (name, text) in logs {
            let (new, now) = since(text, self.0.get(name));
            self.0.insert(name.clone(), now);
            for line in new.unwrap_or_default() {
                let Some(entry) = parse_line(line) else {
                    continue;
                };
                let Some(kind) = classify(&entry) else {
                    continue;
                };
                // A reset of the checked-out branch is in both logs.
                if seen.insert((entry.from.clone(), entry.to.clone(), entry.at.timestamp())) {
                    out.found.push((name.clone(), entry, kind));
                }
            }
        }
        out.found.sort_by_key(|(_, e, _)| e.at);
        out
    }
}

// ============================================================================
// git
// ============================================================================

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args(args)
        .stdin(Stdio::null())
        .env("LC_ALL", "C")
        .env("GIT_REFLOG_ACTION", tag("read"));
    for var in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
        cmd.env_remove(var);
    }
    match run_command(cmd, TIMEOUT) {
        CmdOutcome::Ran(out) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
        }
        _ => None,
    }
}

/// The text of `logs/HEAD` and `logs/refs/heads/<branch>` in `root`. A log
/// that does not exist (or cannot be read) is absent, not empty: its marks are
/// left alone.
pub(super) fn read_logs(root: &Path, branch: Option<&str>) -> Vec<(String, String)> {
    let mut names = vec!["HEAD".to_string()];
    names.extend(branch.map(|b| format!("refs/heads/{b}")));
    names
        .into_iter()
        .filter_map(|name| {
            let rel = git(root, &["rev-parse", "--git-path", &format!("logs/{name}")])?;
            let path = if Path::new(&rel).is_absolute() {
                PathBuf::from(rel)
            } else {
                root.join(rel)
            };
            Some((name, std::fs::read_to_string(path).ok()?))
        })
        .collect()
}

/// The commits reachable from `from` and from no ref now. `None`: unknown.
fn discarded(root: &Path, from: &str) -> Option<DiscardedCommits> {
    if from.bytes().any(|b| !b.is_ascii_hexdigit()) || from.bytes().all(|b| b == b'0') {
        return None;
    }
    let count = git(root, &["rev-list", "--count", from, "--not", "--all"])?;
    let limit = format!("--max-count={MAX_DISCARDED_SUBJECTS}");
    let subjects = git(root, &["log", "--format=%s", &limit, from, "--not", "--all"])?;
    Some(DiscardedCommits {
        count: count.parse().ok()?,
        subjects: subjects.lines().map(str::to_string).collect(),
    })
}

/// `owner/repo` from `origin`'s URL, when it has that shape.
///
/// Fleet-specific normalization first — the recorded URL may carry trailing
/// slashes and repeated `.git` suffixes, which the checkout pass must keep
/// accepting — then the one shared parser (issue #15372).
fn repo_slug(root: &Path) -> Option<String> {
    let url = git(root, &["remote", "get-url", "origin"])?;
    let url = url.trim_end_matches('/').trim_end_matches(".git");
    crate::forge_cmd::parse_nwo_from_remote_url(url)
}

// ============================================================================
// The record
// ============================================================================

fn change_id(root: &Path, entry: &Entry) -> String {
    let mut h = Sha256::new();
    for part in [
        root.to_string_lossy().as_ref(),
        &entry.from,
        &entry.to,
        &entry.at.timestamp().to_string(),
    ] {
        h.update(part.as_bytes());
        h.update([0]);
    }
    hex::encode(&h.finalize()[..8])
}

/// Everything the pass needs to build records for one root.
pub(super) struct Context<'a> {
    pub root: &'a Path,
    pub host: &'a str,
    pub now: DateTime<Utc>,
    pub hold_active: bool,
}

/// The records for what `scan` found in `ctx.root`.
pub(super) fn records(ctx: &Context<'_>, scan: Scan) -> Vec<CheckoutExternalChangeRecord> {
    if scan.found.is_empty() {
        return Vec::new();
    }
    let repo = repo_slug(ctx.root);
    scan.found
        .into_iter()
        .map(|(name, entry, kind)| CheckoutExternalChangeRecord {
            change_id: change_id(ctx.root, &entry),
            repo: repo.clone(),
            visibility: RepoVisibility::Private,
            root: ctx.root.display().to_string(),
            host: ctx.host.to_string(),
            move_kind: kind.as_str().to_string(),
            ref_name: name,
            reflog_line: entry.message.clone(),
            from_sha: entry.from.clone(),
            to_sha: entry.to.clone(),
            at: entry.at,
            observed_at: ctx.now,
            discarded_commits: discarded(ctx.root, &entry.from),
            hold_active: ctx.hold_active,
            loom: Provenance::current(),
        })
        .collect()
}

// ============================================================================
// The pass's own fast-forward (#15729)
// ============================================================================

/// Whether `entry` is the one the pass's merge writes: Loom's [`FF_OP`] tag,
/// on a ref that moved.
fn is_own_fast_forward(entry: &Entry) -> bool {
    entry.from != entry.to
        && entry
            .message
            .strip_prefix(&tag(FF_OP))
            .is_some_and(|rest| rest.starts_with(REFLOG_ACTION_SEPARATOR))
}

/// The text of `logs/HEAD` in `root`; `None` when it cannot be read.
fn head_log(root: &Path) -> Option<String> {
    read_logs(root, None)
        .into_iter()
        .next()
        .map(|(_, text)| text)
}

/// The HEAD reflog as it stood immediately before the pass's merge, and the
/// commit HEAD was at then.
#[derive(Debug)]
pub(super) struct Before {
    mark: Mark,
    head: String,
}

/// Read [`Before`] in `root`. The reflog is read first and HEAD after, so a
/// move that lands between the two is a line after the mark and not a stale
/// `head`. `None` when either cannot be read: the move then emits no record,
/// and nothing else changes.
pub(super) fn before_merge(root: &Path) -> Option<Before> {
    let text = head_log(root)?;
    let head = git(root, &["rev-parse", "--verify", "HEAD"])?;
    let is_sha = |s: &str| s.len() >= 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
    is_sha(&head).then(|| {
        let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        Before {
            mark: Mark::of(&lines),
            head,
        }
    })
}

/// The entry the pass's merge added to `root`'s HEAD reflog since `before`
/// was read: the one Loom `ff` entry that starts at the commit the merge
/// began from.
///
/// `None` when the reflog cannot be read or no longer extends what was read
/// before the merge; when git wrote no such entry, because the checkout was
/// already at the tip and the merge had nothing to move; or when more than
/// one could be the pass's, as when another Loom pass moved the same checkout
/// meanwhile. Which entry is the pass's is then not known, and unknown is not
/// a fact.
pub(super) fn fast_forwarded(root: &Path, before: &Before) -> Option<Entry> {
    let text = head_log(root)?;
    let (added, _) = since(&text, Some(&before.mark));
    let mut own = added?
        .into_iter()
        .filter_map(parse_line)
        .filter(|entry| is_own_fast_forward(entry) && entry.from == before.head);
    let entry = own.next()?;
    own.next().is_none().then_some(entry)
}

/// The `checkout.fast_forward` record for the move `entry` describes.
pub(super) fn fast_forward_record(
    root: &Path,
    host: &str,
    now: DateTime<Utc>,
    branch: &str,
    entry: &Entry,
) -> CheckoutFastForwardRecord {
    CheckoutFastForwardRecord {
        change_id: change_id(root, entry),
        repo: repo_slug(root),
        visibility: RepoVisibility::Private,
        root: root.display().to_string(),
        host: host.to_string(),
        ref_name: format!("refs/heads/{branch}"),
        from_sha: entry.from.clone(),
        to_sha: entry.to.clone(),
        at: entry.at,
        observed_at: now,
        loom: Provenance::current(),
    }
}

/// Emit one `checkout.fast_forward` record. Best-effort: a record without
/// valid provenance is dropped, and nothing about the move depends on it.
pub(super) fn publish_fast_forward(record: &CheckoutFastForwardRecord) {
    if record.has_provenance() {
        crate::observability::ops::emit_record(TelemetryRecord::CheckoutFastForward(
            record.clone(),
        ));
    } else {
        log::warn!("checkout_ff: dropped checkout.fast_forward record: invalid provenance");
    }
}

// ============================================================================
// Publishing, and the hold-clear citation
// ============================================================================

type Cited = HashMap<PathBuf, Vec<(DateTime<Utc>, String)>>;

fn cited_cell() -> &'static Mutex<Cited> {
    static CITED: OnceLock<Mutex<Cited>> = OnceLock::new();
    CITED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Log, remember for the hold-clear citation, and emit one record.
pub(super) fn publish(record: &CheckoutExternalChangeRecord) {
    let discarded = record.discarded_commits.as_ref().map_or_else(
        || "unknown".to_string(),
        |d| format!("{} ({})", d.count, d.subjects.join("; ")),
    );
    log::warn!(
        "checkout_ff: {}: checkout.external_change {}: `{}` {} -> {} at {}; discarded commits: \
         {discarded}{}",
        record.root,
        record.change_id,
        record.reflog_line,
        record.from_sha,
        record.to_sha,
        record.at.to_rfc3339(),
        if record.hold_active {
            "; a dispatch hold stands"
        } else {
            ""
        }
    );
    let mut cited = cited_cell().lock().unwrap_or_else(PoisonError::into_inner);
    let list = cited.entry(PathBuf::from(&record.root)).or_default();
    list.push((record.observed_at, record.change_id.clone()));
    if list.len() > KEEP_CITED {
        list.remove(0);
    }
    drop(cited);
    if record.has_provenance() {
        crate::observability::ops::emit_record(TelemetryRecord::CheckoutExternalChange(
            record.clone(),
        ));
    } else {
        log::warn!("checkout_ff: dropped checkout.external_change record: invalid provenance");
    }
}

/// The text a hold's clear appends when external changes were seen in `root`
/// since the hold began (it coincides with them), else empty.
#[must_use]
pub fn cited(root: &Path, since: DateTime<Utc>) -> String {
    let cited = cited_cell().lock().unwrap_or_else(PoisonError::into_inner);
    let ids: Vec<&str> = cited
        .get(root)
        .into_iter()
        .flatten()
        .filter(|(at, _)| *at >= since)
        .map(|(_, id)| id.as_str())
        .collect();
    if ids.is_empty() {
        String::new()
    } else {
        format!("; coincides with checkout.external_change {}", ids.join(", "))
    }
}

/// The host id records are stamped with.
pub(super) fn host() -> String {
    crate::observability::ops::global_ops_sink()
        .map_or_else(crate::sweep_registry::host_identity, |sink| sink.host_id().to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "../tests/checkout_external_change.rs"]
mod tests;
