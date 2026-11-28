//! `checkout.fast_forward` (#15729): one fast-forward that the daemon's own
//! checkout pass performed on a managed checkout.
//!
//! # Why this kind exists
//!
//! `checkout.external_change` reports every reflog entry that moved a managed
//! checkout and was **not** Loom's, and `checkout.freshness` shows where each
//! checkout's HEAD was per pass. Neither named the mover when the checkout was
//! moved by the fast-forward pass itself: the pass wrote `loom:ff: Fast-forward`
//! to the reflog and emitted nothing for the move. So a move by the pass and a
//! plain `git pull` by anything else could not be told apart from the store
//! (2AMLogic/2am#8769: three moves of one fleet checkout, none attributable).
//!
//! # Facts only
//!
//! One record per fast-forward the pass performs, carrying the checkout and
//! the full commit before and after, at the reflog entry's own time. The entry
//! is picked out from the lines the pass's own merge added, so a stale entry,
//! a concurrent external move or a merge git had nothing to do cannot claim
//! credit. A reflog that cannot be read emits nothing: unknown is not a fact.
//!
//! **OTLP only.** The scalars ride as `loom.checkout.*` attributes
//! ([`CHECKOUT_FAST_FORWARD_LOG_ATTRIBUTE_KEYS`], the keys
//! `checkout.external_change` already exports, which the collector's log
//! `keep_keys` lists; contract-tested). The body is the record's JSON, which
//! carries the checkout root. **Provenance is required.**

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::telemetry::provenance::Provenance;
use crate::telemetry::RepoVisibility;

/// What `loom.checkout.kind` carries on this kind: the kind of move, as it
/// carries `reset`, `checkout` or `rebase` on `checkout.external_change`.
pub const MOVE_KIND: &str = "fast-forward";

/// Every log attribute key `checkout.fast_forward` exports.
pub const CHECKOUT_FAST_FORWARD_LOG_ATTRIBUTE_KEYS: &[&str] = &[
    "loom.checkout.change_id",
    "loom.checkout.kind",
    "loom.checkout.ref",
    "loom.checkout.from_sha",
    "loom.checkout.to_sha",
    "loom.checkout.version",
    "loom.checkout.revision",
    "loom.checkout.tree_state",
    "loom.checkout.provenance_complete",
];

/// The `checkout.fast_forward` record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutFastForwardRecord {
    /// Stable id: a hash of root, from, to and the reflog time, as on
    /// `checkout.external_change`. The same reflog entry read twice is the
    /// same id.
    pub change_id: String,
    /// `owner/repo` from the checkout's `origin`, when it parses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Private until resolved: a missing tag never decodes to public.
    #[serde(default)]
    pub visibility: RepoVisibility,
    /// The registered root.
    pub root: String,
    /// The host whose checkout it is (and whose reflog was read).
    pub host: String,
    /// The branch the pass moved, `refs/heads/<default>`. The entry itself is
    /// read from the checkout's `HEAD` reflog, which carries the same line.
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// The commit before the move, in full.
    pub from_sha: String,
    /// The commit after it, in full.
    pub to_sha: String,
    /// When git wrote the entry: the record time.
    pub at: DateTime<Utc>,
    /// When this daemon read it.
    pub observed_at: DateTime<Utc>,
    /// The observing build.
    pub loom: Provenance,
}

impl CheckoutFastForwardRecord {
    /// Whether the record carries valid provenance.
    #[must_use]
    pub fn has_provenance(&self) -> bool {
        self.loom.is_valid()
    }
}
