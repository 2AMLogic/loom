//! `loom-daemon ratification-lane status <PR>` — the read-only report that
//! names, for a PR's current head, the **effective** ee- and market-key
//! review markers and flags the superseded duplicates the pre-#14800 retry
//! bug piled up (#14834). Only reads: no dispatch, no post, no edit, no
//! hold release. "Effective" identifies the review to read first; the human
//! retains the ratification decision.
//!
//! It also prints this host's attempt bound for that head (#15315): per key,
//! the counted attempts in the trailing window from the workspace's attempt
//! ledger, and `CAPPED until <time>` for a key the lane will not dispatch
//! again until then. There is no verb to clear it: deleting the ledger file
//! the block names is the reset.
//!
//! The sub-action enum lives here, not in `root.rs`, because that file is
//! frozen by the file-size ratchet — the same reason `cli::inflight` is
//! shaped this way.

use anyhow::{anyhow, Result};
use clap::Subcommand;
use std::path::{Path, PathBuf};

use crate::role_runner::ratification_attempts::{
    key_attempts, ledger_path, Ledger, ATTEMPT_WINDOW_HOURS, MAX_COUNTED_ATTEMPTS_PER_WINDOW,
};
use crate::role_runner::ratification_lane::{resolve_status, GhPrSource, Key, KeyStatus};

#[derive(Subcommand)]
pub(crate) enum Action {
    /// Print the effective ee- and market-key review markers for a PR's
    /// current head, flagging superseded duplicates (#14834). Read-only.
    ///
    /// Prints the reviewed head SHA, then one row per key linking the
    /// effective review and its superseded duplicates. A key a complete read
    /// finds no marker for is printed as MISSING. Then this host's attempt
    /// bound for that head (#15315): each key's counted attempts in the
    /// window, and CAPPED until the time the lane may dispatch it again.
    /// Exits non-zero when the PR head or the comment history cannot be
    /// read; an unreadable attempt ledger is reported, not an error.
    Status {
        /// The PR number.
        #[arg(value_name = "PR")]
        pr: u64,

        /// The `owner/name` repository. Defaults to the current directory's
        /// git remote.
        #[arg(long, value_name = "NWO")]
        repo: Option<String>,

        /// Workspace root for config and trust resolution. Defaults to the
        /// current directory.
        #[arg(long, value_name = "PATH")]
        root: Option<PathBuf>,
    },
}

pub(crate) fn handle(action: Action) -> Result<()> {
    let Action::Status { pr, repo, root } = action;
    let root = match root {
        Some(r) => r,
        None => std::env::current_dir()
            .map_err(|e| anyhow!("could not resolve the working directory: {e}"))?,
    };
    let nwo = match repo {
        Some(n) => n,
        None => crate::credential_preflight::nwo_from_git_remote(&root).ok_or_else(|| {
            anyhow!(
                "could not resolve the repository from {}; pass --repo owner/name",
                root.display()
            )
        })?,
    };
    let policy = crate::comment_trust::TrustPolicy::for_root(&root);
    let source = GhPrSource::new(&root, &nwo, &policy);
    let head = source
        .pr_head(pr)
        .map_err(|why| anyhow!("{nwo}#{pr}: head could not be read: {why}"))?;
    let comments = source
        .marker_comments(pr)
        .map_err(|why| anyhow!("{nwo}#{pr}: {why}"))?;
    let status = resolve_status(&head, &nwo, pr, &comments);
    println!("{nwo}#{pr}");
    println!("Reviewed head: {}", status.head_sha);
    println!("{}", key_row(&status.ee));
    println!("{}", key_row(&status.market));
    let path = ledger_path(&root);
    let ledger = crate::role_runner::ratification_attempts::load(&path);
    let view = AttemptsView {
        path: &path,
        nwo: &nwo,
        pr,
        head: &status.head_sha,
        now: chrono::Utc::now(),
    };
    println!("{}", attempts_block(ledger.as_ref().map_err(String::as_str), &view));
    Ok(())
}

/// What the attempt block reports on.
struct AttemptsView<'a> {
    path: &'a Path,
    nwo: &'a str,
    pr: u64,
    head: &'a str,
    now: chrono::DateTime<chrono::Utc>,
}

/// This host's attempt bound for the head (#15315), from the same
/// [`key_attempts`] the lane gates on: per key, its counted attempts in the
/// window (one line each) and, when capped, the release time. An unreadable
/// ledger is named, with what the lane does about it.
fn attempts_block(ledger: Result<&Ledger, &str>, v: &AttemptsView<'_>) -> String {
    let mut out = format!("Attempts on this host ({}):", v.path.display());
    let ledger = match ledger {
        Ok(l) => l,
        Err(why) => {
            out.push_str(&format!(
                "\n  ledger unreadable ({why}); the lane treats it as empty and caps nothing"
            ));
            return out;
        }
    };
    for key in [Key::Ee, Key::Market] {
        let k = key_attempts(ledger, v.nwo, v.pr, v.head, key, v.now);
        let n = k.counted.len();
        let counted = format!(
            "{n}/{MAX_COUNTED_ATTEMPTS_PER_WINDOW} counted withheld at this head in the last {ATTEMPT_WINDOW_HOURS}h"
        );
        out.push_str(&match k.capped_until {
            Some(until) => format!(
                "\n  {}: CAPPED until {} — {counted}",
                key.wire(),
                until.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            ),
            None => format!("\n  {}: {counted}", key.wire()),
        });
        for a in &k.counted {
            out.push_str(&format!(
                "\n    {} {}",
                a.at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                a.class
            ));
        }
    }
    out.push_str(
        "\n  (launch refusals and host failures are not counted; see \
         loom.ratification.<key>.pre_launch_failed. Delete the file to reset.)",
    );
    out
}

/// One key's block: the effective link (or the explicit missing-key line),
/// then one indented line per superseded duplicate.
fn key_row(k: &KeyStatus) -> String {
    let mut out = match &k.effective {
        Some(m) => {
            format!("{}: effective {} {} ({})", k.key.wire(), stamp(m), m.url, m.format.label())
        }
        None => format!(
            "{}: MISSING — no trusted {} marker names this head",
            k.key.wire(),
            k.key.wire()
        ),
    };
    for s in &k.superseded {
        out.push_str(&format!("\n  superseded {} {} ({})", stamp(s), s.url, s.format.label()));
    }
    out
}

/// RFC 3339, second precision — the forge's own comment-creation stamp.
fn stamp(m: &crate::role_runner::ratification_lane::MarkerLink) -> String {
    m.created_at
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::role_runner::ratification_lane::{Key, MarkerFormat, MarkerLink};

    fn link(id: u64, at: &str) -> MarkerLink {
        MarkerLink {
            comment_id: id,
            created_at: chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S")
                .unwrap()
                .and_utc(),
            url: format!("https://example/pull/7#issuecomment-{id}"),
            format: MarkerFormat::Lane,
        }
    }

    #[test]
    fn key_row_names_the_effective_marker_then_superseded_links() {
        let k = KeyStatus {
            key: Key::Market,
            effective: Some(MarkerLink {
                format: MarkerFormat::Hand,
                ..link(30, "2026-11-20T19:14:25")
            }),
            superseded: vec![
                link(20, "2026-11-20T19:00:37"),
                link(25, "2026-11-20T19:05:00"),
            ],
        };
        let row = key_row(&k);
        assert!(row.starts_with("market: effective 2026-11-20T19:14:25Z "), "{row}");
        assert!(row.contains("#issuecomment-30 (RATIFY-KEY)"), "{row}");
        assert!(row.contains("\n  superseded 2026-11-20T19:00:37Z "), "{row}");
        assert!(row.contains("\n  superseded 2026-11-20T19:05:00Z "), "{row}");
    }

    #[test]
    fn key_row_says_missing_explicitly() {
        let k = KeyStatus {
            key: Key::Ee,
            effective: None,
            superseded: Vec::new(),
        };
        let row = key_row(&k);
        assert!(row.starts_with("ee: MISSING"), "{row}");
        assert!(row.contains("no trusted ee marker"), "{row}");
    }

    fn at(s: &str) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .with_timezone(&chrono::Utc)
    }

    const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";

    fn attempt(key: Key, stamp: &str) -> crate::role_runner::ratification_attempts::AttemptRecord {
        crate::role_runner::ratification_attempts::AttemptRecord {
            repo: "o/r".into(),
            pr: 7,
            head_sha: HEAD.into(),
            key: key.wire().into(),
            class: "verdict_placeholder".into(),
            at: at(stamp),
        }
    }

    fn view(now: &str) -> AttemptsView<'static> {
        AttemptsView {
            path: Path::new("/w/.loom/state/ratification-lane/attempts.json"),
            nwo: "o/r",
            pr: 7,
            head: HEAD,
            now: at(now),
        }
    }

    #[test]
    fn status_shows_attempts_and_capped_line() {
        let ledger = Ledger {
            attempts: vec![
                attempt(Key::Ee, "2026-11-22T10:00:00Z"),
                attempt(Key::Ee, "2026-11-22T11:00:00Z"),
                attempt(Key::Ee, "2026-11-22T12:00:00Z"),
                attempt(Key::Market, "2026-11-22T12:30:00Z"),
                // Another head is never counted here.
                crate::role_runner::ratification_attempts::AttemptRecord {
                    head_sha: "f".repeat(40),
                    ..attempt(Key::Market, "2026-11-22T12:31:00Z")
                },
            ],
            ..Ledger::default()
        };
        let block = attempts_block(Ok(&ledger), &view("2026-11-22T13:00:00Z"));
        assert!(
            block.starts_with(
                "Attempts on this host (/w/.loom/state/ratification-lane/attempts.json):"
            ),
            "{block}"
        );
        assert!(
            block.contains(
                "\n  ee: CAPPED until 2026-11-23T10:00:00Z — 3/3 counted withheld at this head in the last 24h"
            ),
            "{block}"
        );
        assert!(block.contains("\n    2026-11-22T11:00:00Z verdict_placeholder"), "{block}");
        assert!(
            block.contains("\n  market: 1/3 counted withheld at this head in the last 24h\n"),
            "{block}"
        );
        assert!(!block.contains("12:31"), "{block}");
        assert!(block.contains("launch refusals and host failures are not counted"), "{block}");
        assert!(block.contains("pre_launch_failed"), "{block}");

        // A day later the window holds nothing: no cap, no lines.
        let block = attempts_block(Ok(&ledger), &view("2026-11-23T13:00:00Z"));
        assert!(block.contains("\n  ee: 0/3 counted"), "{block}");
        assert!(!block.contains("CAPPED"), "{block}");
    }

    #[test]
    fn attempts_block_reports_an_unreadable_ledger_without_failing() {
        let block = attempts_block(Err("did not parse: EOF"), &view("2026-11-22T13:00:00Z"));
        assert!(block.contains("ledger unreadable (did not parse: EOF)"), "{block}");
        assert!(block.contains("caps nothing"), "{block}");
        assert!(!block.contains("ee:"), "{block}");
    }

    #[test]
    fn documented_invocation_parses_through_the_cli_entry_point() {
        let cli = crate::cli::whole_cli_parse::try_parse_cli(&[
            "loom-daemon",
            "ratification-lane",
            "status",
            "98",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(crate::cli::Commands::RatificationLane {
                action: Action::Status { pr: 98, .. }
            })
        ));
        let with_flags = crate::cli::whole_cli_parse::try_parse_cli(&[
            "loom-daemon",
            "ratification-lane",
            "status",
            "7",
            "--repo",
            "o/r",
            "--root",
            "/tmp/x",
        ])
        .unwrap();
        assert!(matches!(
            with_flags.command,
            Some(crate::cli::Commands::RatificationLane {
                action: Action::Status { pr: 7, .. }
            })
        ));
    }
}
