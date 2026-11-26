//! Dispatch and first-activity telemetry for a Claude seat step (#14371 AC7).
//!
//! The supervisor writes two `# LOOM_SEAT_*` marker lines to its stderr, beside
//! the `# LOOM_DISPATCH_MODE` launch record, so a run log carries what is
//! needed to recompute start time inside a seat against a host run:
//!
//! ```text
//! # LOOM_SEAT_DISPATCH dispatched_at=<RFC 3339 UTC, ms>
//! # LOOM_SEAT_FIRST_ACTIVITY dispatched_at=<..> first_activity_at=<..> elapsed_ms=<n>
//! ```
//!
//! *Dispatch* is the instant before `docker exec` is spawned (after preflight).
//! *First activity* is the first byte the worker writes to **stderr**: stdout
//! is inherited, so the supervisor cannot see it, and `spawn-claude.sh` logs
//! to stderr as its first act. A step that exits without writing any stderr
//! reports `first_activity_at=none` rather than omitting the record, so a
//! missing signal is distinguishable from a missing marker.
use std::io::Read;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Instant;

use chrono::{DateTime, SecondsFormat, Utc};

fn stamp(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The dispatch marker line (no trailing newline).
pub(super) fn dispatch_line(dispatched_at: DateTime<Utc>) -> String {
    format!("# LOOM_SEAT_DISPATCH dispatched_at={}", stamp(dispatched_at))
}

/// The first-activity marker line; `first` is `None` when the worker never
/// wrote to stderr.
pub(super) fn first_activity_line(
    dispatched_at: DateTime<Utc>,
    first: Option<(DateTime<Utc>, u128)>,
) -> String {
    match first {
        Some((at, elapsed_ms)) => format!(
            "# LOOM_SEAT_FIRST_ACTIVITY dispatched_at={} first_activity_at={} elapsed_ms={elapsed_ms}",
            stamp(dispatched_at),
            stamp(at)
        ),
        None => format!(
            "# LOOM_SEAT_FIRST_ACTIVITY dispatched_at={} first_activity_at=none",
            stamp(dispatched_at)
        ),
    }
}

/// Tracks one dispatch. Cheap to clone into the stderr reader thread.
#[derive(Clone)]
pub(super) struct SeatTiming {
    dispatched_at: DateTime<Utc>,
    started: Instant,
    seen: Arc<AtomicBool>,
}

impl SeatTiming {
    /// Record dispatch now and write the dispatch marker to stderr.
    pub(super) fn dispatch() -> Self {
        let timing = Self {
            dispatched_at: Utc::now(),
            started: Instant::now(),
            seen: Arc::new(AtomicBool::new(false)),
        };
        eprintln!("{}", dispatch_line(timing.dispatched_at));
        timing
    }

    /// Wrap the worker's stderr so the first byte emits the marker.
    pub(super) fn watch<R: Read>(&self, inner: R) -> FirstActivity<R> {
        FirstActivity {
            inner,
            timing: self.clone(),
        }
    }

    /// At exit: if the worker never spoke, say so.
    pub(super) fn finish(&self) {
        if !self.seen.load(Ordering::Acquire) {
            eprintln!("{}", first_activity_line(self.dispatched_at, None));
        }
    }
}

/// A [`Read`] adapter that announces the first non-empty read.
pub(super) struct FirstActivity<R> {
    inner: R,
    timing: SeatTiming,
}

impl<R: Read> Read for FirstActivity<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buf)?;
        if count > 0 && !self.timing.seen.swap(true, Ordering::AcqRel) {
            let elapsed = self.timing.started.elapsed().as_millis();
            eprintln!(
                "{}",
                first_activity_line(self.timing.dispatched_at, Some((Utc::now(), elapsed)))
            );
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use std::io::Cursor;

    fn at(ms: i64) -> DateTime<Utc> {
        Utc.timestamp_millis_opt(1_700_000_000_000 + ms).unwrap()
    }

    #[test]
    fn dispatch_marker_is_rfc3339_millis_utc() {
        assert_eq!(
            dispatch_line(at(0)),
            "# LOOM_SEAT_DISPATCH dispatched_at=2023-11-14T22:13:20.000Z"
        );
    }

    #[test]
    fn first_activity_marker_names_both_instants_and_the_gap() {
        assert_eq!(
            first_activity_line(at(0), Some((at(1_250), 1_250))),
            "# LOOM_SEAT_FIRST_ACTIVITY dispatched_at=2023-11-14T22:13:20.000Z \
             first_activity_at=2023-11-14T22:13:21.250Z elapsed_ms=1250"
        );
    }

    #[test]
    fn a_silent_worker_reports_none_rather_than_no_record() {
        let line = first_activity_line(at(0), None);
        assert!(line.ends_with("first_activity_at=none"), "{line}");
    }

    #[test]
    fn only_the_first_nonempty_read_counts_as_activity() {
        let timing = SeatTiming {
            dispatched_at: at(0),
            started: Instant::now(),
            seen: Arc::new(AtomicBool::new(false)),
        };
        let mut reader = timing.watch(Cursor::new(b"hello".to_vec()));
        let mut buf = [0u8; 2];
        // An empty read (EOF on an empty source) is not activity.
        assert_eq!(
            timing
                .watch(Cursor::new(Vec::new()))
                .read(&mut buf)
                .unwrap(),
            0
        );
        assert!(!timing.seen.load(Ordering::Acquire));
        assert_eq!(reader.read(&mut buf).unwrap(), 2);
        assert!(timing.seen.load(Ordering::Acquire));
    }
}
