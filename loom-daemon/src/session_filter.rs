//! The session-attribution filter shared by the Codex rollout reader
//! ([`crate::codex_usage`]) and the OpenCode SQLite reader
//! ([`crate::opencode_usage`]).
//!
//! Both readers attribute a stored session to a caller by the same rule, so
//! the rule lives here once; each reader re-exports [`SessionFilter`] under
//! its own module path. What stays in each reader is the time-window policy
//! (Codex applies a supplied window to an id match; OpenCode skips it for an
//! id-keyed lookup) — that decision is made after [`SessionFilter::matches`].

use std::path::PathBuf;

/// Which sessions a caller wants attributed: an exact id set when the caller
/// has one (#14048), else the directory set every consumer used before it.
///
/// An entirely-empty filter (no directories, no ids) matches NOTHING: a
/// caller with no key at all must not silently attribute every session on the
/// host, which would fold an unrelated sweep's spend into a completion.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionFilter {
    /// Working directories to attribute, matched exactly against the stored
    /// session's working directory. Empty ⇒ no directory constraint (only
    /// meaningful together with [`Self::ids`], which is then the whole key).
    pub directories: Vec<PathBuf>,
    /// Exact stored session ids to attribute. Empty ⇒ no id constraint.
    pub ids: Vec<String>,
}

impl SessionFilter {
    /// The directory+window key every consumer in the daemon used before
    /// #14048, unchanged.
    #[must_use]
    pub fn directories(directories: &[PathBuf]) -> Self {
        Self {
            directories: directories.to_vec(),
            ids: Vec::new(),
        }
    }

    /// The exact-id key (#14048): the session ids a launch's own log names.
    #[must_use]
    pub fn ids(ids: &[String]) -> Self {
        Self {
            directories: Vec::new(),
            ids: ids.to_vec(),
        }
    }

    /// Whether a stored session's `directory` / `session_id` pass this filter:
    /// both constraints apply when both are set, and an empty filter matches
    /// nothing.
    #[must_use]
    pub(crate) fn matches(&self, directory: &str, session_id: Option<&str>) -> bool {
        if self.directories.is_empty() && self.ids.is_empty() {
            return false;
        }
        if !self.directories.is_empty()
            && !self
                .directories
                .iter()
                .any(|d| d.to_str().is_some_and(|d| d == directory))
        {
            return false;
        }
        if !self.ids.is_empty()
            && !session_id.is_some_and(|id| self.ids.iter().any(|want| want == id))
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(d: &[&str]) -> Vec<PathBuf> {
        d.iter().map(PathBuf::from).collect()
    }

    fn ids(i: &[&str]) -> Vec<String> {
        i.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn empty_filter_matches_nothing() {
        let f = SessionFilter::default();
        assert!(!f.matches("/w", Some("a")));
        assert!(!f.matches("/w", None));
    }

    #[test]
    fn directories_only_requires_exact_directory() {
        let f = SessionFilter::directories(&dirs(&["/w", "/x"]));
        assert!(f.matches("/x", None));
        assert!(f.matches("/w", Some("any")));
        assert!(!f.matches("/w/sub", Some("any")));
    }

    #[test]
    fn ids_only_requires_a_present_matching_id() {
        let f = SessionFilter::ids(&ids(&["a", "b"]));
        assert!(f.matches("/anywhere", Some("b")));
        assert!(!f.matches("/anywhere", Some("c")));
        assert!(!f.matches("/anywhere", None));
    }

    #[test]
    fn both_lists_must_both_match() {
        let f = SessionFilter {
            directories: dirs(&["/w"]),
            ids: ids(&["a"]),
        };
        assert!(f.matches("/w", Some("a")));
        assert!(!f.matches("/other", Some("a")));
        assert!(!f.matches("/w", Some("b")));
        assert!(!f.matches("/w", None));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_directory_never_matches() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let bad = PathBuf::from(OsString::from_vec(vec![b'/', 0xff]));
        let f = SessionFilter::directories(&[bad]);
        assert!(!f.matches("/\u{fffd}", None));
    }
}
