//! Shared plumbing for the sibling dispatch brakes (#15663): the
//! **env > config > default** knob resolution behind every
//! `resolve_*_config`, and the per-issue `until`-window queries behind every
//! brake's `*_remaining` / `*_until` / `*_issues` triplet.
//!
//! Each brake module (`quarantine`, `dispatch`, `noop_cooldown`,
//! `decline_cooldown`, `prless_retry`, `outcome_journal::writeback`) used to
//! carry its own copy — an intermediate `*FileConfig` struct and a `read_*`
//! reader per brake, and four verbatim window-query bodies. Nothing here
//! changes a precedence rule: a set env var still wins outright (a zero,
//! non-numeric or not-Unicode value falls through), the config tier is still
//! the effective config's block for that brake, and every clamping rule stays
//! with the brake that owns it.

use super::*;

/// A boolean brake knob resolved **env > config > default** (Issue #3939's
/// enable toggle, and every brake's since): a set `env_key` decides outright
/// — `1`/`true`/`yes`/`on`, trimmed and ASCII-case-insensitive, is `true` and
/// any other value `false` — else `block`'s `json_key` when it is a JSON
/// boolean, else `default`.
pub(super) fn env_or_config_bool(
    block: Option<&serde_json::Value>,
    env_key: &str,
    json_key: &str,
    default: bool,
) -> bool {
    env_bool_override(env_key)
        .or_else(|| {
            block
                .and_then(|b| b.get(json_key))
                .and_then(serde_json::Value::as_bool)
        })
        .unwrap_or(default)
}

/// The env tier of a boolean brake knob ([`env_or_config_bool`]): `Some` when
/// `env_key` is set, `None` when it is unset (or not Unicode) and a lower tier
/// must decide. Read on its own by a resolver that can then answer without
/// touching the config tiers at all.
pub(super) fn env_bool_override(env_key: &str) -> Option<bool> {
    std::env::var(env_key)
        .ok()
        .map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"))
}

/// A strictly-positive `u64` brake knob (a duration in seconds) resolved
/// **env > config > default**: a set `env_key` that parses and is `> 0`, else
/// `block`'s `json_key` when it is a JSON integer `> 0`, else `default`. Zero,
/// non-numeric and not-Unicode values fall through to the next tier.
pub(super) fn env_or_config_positive_u64(
    block: Option<&serde_json::Value>,
    env_key: &str,
    json_key: &str,
    default: u64,
) -> u64 {
    env_u64(env_key)
        .filter(|&n| n > 0)
        .or_else(|| config_u64(block, json_key).filter(|&n| n > 0))
        .unwrap_or(default)
}

/// [`env_or_config_positive_u64`] for a `u32` knob (a threshold, an attempt
/// ceiling): a value that does not fit `u32` falls through like a zero.
pub(super) fn env_or_config_positive_u32(
    block: Option<&serde_json::Value>,
    env_key: &str,
    json_key: &str,
    default: u32,
) -> u32 {
    env_u32(env_key)
        .filter(|&n| n > 0)
        .or_else(|| config_u32(block, json_key).filter(|&n| n > 0))
        .unwrap_or(default)
}

/// A `u32` brake knob resolved **env > config > default** that keeps `0`
/// (`noopCooldown.holdThreshold`, where zero disables the hold, #10156).
pub(super) fn env_or_config_u32(
    block: Option<&serde_json::Value>,
    env_key: &str,
    json_key: &str,
    default: u32,
) -> u32 {
    env_u32(env_key)
        .or_else(|| config_u32(block, json_key))
        .unwrap_or(default)
}

fn env_u64(env_key: &str) -> Option<u64> {
    std::env::var(env_key)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
}

fn env_u32(env_key: &str) -> Option<u32> {
    env_u64(env_key).and_then(|n| u32::try_from(n).ok())
}

fn config_u64(block: Option<&serde_json::Value>, json_key: &str) -> Option<u64> {
    block
        .and_then(|b| b.get(json_key))
        .and_then(serde_json::Value::as_u64)
}

fn config_u32(block: Option<&serde_json::Value>, json_key: &str) -> Option<u32> {
    config_u64(block, json_key).and_then(|n| u32::try_from(n).ok())
}

/// How much longer a per-issue brake window has to run at `now`, or `None`
/// when the brake is disabled, the issue has no window, or the window has
/// elapsed. `Some(Duration::ZERO)` is never returned — an elapsed window
/// reads as `None` (Issue #9311's `*_remaining` contract).
pub(super) fn cooldown_window_remaining(
    enabled: bool,
    until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<Duration> {
    if !enabled {
        return None;
    }
    let remaining = until? - now;
    if remaining <= chrono::Duration::zero() {
        return None;
    }
    remaining.to_std().ok().filter(|d| !d.is_zero())
}

/// The instant a per-issue brake window ends, or `None` on exactly the
/// [`cooldown_window_remaining`] conditions.
pub(super) fn cooldown_window_until(
    enabled: bool,
    until: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    if !enabled {
        return None;
    }
    until.filter(|u| *u > now)
}

/// Every issue in `map` that `predicate` admits — the brake's own live-window
/// test, plus whatever else it keys on — or an empty set when the brake is
/// disabled.
pub(super) fn active_cooldown_issues<V>(
    enabled: bool,
    map: &HashMap<u32, V>,
    predicate: impl Fn(&V) -> bool,
) -> HashSet<u32> {
    if !enabled {
        return HashSet::new();
    }
    map.iter()
        .filter(|(_, s)| predicate(s))
        .map(|(issue, _)| *issue)
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::env_test_support::EnvGuard;
    use serial_test::serial;

    const ENV: &str = "LOOM_TEST_BRAKE_KNOB";

    fn block(json: &str) -> serde_json::Value {
        serde_json::from_str(json).unwrap()
    }

    fn at(secs: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000 + secs, 0).unwrap()
    }

    /// A boolean knob: a set env var decides outright, else the block's JSON
    /// boolean, else the default.
    #[test]
    #[serial]
    fn bool_knob_resolves_env_then_config_then_default() {
        let on = block(r#"{"enabled":true}"#);
        let off = block(r#"{"enabled":false}"#);

        let _env = EnvGuard::set(ENV, " ON ");
        assert!(env_or_config_bool(Some(&off), ENV, "enabled", false));
        let _env = EnvGuard::set(ENV, "disabled");
        assert!(!env_or_config_bool(Some(&on), ENV, "enabled", true));
        assert_eq!(env_bool_override(ENV), Some(false));

        let _env = EnvGuard::clear(&[ENV]);
        assert_eq!(env_bool_override(ENV), None);
        assert!(env_or_config_bool(Some(&on), ENV, "enabled", false));
        assert!(!env_or_config_bool(Some(&off), ENV, "enabled", true));
        assert!(
            env_or_config_bool(Some(&off), ENV, "missing", true),
            "another key in the block is not this knob: the default decides"
        );
        assert!(!env_or_config_bool(Some(&block(r#"{"enabled":"yes"}"#)), ENV, "enabled", false));
        assert!(env_or_config_bool(None, ENV, "enabled", true));
    }

    /// A positive integer knob: zero, non-numeric and out-of-range env values
    /// fall through, as does a zero or out-of-range JSON number.
    #[test]
    #[serial]
    fn positive_knobs_drop_zero_and_out_of_range_values() {
        let b = block(r#"{"secs":900,"count":5,"zero":0,"huge":18446744073709551615}"#);

        let _env = EnvGuard::set(ENV, " 42 ");
        assert_eq!(env_or_config_positive_u64(Some(&b), ENV, "secs", 1), 42);
        assert_eq!(env_or_config_positive_u32(Some(&b), ENV, "secs", 1), 42);

        let _env = EnvGuard::set(ENV, "0");
        assert_eq!(env_or_config_positive_u64(Some(&b), ENV, "secs", 1), 900);
        assert_eq!(env_or_config_positive_u32(Some(&b), ENV, "count", 1), 5);

        let _env = EnvGuard::set(ENV, "5000000000");
        assert_eq!(
            env_or_config_positive_u32(Some(&b), ENV, "count", 1),
            5,
            "an env value over u32 falls through"
        );

        let _env = EnvGuard::set(ENV, "soon");
        assert_eq!(env_or_config_positive_u64(Some(&b), ENV, "secs", 1), 900);

        let _env = EnvGuard::clear(&[ENV]);
        assert_eq!(
            env_or_config_positive_u32(Some(&b), ENV, "huge", 1),
            1,
            "a JSON number over u32 falls through"
        );
        assert_eq!(env_or_config_positive_u32(Some(&b), ENV, "zero", 1), 1);
        assert_eq!(env_or_config_positive_u32(Some(&block(r#"{"count":-1}"#)), ENV, "count", 1), 1);
        assert_eq!(env_or_config_positive_u32(None, ENV, "count", 1), 1);
        assert_eq!(env_or_config_positive_u64(Some(&b), ENV, "huge", 1), u64::MAX);
    }

    /// A plain `u32` knob keeps zero — `noopCooldown.holdThreshold`'s "0
    /// disables the hold" — where the positive knobs drop it.
    #[test]
    #[serial]
    fn u32_knob_keeps_zero() {
        let _env = EnvGuard::set(ENV, "0");
        assert_eq!(env_or_config_u32(Some(&block(r#"{"n":5}"#)), ENV, "n", 3), 0);
        assert_eq!(env_or_config_positive_u32(Some(&block(r#"{"n":5}"#)), ENV, "n", 3), 5);

        let _env = EnvGuard::clear(&[ENV]);
        assert_eq!(env_or_config_u32(Some(&block(r#"{"n":0}"#)), ENV, "n", 3), 0);
        assert_eq!(env_or_config_u32(None, ENV, "n", 3), 3);
    }

    /// The window ends at `until`: strictly before it there is time left and
    /// an instant to report; at and after it there is neither, and a disabled
    /// brake or an issue without a window never has one.
    #[test]
    fn window_queries_agree_on_where_the_window_ends() {
        assert_eq!(
            cooldown_window_remaining(true, Some(at(60)), at(0)),
            Some(Duration::from_secs(60))
        );
        assert_eq!(cooldown_window_until(true, Some(at(60)), at(59)), Some(at(60)));

        for now in [60, 61] {
            assert_eq!(cooldown_window_remaining(true, Some(at(60)), at(now)), None);
            assert_eq!(cooldown_window_until(true, Some(at(60)), at(now)), None);
        }
        for enabled in [true, false] {
            assert_eq!(cooldown_window_remaining(enabled, None, at(0)), None);
            assert_eq!(cooldown_window_until(enabled, None, at(0)), None);
        }
        assert_eq!(cooldown_window_remaining(false, Some(at(60)), at(0)), None);
        assert_eq!(cooldown_window_until(false, Some(at(60)), at(0)), None);
    }

    /// The issue set is every entry the caller's predicate admits — the
    /// live-window filter, plus whatever else the brake keys on (the open-PR
    /// guard's cause, #7606) — and empty when the brake is disabled.
    #[test]
    fn active_issues_are_the_entries_the_predicate_admits() {
        let map: HashMap<u32, (DateTime<Utc>, bool)> = [
            (1, (at(60), false)),
            (2, (at(60), true)),
            (3, (at(-1), true)),
        ]
        .into_iter()
        .collect();

        let live = active_cooldown_issues(true, &map, |&(until, _)| until > at(0));
        assert_eq!(live, HashSet::from([1, 2]));
        let open_pr = active_cooldown_issues(true, &map, |&(until, cause)| until > at(0) && cause);
        assert_eq!(open_pr, HashSet::from([2]));
        let disabled = active_cooldown_issues(false, &map, |&(_, cause)| cause);
        assert!(disabled.is_empty());
    }
}
