//! Issue #15372: the activity database's default location resolves the same
//! way through **both** public import paths.
//!
//! `default_activity_db_path` used to exist as three per-caller copies
//! (`limit_calibration`, `usage_report_cli`, `stats`). It now lives once, in
//! `activity::weekly_point_history`, with `activity` re-exporting it and
//! `limit_calibration` re-exporting it so its historical import path stays
//! publicly callable. This is the downstream-crate proof: both names answer
//! the exact `LOOM_ACTIVITY_DB` override, and without it both fall back to
//! `.loom/activity.db` under the discovered home (the no-home `.` arm is not
//! exercised here — every CI host resolves an account home — but it is the
//! verbatim branch the pre-#15372 implementations already carried).
//!
//! Single test function on purpose: it mutates `LOOM_ACTIVITY_DB`, and one
//! function is serialized by construction under both `cargo test` (one
//! mutating fn per binary) and `cargo nextest` (process per test).

use std::path::PathBuf;

use loom_daemon::activity::default_activity_db_path as activity_path;
use loom_daemon::limit_calibration::default_activity_db_path as calibration_path;

#[test]
fn both_public_import_paths_return_the_exact_override_and_default() {
    let dir = tempfile::tempdir().unwrap();
    let want = dir.path().join("exact-activity.db");
    std::env::set_var("LOOM_ACTIVITY_DB", &want);
    assert_eq!(activity_path(), want);
    assert_eq!(calibration_path(), want);
    std::env::remove_var("LOOM_ACTIVITY_DB");

    let default = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".loom")
        .join("activity.db");
    assert_eq!(activity_path(), default);
    assert_eq!(calibration_path(), default);
}
