//! Shared parsing for env/config override strings: truthy flags and positive
//! integers.
//!
//! One definition of "is this string truthy" for the flags that accept
//! `1`/`true`/`yes`/`on` (trimmed, ASCII case-insensitive); anything else,
//! including garbage, is `false`. Copies with different accepted values or
//! return shapes (bash-mirroring, default-on, tri-state) deliberately stay
//! local to their modules.
//!
//! One definition of "is this a positive integer" for the numeric overrides
//! (`u64`, `usize`, `u32`): the value is trimmed, and an unset, zero,
//! negative, or unparseable value is `None`. Parsers that deliberately accept
//! `0`, and signed or floating-point knobs, likewise stay local to their
//! modules.

/// `true` for `1`, `true`, `yes`, `on` (trimmed, case-insensitive); else `false`.
#[must_use]
pub fn parse_truthy(value: &str) -> bool {
    matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}

/// Read env var `name`: `None` when unset, otherwise [`parse_truthy`] of its value.
#[must_use]
pub fn env_flag(name: &str) -> Option<bool> {
    std::env::var(name).ok().map(|value| parse_truthy(&value))
}

/// Parse `value` (trimmed) as a positive integer; `None` when zero, negative, or unparseable.
#[must_use]
pub fn parse_positive<T>(value: &str) -> Option<T>
where
    T: std::str::FromStr + PartialOrd + Default,
{
    value.trim().parse::<T>().ok().filter(|v| *v > T::default())
}

/// Read env var `name` as a positive integer; `None` when unset, zero, negative, or unparseable.
#[must_use]
pub fn env_positive<T>(name: &str) -> Option<T>
where
    T: std::str::FromStr + PartialOrd + Default,
{
    std::env::var(name).ok().and_then(|v| parse_positive(&v))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

    #[test]
    fn truthy_values() {
        for v in [
            "1", "true", "yes", "on", "TRUE", "Yes", "ON", " on ", "\ttrue\n",
        ] {
            assert!(parse_truthy(v), "{v:?} should be truthy");
        }
    }

    #[test]
    fn falsy_values() {
        for v in ["0", "false", "no", "off", "", "  ", "maybe", "2", "onn"] {
            assert!(!parse_truthy(v), "{v:?} should be falsy");
        }
    }

    #[test]
    fn env_flag_unset_is_none() {
        assert_eq!(env_flag("LOOM_ENV_FLAG_TEST_DEFINITELY_UNSET_13376"), None);
    }

    #[test]
    fn positive_values() {
        assert_eq!(parse_positive::<u64>("1"), Some(1));
        assert_eq!(parse_positive::<u64>(" 42 "), Some(42));
        assert_eq!(parse_positive::<u64>("\t3600\n"), Some(3600));
        assert_eq!(parse_positive::<usize>("4"), Some(4));
        assert_eq!(parse_positive::<u32>("5"), Some(5));
    }

    #[test]
    fn zero_is_none() {
        for v in ["0", "00", " 0 "] {
            assert_eq!(parse_positive::<u64>(v), None, "{v:?} should not be positive");
        }
        assert_eq!(parse_positive::<i64>("-0"), None);
    }

    #[test]
    fn negative_values_are_none() {
        assert_eq!(parse_positive::<i64>("-1"), None);
        assert_eq!(parse_positive::<i64>(" -7 "), None);
        assert_eq!(parse_positive::<u64>("-1"), None);
    }

    #[test]
    fn garbage_is_none() {
        for v in [
            "notanumber",
            "1s",
            "1.5",
            "+",
            "18446744073709551616",
            "",
            "   ",
        ] {
            assert_eq!(parse_positive::<u64>(v), None, "{v:?} should not parse as positive");
        }
    }

    #[test]
    fn env_positive_unset_is_none() {
        assert_eq!(env_positive::<u64>("LOOM_ENV_POSITIVE_TEST_DEFINITELY_UNSET_15808"), None);
    }

    #[test]
    #[serial]
    fn env_positive_reads_set_values() {
        const NAME: &str = "LOOM_ENV_POSITIVE_TEST_SET_15808";
        for (raw, expected) in [("1", 1u64), (" 30 ", 30), ("3600", 3600)] {
            std::env::set_var(NAME, raw);
            assert_eq!(env_positive::<u64>(NAME), Some(expected), "{raw:?}");
        }
        for raw in ["0", "-1", "notanumber", "", "   "] {
            std::env::set_var(NAME, raw);
            assert_eq!(env_positive::<u64>(NAME), None, "{raw:?}");
        }
        std::env::set_var(NAME, "4");
        assert_eq!(env_positive::<usize>(NAME), Some(4));
        std::env::set_var(NAME, "5");
        assert_eq!(env_positive::<u32>(NAME), Some(5));
        std::env::remove_var(NAME);
    }
}
