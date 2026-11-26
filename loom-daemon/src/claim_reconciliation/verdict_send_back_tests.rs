//! #15068 coverage for [`super::verdict_send_back`]: the daemon-side port of
//! `verdict-staleness-guard.sh` step 4b (#13586, hardened by #14516) — a
//! trusted human's hand-applied `loom:changes-requested` label, newer than
//! the newest marker and followed by no head move, pins the verdict to the
//! current head so the stale-verdict pass cannot clear an operator's
//! rejection and re-queue the head they refused.
//!
//! The `(v)`/`(w)` cases (plus the #14470 case) in
//! `defaults/scripts/tests/test-verdict-staleness-guard.sh` are the shell
//! guard's half of the same fixtures: the two implementations are pinned to
//! agree on this shape, and a change to either side's conditions must be
//! mirrored here or there.

#![allow(clippy::unwrap_used)]

use super::verdict_send_back::{describes_current_head, LabelEvent, MarkerComment};
use chrono::{DateTime, Utc};

/// The #14470 incident's SHAs, verbatim — the same fixture
/// `test-verdict-staleness-guard.sh` case (v8) pins the shell guard with.
const SHA_A: &str = "3efb454aff553b10483684e2bf4ffdf5a8628a02";
const SHA_B: &str = "bcf2c895d92b25e1a286ae31b98542f5835a0bba";

fn ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

/// The #14470 incident, as evidence: the fleet App's bot verdict marker names
/// `SHA_A` (3efb454a…), the head is `SHA_B` (bcf2c895…), and `rjwalters`
/// applied the label a day after the marker with no push between.
fn incident_14470() -> (MarkerComment, LabelEvent) {
    (
        MarkerComment {
            author: "loom-fleet-dispatch[bot]".to_string(),
            at: ts("2026-11-20T09:00:00Z"),
        },
        LabelEvent {
            actor: "rjwalters".to_string(),
            at: ts("2026-11-21T00:43:32Z"),
            head_moves_after: 0,
        },
    )
}

#[test]
fn the_14470_shape_pins_the_verdict() {
    let (marker, event) = incident_14470();
    assert!(describes_current_head(&marker, &event));
}

#[test]
fn the_verdict_writers_own_label_is_not_a_send_back() {
    // #14516: post-verdict.sh writes the marker and applies the label under
    // one identity, so a label by the marker's author is the verdict write,
    // not a send-back — even spelled with different case.
    let (marker, event) = incident_14470();
    let mut by_author = event;
    by_author.actor = "Loom-Fleet-Dispatch[Bot]".to_string();
    assert!(!describes_current_head(&marker, &by_author));
}

#[test]
fn a_label_older_than_the_marker_is_not_a_send_back() {
    let (marker, mut event) = incident_14470();
    event.at = ts("2026-11-19T00:00:00Z");
    assert!(!describes_current_head(&marker, &event));
}

#[test]
fn a_head_move_after_the_label_is_not_a_send_back() {
    // By position, not commit date: this is the count of `committed` /
    // `head_ref_force_pushed` events the timeline lists AFTER the label.
    let (marker, mut event) = incident_14470();
    event.head_moves_after = 1;
    assert!(!describes_current_head(&marker, &event));
}

#[test]
fn an_unknown_marker_author_is_not_a_send_back() {
    // A trusted comment carrying no login (a deleted account) cannot prove
    // the labeler is a DIFFERENT identity — insufficient evidence fails
    // closed to the marker comparison.
    let (mut marker, event) = incident_14470();
    marker.author = String::new();
    assert!(!describes_current_head(&marker, &event));
}

// --- end to end through `forge::reconcile_pr_verdicts` ----------------------
//
// PR #400 carries `loom:changes-requested` at head `SHA_B`; the newest
// trusted marker (by the fleet App) names `SHA_A`. `gh` is stubbed with the
// marker comment, a timeline and a collaborator-permission answer.

mod e2e {
    use super::super::open_pr_listing::test_support::{pulls_arm, row};
    use super::super::verdict_send_back::describes_current_head;
    use super::super::verdict_send_back_tests::{incident_14470, ts, SHA_A, SHA_B};
    use crate::claim_reconciliation::forge;
    use crate::claim_reconciliation::verdict_invalidation::VERDICT_TREE_CARVEOUT_ENABLED_ENV;
    use crate::claim_reconciliation::VerdictReconcileStats;
    use serial_test::serial;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    /// One pass over PR #400. `timeline` is the PR's timeline JSON, `perm`
    /// the collaborator-permission answer for `rjwalters` (`write` trusts,
    /// anything else does not). Returns the stats and every write the pass
    /// attempted. The tree carve-out is switched OFF so a would-be clear
    /// takes the ordinary path without the equivalence machinery (its
    /// evidence gate has its own coverage in `verdict_dedup_tests.rs`).
    fn reconcile(timeline: &str, perm: &str) -> (VerdictReconcileStats, String) {
        reconcile_with(timeline, perm, None)
    }

    /// As [`reconcile`], with an optional `forge.trustedCommenters` login
    /// and `perm == "fail"` making the permission probe exit non-zero.
    fn reconcile_with(
        timeline: &str,
        perm: &str,
        allowlisted: Option<&str>,
    ) -> (VerdictReconcileStats, String) {
        let dir = tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir_all(root.join(".loom")).unwrap();
        if let Some(login) = allowlisted {
            std::fs::write(
                root.join(".loom/config.json"),
                format!(r#"{{"forge":{{"trustedCommenters":["{login}"]}}}}"#),
            )
            .unwrap();
        }
        let perm_arm = if perm == "fail" {
            "echo 'Not Found' >&2; exit 1".to_string()
        } else {
            format!("echo '{perm}'")
        };
        let comments = dir.path().join("comments.json");
        std::fs::write(
            &comments,
            format!(
                r#"[{{"user":{{"login":"loom-fleet-dispatch[bot]","type":"Bot"}},"author_association":"NONE","created_at":"{marker_at}","body":"Rejected.\n\n<!-- loom:verdict-sha sha={SHA_A} verdict=changes-requested -->"}}]"#,
                marker_at = "2026-11-20T09:00:00Z",
            ),
        )
        .unwrap();
        let tl = dir.path().join("timeline.json");
        std::fs::write(&tl, timeline).unwrap();
        let writes = dir.path().join("writes.log");
        let gh = dir.path().join("fake-gh.sh");
        std::fs::write(
            &gh,
            format!(
                r#"#!/usr/bin/env bash
{pulls}case "$*" in
  "pr edit "*|"pr comment "*) printf '%s\n' "$*" >> "{writes}" ;;
  "api repos/{{owner}}/{{repo}}/issues/400/comments"*) cat "{comments}" ;;
  "api repos/{{owner}}/{{repo}}/issues/400/timeline"*) cat "{tl}" ;;
  "api repos/{{owner}}/{{repo}}/collaborators/rjwalters/permission"*) {perm_arm} ;;
  *) echo '{{}}' ;;
esac
"#,
                writes = writes.display(),
                pulls = pulls_arm(&[row(400, &["loom:changes-requested"]).sha(SHA_B)]),
                comments = comments.display(),
                tl = tl.display(),
                perm_arm = perm_arm,
            ),
        )
        .unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        let prev_gh_bin = std::env::var_os("LOOM_GH_BIN");
        let prev_carveout = std::env::var_os(VERDICT_TREE_CARVEOUT_ENABLED_ENV);
        std::env::set_var("LOOM_GH_BIN", &gh);
        std::env::set_var(VERDICT_TREE_CARVEOUT_ENABLED_ENV, "0");
        let stats = forge::reconcile_pr_verdicts(&gh, &root);
        match prev_carveout {
            Some(v) => std::env::set_var(VERDICT_TREE_CARVEOUT_ENABLED_ENV, v),
            None => std::env::remove_var(VERDICT_TREE_CARVEOUT_ENABLED_ENV),
        }
        match prev_gh_bin {
            Some(v) => std::env::set_var("LOOM_GH_BIN", v),
            None => std::env::remove_var("LOOM_GH_BIN"),
        }
        (stats, std::fs::read_to_string(&writes).unwrap_or_default())
    }

    /// The timeline of the #14470 incident: the head commit landed before the
    /// verdict, and the operator's label is the newest event.
    fn incident_timeline() -> String {
        format!(
            r#"[{{"event":"committed","sha":"{SHA_B}","committed_date":"{commit_at}"}},{{"event":"labeled","label":{{"name":"loom:changes-requested"}},"actor":{{"login":"rjwalters"}},"created_at":"{label_at}"}}]"#,
            commit_at = "2026-11-19T01:07:00Z",
            label_at = incident_14470().1.at.to_rfc3339(),
        )
    }

    #[test]
    #[serial]
    fn a_trusted_send_back_keeps_the_verdict_and_writes_nothing() {
        let (stats, writes) = reconcile(&incident_timeline(), "write");
        assert_eq!(stats.checked, 1, "{stats:?}");
        assert_eq!(stats.invalidated, 0, "the operator's rejection stands: {stats:?}");
        assert_eq!(stats.anchored, 0, "{stats:?}");
        assert_eq!(stats.unverifiable, 0, "{stats:?}");
        assert!(
            !writes.contains("pr edit") && !writes.contains("pr comment"),
            "no label flip, no stale-verdict notice: {writes}"
        );
    }

    #[test]
    #[serial]
    fn a_labeler_without_write_access_fails_closed_to_the_clear() {
        let (stats, writes) = reconcile(&incident_timeline(), "none");
        assert_eq!(stats.invalidated, 1, "untrusted actor -> ordinary clear: {stats:?}");
        assert!(writes.contains("--add-label loom:review-requested"), "{writes}");
    }

    #[test]
    #[serial]
    fn an_unreadable_probe_still_trusts_an_allowlisted_login() {
        // The probe 404s (not a collaborator) but the login is on
        // `forge.trustedCommenters`: the shell guard keeps the verdict.
        let (stats, writes) = reconcile_with(&incident_timeline(), "fail", Some("rjwalters"));
        assert_eq!(stats.invalidated, 0, "login-trusted send-back stands: {stats:?}");
        assert!(!writes.contains("pr edit"), "{writes}");
    }

    #[test]
    #[serial]
    fn an_unreadable_probe_for_an_unlisted_login_fails_closed() {
        let (stats, _) = reconcile_with(&incident_timeline(), "fail", None);
        assert_eq!(stats.invalidated, 1, "{stats:?}");
    }

    #[test]
    #[serial]
    fn a_commit_after_the_label_fails_closed_to_the_clear() {
        let tl = format!(
            r#"[{{"event":"labeled","label":{{"name":"loom:changes-requested"}},"actor":{{"login":"rjwalters"}},"created_at":"{}"}},{{"event":"committed","sha":"{SHA_B}","committed_date":"{}"}}]"#,
            ts("2026-11-21T00:43:32Z"),
            "2026-11-21T01:00:00Z",
        );
        let (stats, writes) = reconcile(&tl, "write");
        assert_eq!(stats.invalidated, 1, "head moved after the label: {stats:?}");
        assert!(writes.contains("--add-label loom:review-requested"), "{writes}");
    }

    #[test]
    #[serial]
    fn an_unreadable_timeline_fails_closed_to_the_clear() {
        // `{}` parses as no events listing at all — the fetch "succeeded" but
        // the answer is unusable, which must read as no evidence.
        let (stats, writes) = reconcile("{}", "write");
        assert_eq!(stats.invalidated, 1, "unreadable evidence -> ordinary clear: {stats:?}");
        assert!(writes.contains("--add-label loom:review-requested"), "{writes}");
    }

    /// Sanity for the fixture itself: the timeline used above, parsed against
    /// the incident's marker, satisfies the pure predicate.
    #[test]
    fn the_incident_fixture_matches_the_pure_predicate() {
        let (marker, event) = incident_14470();
        assert!(describes_current_head(&marker, &event));
    }
}
