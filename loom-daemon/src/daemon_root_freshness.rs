//! `checkout.freshness` for the daemon's own root (#12727).
//!
//! The daemon root (`~/loom-daemon` on a release-roll host) is not a
//! registered workspace, so the checkout fast-forward pass never visits it and
//! nothing says how far behind GitHub it has drifted, or whether its dirty
//! tracked files are mere copies of origin's. This samples it on a timer and
//! emits the same OTLP-only `checkout.freshness` record, tagged
//! `checkout_role = daemon-root`. **Measurement only**: it never fetches into,
//! merges, or writes to the checkout (one `git ls-remote`, local reads).

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;

use crate::fleet_sync::checkout_ff::git::{self, LOCAL_TIMEOUT};
use crate::role_tick_telemetry::prompt_tree::{self, remote};
use crate::telemetry::kinds::checkout_freshness::CheckoutFreshnessRecord;
use crate::telemetry::TelemetryRecord;

/// How often the daemon root is sampled.
pub const SAMPLE_INTERVAL: Duration = Duration::from_secs(15 * 60);
const FIRST_SAMPLE_DELAY: Duration = Duration::from_secs(60);
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
const ROLE: &str = "daemon-root";
/// The `git_spawn` site this sampler's git children are logged under
/// (#15301), so its read-only probes are not mistaken for `checkout_ff` passes.
const SITE: &str = "daemon_root_freshness";

/// Spawn the sampler for `root`. Inert unless the OTLP exporter is configured.
pub fn spawn_task(root: PathBuf) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        tokio::time::sleep(FIRST_SAMPLE_DELAY).await;
        loop {
            if crate::observability::ops::records_exported() {
                let root = root.clone();
                let sampled = root.clone();
                match tokio::task::spawn_blocking(move || measure(&sampled)).await {
                    Ok(Some(record)) => {
                        if let Some(line) = warn_line(&root, &record) {
                            log::warn!("daemon_root_freshness: {line}");
                        }
                        crate::observability::ops::emit_record(TelemetryRecord::CheckoutFreshness(
                            record,
                        ));
                    }
                    Ok(None) => {}
                    Err(e) => log::warn!("daemon_root_freshness: sample failed: {e}"),
                }
            }
            tokio::time::sleep(SAMPLE_INTERVAL).await;
        }
    })
}

fn local(root: &Path, args: &[&str]) -> Option<String> {
    git::ok_as(SITE, root, args, LOCAL_TIMEOUT).ok()
}

/// The remote tip of `branch`, by `git ls-remote` (writes nothing).
fn remote_tip(root: &Path, branch: &str) -> Option<String> {
    let name = format!("refs/heads/{branch}");
    let listed =
        git::ok_as(SITE, root, &["ls-remote", "--quiet", "origin", "--", &name], PROBE_TIMEOUT)
            .ok()?;
    listed
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .find(|(sha, at)| *at == name && sha.len() >= 40)
        .map(|(sha, _)| sha.to_string())
}

/// Count of dirty tracked files, and whether all equal `tip`'s content.
fn dirty(root: &Path, tip: Option<&str>) -> (Option<u64>, Option<bool>) {
    let Some(names) = local(root, &["--no-optional-locks", "diff", "--name-only", "HEAD"]) else {
        return (None, None);
    };
    let paths: Vec<&str> = names.lines().filter(|l| !l.is_empty()).collect();
    let count = Some(paths.len() as u64);
    let identical = tip
        .filter(|_| !paths.is_empty())
        .filter(|t| local(root, &["cat-file", "-e", &format!("{t}^{{commit}}")]).is_some())
        .and_then(|t| {
            let mut args = vec!["--no-optional-locks", "diff", "--name-only", t, "--"];
            args.extend(paths.iter().copied());
            local(root, &args).map(|d| d.trim().is_empty())
        });
    (count, identical)
}

/// One sample of `root`; `None` when it is not a git checkout.
#[must_use]
pub fn measure(root: &Path) -> Option<CheckoutFreshnessRecord> {
    let mut tree = prompt_tree::read(root)?;
    let branch = git::default_branch(root, LOCAL_TIMEOUT);
    let tip = branch.as_deref().and_then(|b| remote_tip(root, b));
    let heard = branch.as_deref().zip(tip.as_deref());
    remote::fill(root, &mut tree, &remote::Expected::for_root(root), heard);
    let (dirty_tracked_files, dirty_identical_to_remote) = dirty(root, tip.as_deref());
    let repo = tree
        .remote_repo
        .clone()
        .filter(|_| tree.remote_foreign != Some(true));
    Some(CheckoutFreshnessRecord {
        captured_at: Utc::now(),
        repo,
        pass_skipped_reason: tip.is_none().then(|| "no-head".to_string()),
        remote_tip_age_s: None,
        checkout_role: Some(ROLE.to_string()),
        dirty_tracked_files,
        dirty_identical_to_remote,
        checkout_state: None,
        prompt: Some(tree),
    })
}

/// The one `WARN` line for a sample that shows the daemon root stalled
/// (#15301), or `None` when it is current. A stall is an `origin/<branch>`
/// the checkout never fetched (`origin_ref_stale`, which includes a tip object
/// missing locally, so the remote count is unknown), or any commit lag. It
/// names the root, why, and how far behind, so the checkout does not drift
/// hundreds of commits before anyone is told. Pure: the caller logs it.
#[must_use]
pub fn warn_line(root: &Path, record: &CheckoutFreshnessRecord) -> Option<String> {
    let tree = record.prompt.as_ref()?;
    let stale = tree.origin_ref_stale == Some(true);
    let behind_remote = tree.commits_behind_remote.filter(|n| *n > 0);
    let behind_local = tree.commits_behind.filter(|n| *n > 0);
    if !stale && behind_remote.is_none() && behind_local.is_none() {
        return None;
    }
    let mut reasons = Vec::new();
    if stale && tree.remote_tip.is_some() && tree.commits_behind_remote.is_none() {
        reasons.push("tip object missing (origin ref stale)".to_string());
    } else if stale {
        reasons.push("origin ref stale".to_string());
    }
    if let Some(n) = record.dirty_tracked_files.filter(|n| *n > 0) {
        reasons.push(format!("{n} dirty tracked file(s)"));
    }
    if reasons.is_empty() {
        reasons.push("behind".to_string());
    }
    let behind = match (behind_remote, behind_local) {
        (Some(n), _) => format!("{n} behind the remote tip"),
        (None, Some(n)) if tree.commits_behind_remote.is_none() => {
            format!("unknown (tip not fetched); {n} behind the local origin ref")
        }
        (None, Some(n)) => format!("{n} behind the local origin ref"),
        (None, None) if tree.commits_behind_remote.is_none() => {
            "unknown (tip not fetched)".to_string()
        }
        (None, None) => "0 behind the remote tip".to_string(),
    };
    Some(format!("{}: {}; {behind}", root.display(), reasons.join(", ")))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::process::Command;

    use super::*;

    fn git_in(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=T", "-c", "user.email=t@example.invalid"])
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    #[test]
    fn a_stale_clone_with_a_dirty_copy_of_origin_reports_lag_and_identical_dirt() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin.git");
        let seed = tmp.path().join("seed");
        let host = tmp.path().join("host");
        std::fs::create_dir(&seed).unwrap();
        git_in(
            tmp.path(),
            &[
                "init",
                "--quiet",
                "--bare",
                "-b",
                "main",
                origin.to_str().unwrap(),
            ],
        );
        git_in(&seed, &["init", "--quiet", "-b", "main"]);
        std::fs::write(seed.join("a.json"), "1").unwrap();
        git_in(&seed, &["add", "."]);
        git_in(&seed, &["commit", "--quiet", "-m", "one"]);
        git_in(&seed, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git_in(&seed, &["push", "--quiet", "origin", "main"]);
        git_in(
            tmp.path(),
            &[
                "clone",
                "--quiet",
                origin.to_str().unwrap(),
                host.to_str().unwrap(),
            ],
        );
        std::fs::write(seed.join("a.json"), "2").unwrap();
        git_in(&seed, &["commit", "--quiet", "-am", "two"]);
        git_in(&seed, &["push", "--quiet", "origin", "main"]);
        // The host's tracked file already holds origin's new content.
        std::fs::write(host.join("a.json"), "2").unwrap();
        git_in(&host, &["fetch", "--quiet", "origin"]);

        let r = measure(&host).unwrap();
        assert_eq!(r.checkout_role.as_deref(), Some("daemon-root"));
        assert_eq!(r.dirty_tracked_files, Some(1));
        assert_eq!(r.dirty_identical_to_remote, Some(true));
        // A local-path origin is "foreign" to the forge, so only the local ref count is set.
        assert_eq!(r.prompt.unwrap().commits_behind, Some(1));
    }

    fn sample(
        tree: crate::role_tick_telemetry::prompt_tree::PromptTree,
        dirty: Option<u64>,
    ) -> CheckoutFreshnessRecord {
        CheckoutFreshnessRecord {
            captured_at: Utc::now(),
            repo: None,
            pass_skipped_reason: None,
            remote_tip_age_s: None,
            checkout_role: Some(ROLE.to_string()),
            dirty_tracked_files: dirty,
            dirty_identical_to_remote: None,
            checkout_state: None,
            prompt: Some(tree),
        }
    }

    fn tree() -> crate::role_tick_telemetry::prompt_tree::PromptTree {
        crate::role_tick_telemetry::prompt_tree::PromptTree {
            commit: "a".repeat(40),
            loom_version: None,
            commits_behind: Some(0),
            on_branch: Some(true),
            remote_tip: Some("b".repeat(40)),
            commits_behind_remote: Some(0),
            origin_ref_stale: Some(false),
            remote_repo: None,
            remote_foreign: Some(false),
        }
    }

    #[test]
    fn a_current_sample_says_nothing() {
        assert_eq!(warn_line(Path::new("/r"), &sample(tree(), Some(0))), None);
    }

    #[test]
    fn a_missing_tip_object_warns_with_an_unknown_count() {
        let mut t = tree();
        t.origin_ref_stale = Some(true);
        t.commits_behind_remote = None;
        let line = warn_line(Path::new("/r"), &sample(t, Some(1))).unwrap();
        assert!(line.starts_with("/r: "), "{line}");
        assert!(line.contains("tip object missing"), "{line}");
        assert!(line.contains("1 dirty tracked file(s)"), "{line}");
        assert!(line.contains("unknown (tip not fetched)"), "{line}");
    }

    #[test]
    fn a_stale_ref_with_the_tip_present_names_the_remote_count() {
        let mut t = tree();
        t.origin_ref_stale = Some(true);
        t.commits_behind_remote = Some(490);
        let line = warn_line(Path::new("/r"), &sample(t, None)).unwrap();
        assert!(line.contains("origin ref stale"), "{line}");
        assert!(line.contains("490 behind the remote tip"), "{line}");
    }

    #[test]
    fn lag_alone_warns() {
        let mut t = tree();
        t.commits_behind_remote = Some(3);
        let line = warn_line(Path::new("/r"), &sample(t, None)).unwrap();
        assert!(line.contains("3 behind the remote tip"), "{line}");
    }

    #[test]
    fn a_non_checkout_is_not_sampled() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(measure(tmp.path()).is_none());
    }
}
