//! `loom-daemon guide work-plan-receipt record|reset|read` and
//! `loom-daemon guide work-plan-splice` — Guide Step 3's WORK_PLAN.md write
//! path (issue #15219).
//!
//! The role prompt's Step 3 block is pasted into a Bash tool call verbatim,
//! so every file write it used to perform inline (`rm -f` and `>` on the
//! variable-built `$DOCS_WT/...` paths, a `mktemp -d TEMPLATE` scratch dir)
//! was denied by the default guard (`guards.rmScope=repo`, #4921 write
//! confinement, #6520 mktemp fast path) BEFORE anything executed — which
//! meant the #13923 provenance gate was unreachable from a guarded shell.
//! Moving the writes behind this verb leaves the pasted block nothing for
//! the guard to deny: the role file calls it with the literal `--docs-worktree`
//! path it already holds, and the verb owns the receipt file, the marker
//! check, the scratch file and the atomic replace.
//!
//! Receipt contract (#14204/#14259): `<docs-worktree>/.loom-work-plan-receipt`
//! holds `key=value` lines (`receipt=`, `render_hash=`), one per line, first
//! match per key wins. The file is never staged (staging is by exact path)
//! and the docs-worktree reset removes it.
//!
//! Splice contract (#13923): replace ONLY the text between the
//! `<!-- guide:plan-body:start -->` / `<!-- guide:plan-body:end -->` markers
//! of `<docs-worktree>/WORK_PLAN.md` with the body read on stdin (markers
//! kept, everything outside untouched). Fails non-zero with the file
//! untouched when either marker is missing or duplicated; leaves no
//! scratch file behind on success or failure.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::work_docs_archive::{atomic_write, read_regular};

pub(crate) const RECEIPT_FILE: &str = ".loom-work-plan-receipt";
const PLAN_FILE: &str = "WORK_PLAN.md";
pub(crate) const BODY_START: &str = "<!-- guide:plan-body:start -->";
pub(crate) const BODY_END: &str = "<!-- guide:plan-body:end -->";

#[derive(clap::Subcommand)]
pub(crate) enum GuideCommand {
    /// Guide's WORK_PLAN receipt file (#14204/#14259): `record` writes the
    /// outcome classification and render hash, `reset` clears a stale file,
    /// `read` prints one key's value (nothing when the file or key is
    /// absent). Owns `<docs-worktree>/.loom-work-plan-receipt` so the role
    /// prompt's Step 3 block performs no inline file write (#15219).
    #[command(subcommand)]
    WorkPlanReceipt(WorkPlanReceiptCommand),
    /// Replace the `guide:plan-body` marker region of
    /// `<docs-worktree>/WORK_PLAN.md` with the body read on stdin (#13923).
    /// The verb owns the scratch file and the atomic replace; fails
    /// non-zero with the file untouched when either marker is missing or
    /// duplicated.
    WorkPlanSplice(WorkPlanSpliceArgs),
}

#[derive(clap::Subcommand)]
pub(crate) enum WorkPlanReceiptCommand {
    /// Write `receipt=<outcome>` and `render_hash=<hash>` to the receipt
    /// file, replacing any previous content.
    Record(RecordArgs),
    /// Remove the receipt file (a no-op when absent), so a previous tick's
    /// outcome can never be read as this tick's.
    Reset(ResetArgs),
    /// Print the first `<field>=` value from the receipt file; print
    /// nothing when the file or the key is absent.
    Read(ReadArgs),
}

#[derive(clap::Args)]
pub(crate) struct DocsWorktreeArgs {
    /// The Guide docs worktree holding the receipt file (the literal
    /// `DOCS_WT` path Step 1 already resolved).
    #[arg(long, value_name = "DIR")]
    pub(super) docs_worktree: PathBuf,
}

#[derive(clap::Args)]
pub(crate) struct RecordArgs {
    #[command(flatten)]
    wt: DocsWorktreeArgs,

    /// Outcome classification (`written`, `current`, `debounced (...)`,
    /// `provenance-refused`, `inventory-unreadable`, `write-failed`).
    #[arg(long, value_name = "TEXT")]
    outcome: String,

    /// First 12 hex of the rendered body's sha256. Empty when unset.
    #[arg(long, value_name = "HEX", default_value_t)]
    render_hash: String,
}

#[derive(clap::Args)]
pub(crate) struct ResetArgs {
    #[command(flatten)]
    wt: DocsWorktreeArgs,
}

#[derive(clap::Args)]
pub(crate) struct ReadArgs {
    #[command(flatten)]
    wt: DocsWorktreeArgs,

    /// Key to read (`receipt`, `render_hash`).
    #[arg(long, value_name = "NAME")]
    field: String,
}

#[derive(clap::Args)]
pub(crate) struct WorkPlanSpliceArgs {
    #[command(flatten)]
    wt: DocsWorktreeArgs,
}

impl GuideCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::WorkPlanReceipt(cmd) => cmd.run(),
            Self::WorkPlanSplice(args) => {
                let mut body = String::new();
                std::io::stdin()
                    .read_to_string(&mut body)
                    .context("read splice body on stdin")?;
                splice_file(&args.wt.docs_worktree, &body)
            }
        }
    }
}

impl WorkPlanReceiptCommand {
    pub(crate) fn run(self) -> Result<()> {
        match self {
            Self::Record(args) => {
                record_receipt(&args.wt.docs_worktree, &args.outcome, &args.render_hash)?;
            }
            Self::Reset(args) => {
                reset_receipt(&args.wt.docs_worktree)?;
            }
            Self::Read(args) => {
                // Absent file or key prints nothing and still exits 0, the
                // same answer the role file's old `sed -n | head -n1` gave.
                if let Some(value) = read_field(&args.wt.docs_worktree, &args.field)? {
                    println!("{value}");
                }
            }
        }
        Ok(())
    }
}

fn receipt_path(docs_worktree: &Path) -> PathBuf {
    docs_worktree.join(RECEIPT_FILE)
}

/// Write the two-line receipt, replacing any previous file.
pub(crate) fn record_receipt(docs_worktree: &Path, outcome: &str, render_hash: &str) -> Result<()> {
    let text = format!("receipt={outcome}\nrender_hash={render_hash}\n");
    atomic_write(&receipt_path(docs_worktree), &text)
}

/// Remove the receipt file; a missing file is success (idempotent `rm -f`).
pub(crate) fn reset_receipt(docs_worktree: &Path) -> Result<()> {
    match fs::remove_file(receipt_path(docs_worktree)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => {
            Err(e).with_context(|| format!("remove {}", receipt_path(docs_worktree).display()))
        }
    }
}

/// First `<field>=` value in the receipt file; `None` when file or key is
/// absent. Line-based literal prefix match (the file is data, never code).
pub(crate) fn read_field(docs_worktree: &Path, field: &str) -> Result<Option<String>> {
    let Some(text) = read_regular(&receipt_path(docs_worktree))? else {
        return Ok(None);
    };
    let prefix = format!("{field}=");
    Ok(text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned)))
}

/// The new plan text with `body` spliced between the markers, or an error
/// naming the offending marker. Pure: reads nothing, writes nothing.
///
/// The plan must carry exactly one `guide:plan-body` marker pair. On a
/// well-formed plan the output is byte-identical to the awk this verb
/// retired (issue #13923): the start-marker line is kept and the body
/// inserted after it, lines up to (and including) the end-marker line are
/// dropped, everything else passes through. Every emitted line is
/// newline-terminated, so a plan missing its final newline is normalized
/// exactly as the awk's `print` normalized it. A duplicated marker is a
/// deliberate divergence: the retired awk silently emptied the second
/// region (it never re-read the body file after the first drain), and
/// neither that nor re-inserting the body repairs a corrupt plan, so the
/// splice refuses and the caller's file stays untouched.
pub(crate) fn splice_text(plan: &str, body: &str) -> Result<String> {
    if !plan.contains(BODY_START) {
        bail!("WORK_PLAN.md has no {BODY_START} marker; refusing to write");
    }
    if !plan.contains(BODY_END) {
        bail!("WORK_PLAN.md has no {BODY_END} marker; refusing to write");
    }
    // Count with the same predicate the loop below uses (`contains`, so a
    // mid-line marker counts too): a second marker of either kind means a
    // corrupt plan, and bailing here keeps the whole file untouched.
    if plan.lines().filter(|l| l.contains(BODY_START)).count() > 1 {
        bail!("WORK_PLAN.md has more than one {BODY_START} marker; refusing to write");
    }
    if plan.lines().filter(|l| l.contains(BODY_END)).count() > 1 {
        bail!("WORK_PLAN.md has more than one {BODY_END} marker; refusing to write");
    }
    // The role file piped the body through `printf '%s\n'`, and awk's
    // `getline`+`print` newline-terminates every line: strip one trailing
    // newline, then split. An empty body is one empty line, matching
    // `printf '%s\n' ""`.
    let body = body.strip_suffix('\n').unwrap_or(body);
    let mut out = String::new();
    let mut skip = false;
    for line in plan.split_inclusive('\n') {
        let bare = line.trim_end_matches('\n');
        if bare.contains(BODY_START) {
            out.push_str(bare);
            out.push('\n');
            for bl in body.split('\n') {
                out.push_str(bl);
                out.push('\n');
            }
            skip = true;
            continue;
        }
        if bare.contains(BODY_END) {
            skip = false;
        }
        if !skip {
            out.push_str(bare);
            out.push('\n');
        }
    }
    Ok(out)
}

/// Splice `body` into `<docs_worktree>/WORK_PLAN.md`. The file is left
/// untouched on any failure (markers missing, file absent, read error).
pub(crate) fn splice_file(docs_worktree: &Path, body: &str) -> Result<()> {
    let plan_path = docs_worktree.join(PLAN_FILE);
    let Some(plan) = read_regular(&plan_path)? else {
        bail!("{} not found in {}", PLAN_FILE, docs_worktree.display());
    };
    let spliced = splice_text(&plan, body)?;
    atomic_write(&plan_path, &spliced)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "guide_work_plan_tests.rs"]
mod tests;
