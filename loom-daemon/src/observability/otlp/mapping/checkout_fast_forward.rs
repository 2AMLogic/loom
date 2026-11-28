//! OTLP mapping for `checkout.fast_forward` (#15729): one INFO log
//! record per fast-forward the checkout pass performed, stamped at the reflog
//! entry's own time (when git moved the checkout), not when the daemon read
//! it. The body is the record's JSON (it carries the checkout root); the
//! scalars ride as the `loom.checkout.*` attributes `checkout.external_change`
//! already exports, with `loom.repo` / `loom.repo.visibility` when the repo is
//! known.

use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::logs::v1::SeverityNumber;

use super::{kv_bool, kv_string, nanos};
use crate::telemetry::kinds::checkout_fast_forward::MOVE_KIND;
use crate::telemetry::TelemetryRecord;

/// `(event_name, severity, record time, attributes, body)`; `None` for every
/// other kind.
pub(super) fn log_parts(
    record: &TelemetryRecord,
) -> Option<(&'static str, SeverityNumber, u64, Vec<KeyValue>, String)> {
    let TelemetryRecord::CheckoutFastForward(r) = record else {
        return None;
    };
    let mut attributes = vec![
        kv_string("loom.checkout.change_id", r.change_id.clone()),
        kv_string("loom.checkout.kind", MOVE_KIND),
        kv_string("loom.checkout.ref", r.ref_name.clone()),
        kv_string("loom.checkout.from_sha", r.from_sha.clone()),
        kv_string("loom.checkout.to_sha", r.to_sha.clone()),
        kv_string("loom.checkout.version", r.loom.version.clone()),
        kv_string("loom.checkout.revision", r.loom.revision.clone()),
        kv_string("loom.checkout.tree_state", r.loom.tree_state.clone()),
        kv_bool("loom.checkout.provenance_complete", r.loom.complete),
    ];
    if let Some(repo) = &r.repo {
        attributes.push(kv_string("loom.repo", repo.clone()));
    }
    attributes.push(kv_string("loom.repo.visibility", r.visibility.as_str()));
    let body = serde_json::to_string(r).unwrap_or_default();
    Some(("checkout.fast_forward", SeverityNumber::Info, nanos(r.at), attributes, body))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use chrono::{TimeZone, Utc};
    use opentelemetry_proto::tonic::common::v1::any_value;
    use opentelemetry_proto::tonic::logs::v1::LogRecord;

    use super::super::log_record_for;
    use super::*;
    use crate::telemetry::kinds::checkout_fast_forward::{
        CheckoutFastForwardRecord, CHECKOUT_FAST_FORWARD_LOG_ATTRIBUTE_KEYS, MOVE_KIND,
    };
    use crate::telemetry::provenance::Provenance;
    use crate::telemetry::{RepoVisibility, TelemetryEnvelope};

    /// The entry was written at 00:44:00 and read at 00:49:00; the envelope
    /// is stamped later still.
    fn record() -> CheckoutFastForwardRecord {
        let written = Utc.with_ymd_and_hms(2026, 11, 28, 3, 30, 50).unwrap();
        CheckoutFastForwardRecord {
            change_id: "0123456789abcdef".into(),
            repo: Some("example/repo".into()),
            visibility: RepoVisibility::Private,
            root: "/work/repo".into(),
            host: "host-1".into(),
            ref_name: "refs/heads/main".into(),
            from_sha: "a".repeat(40),
            to_sha: "b".repeat(40),
            at: written,
            observed_at: written + chrono::Duration::minutes(5),
            loom: Provenance {
                version: "0.19.1".into(),
                revision: "9d8e226ce0123456789abcdef0123456789abcde".into(),
                tree_state: "clean".into(),
                complete: true,
            },
        }
    }

    fn value(log: &LogRecord, key: &str) -> String {
        let kv = log
            .attributes
            .iter()
            .find(|kv| kv.key == key)
            .unwrap_or_else(|| panic!("{key} is emitted"));
        match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
            Some(any_value::Value::StringValue(s)) => s.clone(),
            other => panic!("{key} must be a string, got {other:?}"),
        }
    }

    #[test]
    fn every_attribute_is_declared_and_stamped_at_the_reflog_time() {
        let r = record();
        let envelope =
            TelemetryEnvelope::new("host-1", TelemetryRecord::CheckoutFastForward(r.clone()));
        let log = log_record_for(&envelope).unwrap();
        assert_eq!(log.event_name, "checkout.fast_forward");
        assert_eq!(log.severity_number, SeverityNumber::Info as i32);
        assert_eq!(log.time_unix_nano, nanos(r.at), "the entry's time, not the read's");
        assert!(
            log.time_unix_nano < nanos(r.observed_at)
                && nanos(r.observed_at) <= log.observed_time_unix_nano,
            "a later read is the observed time, never the event time"
        );
        assert_eq!(value(&log, "loom.kind"), "checkout.fast_forward");
        assert_eq!(value(&log, "loom.checkout.change_id"), r.change_id);
        assert_eq!(value(&log, "loom.checkout.kind"), MOVE_KIND);
        assert_eq!(value(&log, "loom.checkout.ref"), "refs/heads/main");
        assert_eq!(value(&log, "loom.checkout.from_sha"), "a".repeat(40));
        assert_eq!(value(&log, "loom.checkout.to_sha"), "b".repeat(40));
        assert_eq!(value(&log, "loom.repo"), "example/repo");
        assert_eq!(value(&log, "loom.repo.visibility"), "private");
        let body = log.body.unwrap();
        assert!(
            matches!(body.value, Some(any_value::Value::StringValue(ref s)) if s.contains("\"root\":\"/work/repo\"")),
            "the body is the record's JSON: {body:?}"
        );
        let keys: Vec<&str> = log.attributes.iter().map(|kv| kv.key.as_str()).collect();
        for key in CHECKOUT_FAST_FORWARD_LOG_ATTRIBUTE_KEYS {
            assert!(keys.contains(key), "{key} is emitted");
        }
        for key in keys {
            assert!(
                CHECKOUT_FAST_FORWARD_LOG_ATTRIBUTE_KEYS.contains(&key)
                    || [
                        "loom.record_id",
                        "loom.kind",
                        "loom.repo",
                        "loom.repo.visibility"
                    ]
                    .contains(&key),
                "{key} is not allowlisted"
            );
        }
    }

    #[test]
    fn an_unparseable_origin_exports_no_repo() {
        let mut r = record();
        r.repo = None;
        let envelope = TelemetryEnvelope::new("host-1", TelemetryRecord::CheckoutFastForward(r));
        let log = log_record_for(&envelope).unwrap();
        assert!(!log.attributes.iter().any(|kv| kv.key == "loom.repo"));
        assert_eq!(value(&log, "loom.repo.visibility"), "private");
    }

    #[test]
    fn the_collector_keeps_every_checkout_attribute() {
        const CONFIG: &str =
            include_str!("../../../../../defaults/observability/collector/config.yaml");
        let keep = CONFIG
            .lines()
            .find(|l| l.contains("keep_keys(attributes, [") && l.contains("loom.ci.chunk_index"))
            .unwrap();
        for key in CHECKOUT_FAST_FORWARD_LOG_ATTRIBUTE_KEYS {
            assert!(keep.contains(&format!("\"{key}\"")), "collector drops {key}");
        }
    }
}
