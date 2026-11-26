//! What survives of the required-check freshness guard (#8248): the
//! [`CheckRun`] row and the shared forge reads in [`fetch`].
//!
//! The guard itself, its input-scoped predicate (#8919), the merge-tree
//! re-verification (#10388) and the `.loom/stale-check-inputs.json`
//! declaration (#9589) were deleted by #12063 slice 1, together with the
//! re-date remedy (#8508) and the CI-run conclusion gate (#10444): required
//! checks at the head are GitHub's to enforce (ruleset, `strict=false`), and
//! the occasional semantic conflict is caught by main CI and reverted. The
//! reads stay for `required-checks` and `forge wait-checks` (the other
//! `merge-pr` verbs that shared them went in slice 2).

use chrono::{DateTime, Utc};

/// One check run on the PR head, as the Checks API reports it.
///
/// `started_at` is `None` for a run that has not started yet (GitHub leaves it
/// null while a run is queued) and for API responses that omitted it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckRun {
    pub name: String,
    /// `completed`, `in_progress`, `queued`, … — preserved verbatim so a
    /// settled row whose raw `status` disagreed can still report it (#15371).
    pub status: String,
    /// Whether [`super::check_runs_rollup::is_settled`] classified the raw row
    /// as terminal (`status == "completed"`, or a string `conclusion` with a
    /// non-null `completed_at`, #13758 / #15371).
    pub settled: bool,
    /// `Some("success")`, `Some("failure")`, …; `None` until completed.
    pub conclusion: Option<String>,
    /// When the run started; `None` until it has.
    pub started_at: Option<DateTime<Utc>>,
}

pub mod fetch;
