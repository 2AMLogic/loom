//! Forge CLI output parser for detecting forge events from terminal output.
//!
//! This module parses terminal output to detect when agents interact with a forge
//! (GitHub, Gitea, etc.) through CLI tools, capturing:
//! - Issue creation/closing
//! - PR creation/merging/closing
//! - Label changes
//! - Review submissions
//!
//! The URL matching is parameterized by forge host, allowing the same parser to
//! work with GitHub (`github.com`), Gitea, and other forge instances.
//!
//! # Example
//!
//! ```ignore
//! use forge_parser::parse_forge_events;
//!
//! let output = "Creating pull request for feature/issue-42 into main...\nhttps://github.com/owner/repo/pull/123";
//! let events = parse_forge_events(output, "github.com");
//! // Returns: [ParsedForgeEvent { event_type: PrCreated, pr_number: Some(123), ... }]
//! ```

use crate::activity::{PromptForgeEvent, PromptForgeEventType};
use regex::Regex;
use std::sync::LazyLock;

/// Compile a compile-time-constant pattern. A bad pattern is a programming error
/// and fails loudly (the unit tests exercise every static) instead of silently
/// dropping events.
fn static_regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("static forge_parser regex must compile")
}

// Pattern: PR merged
// Matches: "Merged pull request #123" or "Pull request #123 has been merged"
// Also matches: "merged" in gh pr merge output
static MERGE_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?i)(?:merged|merging)\s+(?:pull\s+request\s+)?#?(\d+)"));

// Pattern: Issue closed
// Matches: "Closed issue #123" or "Issue #123 closed"
static CLOSE_ISSUE_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?i)(?:closed?\s+issue\s+#?(\d+)|issue\s+#?(\d+)\s+closed)"));

// Pattern: PR closed (without merge)
// Matches: "Closed pull request #123"
static CLOSE_PR_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?i)closed?\s+pull\s+request\s+#?(\d+)"));

// Pattern: Label added via gh issue/pr edit --add-label
// Output typically shows: "Updated issue #123" or similar after label changes
// We also look for the command pattern in case it's echoed
// Matches: --add-label value OR --add-label "value" OR --add-label 'value'
static LABEL_ADD_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r#"--add-label[=\s]+["']?([^"'\s,]+(?:,[^"'\s,]+)*)["']?"#));

// Pattern: Label removed via gh issue/pr edit --remove-label
static LABEL_REMOVE_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r#"--remove-label[=\s]+["']?([^"'\s,]+(?:,[^"'\s,]+)*)["']?"#));

// Pattern: Review submitted
// gh pr review output: "Approved pull request #123"
static REVIEW_RE: LazyLock<Regex> = LazyLock::new(|| {
    static_regex(r"(?i)(approved|requested\s+changes\s+(?:on|for))\s+(?:pull\s+request\s+)?#?(\d+)")
});

// Context patterns used to find the issue/PR a label edit applies to.
static CONTEXT_ISSUE_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?i)gh\s+issue\s+(?:edit\s+)?#?(\d+)"));
static CONTEXT_PR_RE: LazyLock<Regex> =
    LazyLock::new(|| static_regex(r"(?i)gh\s+pr\s+(?:edit\s+)?#?(\d+)"));

/// A parsed forge event from terminal output
#[derive(Debug, Clone)]
pub struct ParsedForgeEvent {
    pub event_type: PromptForgeEventType,
    pub issue_number: Option<i32>,
    pub pr_number: Option<i32>,
    pub labels_added: Vec<String>,
    pub labels_removed: Vec<String>,
}

impl ParsedForgeEvent {
    /// A label-free event for the given issue and/or PR.
    fn new(
        event_type: PromptForgeEventType,
        issue_number: Option<i32>,
        pr_number: Option<i32>,
    ) -> Self {
        Self {
            event_type,
            issue_number,
            pr_number,
            labels_added: Vec::new(),
            labels_removed: Vec::new(),
        }
    }

    /// Convert to a `PromptForgeEvent` for database storage
    pub fn to_prompt_forge_event(&self, input_id: Option<i64>) -> PromptForgeEvent {
        let (label_before, label_after) =
            if !self.labels_added.is_empty() || !self.labels_removed.is_empty() {
                // For label changes, we track removed as "before" and added as "after"
                (
                    if self.labels_removed.is_empty() {
                        None
                    } else {
                        Some(self.labels_removed.clone())
                    },
                    if self.labels_added.is_empty() {
                        None
                    } else {
                        Some(self.labels_added.clone())
                    },
                )
            } else {
                (None, None)
            };

        PromptForgeEvent {
            id: None,
            input_id,
            issue_number: self.issue_number,
            pr_number: self.pr_number,
            label_before,
            label_after,
            event_type: self.event_type.clone(),
        }
    }
}

/// Whether a numbered match refers to an issue or a pull request.
#[derive(Clone, Copy)]
enum Target {
    Issue,
    Pr,
}

/// Push one event per `re` match, numbered by the last participating capture group.
///
/// That is group 1 for `MERGE_RE`/`CLOSE_PR_RE`, whichever alternative matched for
/// `CLOSE_ISSUE_RE`, and the trailing number (after the verb) for `REVIEW_RE`.
fn push_numbered_events(
    events: &mut Vec<ParsedForgeEvent>,
    output: &str,
    re: &Regex,
    event_type: &PromptForgeEventType,
    target: Target,
) {
    for cap in re.captures_iter(output) {
        let Some(number) = cap
            .iter()
            .skip(1)
            .flatten()
            .last()
            .and_then(|m| m.as_str().parse::<i32>().ok())
        else {
            continue;
        };
        let (issue_number, pr_number) = match target {
            Target::Issue => (Some(number), None),
            Target::Pr => (None, Some(number)),
        };
        events.push(ParsedForgeEvent::new(event_type.clone(), issue_number, pr_number));
    }
}

/// Split a comma-separated label list, trimming and dropping empty entries.
fn parse_label_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// Push one label event per `--add-label`/`--remove-label` match of `re`.
fn push_label_events(events: &mut Vec<ParsedForgeEvent>, output: &str, re: &Regex, added: bool) {
    for cap in re.captures_iter(output) {
        let Some(labels_match) = cap.get(1) else {
            continue;
        };
        let labels = parse_label_list(labels_match.as_str());
        if labels.is_empty() {
            continue;
        }

        // Try to find associated issue/PR number
        let (issue_number, pr_number) = extract_issue_pr_from_context(output);
        let event_type = if added {
            PromptForgeEventType::LabelAdded
        } else {
            PromptForgeEventType::LabelRemoved
        };
        let mut event = ParsedForgeEvent::new(event_type, issue_number, pr_number);
        if added {
            event.labels_added = labels;
        } else {
            event.labels_removed = labels;
        }
        events.push(event);
    }
}

/// Parse terminal output for forge CLI events
///
/// Detects patterns from CLI output such as:
/// - `https://{forge_host}/owner/repo/issues/123` (issue created)
/// - `https://{forge_host}/owner/repo/pull/456` (PR created, GitHub)
/// - `https://{forge_host}/owner/repo/pulls/456` (PR created, Gitea)
/// - `Merged pull request #789`
/// - `Closed issue #123`
/// - Label change patterns
///
/// The `forge_host` parameter specifies which hostname to match in URLs
/// (e.g., `"github.com"`, `"gitea.example.com"`).
pub fn parse_forge_events(output: &str, forge_host: &str) -> Vec<ParsedForgeEvent> {
    let mut events = Vec::new();

    // Pattern: Issue/PR URL from forge CLI create commands
    // Matches: https://{forge_host}/owner/repo/issues/123
    // Matches: https://{forge_host}/owner/repo/pull/456  (GitHub)
    // Matches: https://{forge_host}/owner/repo/pulls/456 (Gitea)
    // The host is escaped, so this only fails on a pathological host length.
    let escaped_host = regex::escape(forge_host);
    let url_pattern = format!(r"https://{escaped_host}/[^/]+/[^/]+/(issues|pulls?)/(\d+)");
    if let Ok(url_re) = Regex::new(&url_pattern) {
        for cap in url_re.captures_iter(output) {
            let (Some(type_match), Some(number_match)) = (cap.get(1), cap.get(2)) else {
                continue;
            };
            let Ok(number) = number_match.as_str().parse::<i32>() else {
                continue;
            };
            events.push(if type_match.as_str() == "issues" {
                ParsedForgeEvent::new(PromptForgeEventType::IssueCreated, Some(number), None)
            } else {
                ParsedForgeEvent::new(PromptForgeEventType::PrCreated, None, Some(number))
            });
        }
    }

    push_numbered_events(
        &mut events,
        output,
        &MERGE_RE,
        &PromptForgeEventType::PrMerged,
        Target::Pr,
    );
    push_numbered_events(
        &mut events,
        output,
        &CLOSE_ISSUE_RE,
        &PromptForgeEventType::IssueClosed,
        Target::Issue,
    );
    push_numbered_events(
        &mut events,
        output,
        &CLOSE_PR_RE,
        &PromptForgeEventType::PrClosed,
        Target::Pr,
    );
    push_label_events(&mut events, output, &LABEL_ADD_RE, true);
    push_label_events(&mut events, output, &LABEL_REMOVE_RE, false);
    push_numbered_events(
        &mut events,
        output,
        &REVIEW_RE,
        &PromptForgeEventType::ReviewSubmitted,
        Target::Pr,
    );

    events
}

/// Extract issue or PR number from command context
fn extract_issue_pr_from_context(output: &str) -> (Option<i32>, Option<i32>) {
    let first_number = |re: &Regex| {
        re.captures(output)
            .and_then(|cap| cap.get(1))
            .and_then(|m| m.as_str().parse::<i32>().ok())
    };

    // Try to find "issue edit N" or "issue N" patterns
    if let Some(n) = first_number(&CONTEXT_ISSUE_RE) {
        return (Some(n), None);
    }
    // Try to find "pr edit N" patterns
    if let Some(n) = first_number(&CONTEXT_PR_RE) {
        return (None, Some(n));
    }
    (None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GITHUB_HOST: &str = "github.com";
    const GITEA_HOST: &str = "gitea.example.com";

    #[test]
    fn test_parse_issue_created() {
        let output = "Creating issue...\nhttps://github.com/owner/repo/issues/42";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::IssueCreated);
        assert_eq!(events[0].issue_number, Some(42));
    }

    #[test]
    fn test_parse_pr_created() {
        let output = "Creating pull request for feature/test into main in owner/repo\n\nhttps://github.com/owner/repo/pull/123";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::PrCreated);
        assert_eq!(events[0].pr_number, Some(123));
    }

    #[test]
    fn test_parse_pr_merged() {
        let output = "Merged pull request #456 (squash)";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert!(!events.is_empty());
        let merge_event = events
            .iter()
            .find(|e| e.event_type == PromptForgeEventType::PrMerged);
        assert!(merge_event.is_some());
        if let Some(evt) = merge_event {
            assert_eq!(evt.pr_number, Some(456));
        }
    }

    #[test]
    fn test_parse_issue_closed() {
        let output = "Closed issue #789";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::IssueClosed);
        assert_eq!(events[0].issue_number, Some(789));
    }

    #[test]
    fn test_parse_label_added() {
        let output = "gh issue edit 42 --add-label \"loom:building\"";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::LabelAdded);
        assert_eq!(events[0].labels_added, vec!["loom:building"]);
        assert_eq!(events[0].issue_number, Some(42));
    }

    #[test]
    fn test_parse_label_removed() {
        let output = "gh issue edit 42 --remove-label loom:issue";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::LabelRemoved);
        assert_eq!(events[0].labels_removed, vec!["loom:issue"]);
    }

    #[test]
    fn test_parse_review_approved() {
        let output = "Approved pull request #123";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::ReviewSubmitted);
        assert_eq!(events[0].pr_number, Some(123));
    }

    #[test]
    fn test_parse_multiple_events() {
        let output = "gh issue edit 42 --remove-label loom:issue --add-label loom:building";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 2);

        let remove_event = events
            .iter()
            .find(|e| e.event_type == PromptForgeEventType::LabelRemoved);
        let add_event = events
            .iter()
            .find(|e| e.event_type == PromptForgeEventType::LabelAdded);

        assert!(remove_event.is_some());
        assert!(add_event.is_some());
        if let Some(evt) = remove_event {
            assert_eq!(evt.labels_removed, vec!["loom:issue"]);
        }
        if let Some(evt) = add_event {
            assert_eq!(evt.labels_added, vec!["loom:building"]);
        }
    }

    #[test]
    fn test_no_false_positives() {
        let output = "Compiling loom-daemon v0.1.0\nFinished release target";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert!(events.is_empty());
    }

    #[test]
    fn test_to_prompt_forge_event() {
        let parsed = ParsedForgeEvent {
            event_type: PromptForgeEventType::LabelAdded,
            issue_number: Some(42),
            pr_number: None,
            labels_added: vec!["loom:building".to_string()],
            labels_removed: Vec::new(),
        };

        let event = parsed.to_prompt_forge_event(Some(100));
        assert_eq!(event.input_id, Some(100));
        assert_eq!(event.issue_number, Some(42));
        assert_eq!(event.label_after, Some(vec!["loom:building".to_string()]));
        assert!(event.label_before.is_none());
    }

    #[test]
    fn test_static_regexes_compile() {
        // Force every LazyLock so a bad static pattern fails here, not in production.
        for re in [
            &*MERGE_RE,
            &*CLOSE_ISSUE_RE,
            &*CLOSE_PR_RE,
            &*LABEL_ADD_RE,
            &*LABEL_REMOVE_RE,
            &*REVIEW_RE,
            &*CONTEXT_ISSUE_RE,
            &*CONTEXT_PR_RE,
        ] {
            assert!(!re.as_str().is_empty());
        }
    }

    #[test]
    fn test_parse_pr_label_context_and_comma_list() {
        let output = "gh pr edit 7 --add-label \"a, b\" --remove-label x,y";
        let events = parse_forge_events(output, GITHUB_HOST);
        // `[^"'\s,]+` stops at the space inside the quotes, so only "a," is captured.
        let added = events
            .iter()
            .find(|e| e.event_type == PromptForgeEventType::LabelAdded)
            .expect("label added");
        assert_eq!(added.pr_number, Some(7));
        assert_eq!(added.issue_number, None);
        let removed = events
            .iter()
            .find(|e| e.event_type == PromptForgeEventType::LabelRemoved)
            .expect("label removed");
        assert_eq!(removed.labels_removed, vec!["x", "y"]);
    }

    #[test]
    fn test_parse_issue_closed_trailing_form_and_pr_closed() {
        let events = parse_forge_events("Issue #5 closed\nClosed pull request #6", GITHUB_HOST);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event_type, PromptForgeEventType::IssueClosed);
        assert_eq!(events[0].issue_number, Some(5));
        assert_eq!(events[1].event_type, PromptForgeEventType::PrClosed);
        assert_eq!(events[1].pr_number, Some(6));
    }

    // --- Gitea-specific tests ---

    #[test]
    fn test_gitea_issue_created() {
        let output = "Creating issue...\nhttps://gitea.example.com/owner/repo/issues/42";
        let events = parse_forge_events(output, GITEA_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::IssueCreated);
        assert_eq!(events[0].issue_number, Some(42));
    }

    #[test]
    fn test_gitea_pr_created_with_pulls_path() {
        // Gitea uses /pulls/ instead of /pull/
        let output = "Creating pull request...\nhttps://gitea.example.com/owner/repo/pulls/99";
        let events = parse_forge_events(output, GITEA_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::PrCreated);
        assert_eq!(events[0].pr_number, Some(99));
    }

    #[test]
    fn test_github_url_not_matched_with_gitea_host() {
        // When configured for Gitea, GitHub URLs should not match
        let output = "https://github.com/owner/repo/pull/123";
        let events = parse_forge_events(output, GITEA_HOST);
        assert!(events.is_empty(), "GitHub URL should not match when forge_host is set to Gitea");
    }

    #[test]
    fn test_gitea_url_not_matched_with_github_host() {
        // When configured for GitHub, Gitea URLs should not match
        let output = "https://gitea.example.com/owner/repo/pulls/42";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert!(events.is_empty(), "Gitea URL should not match when forge_host is set to GitHub");
    }

    #[test]
    fn test_github_pull_path_still_works() {
        // GitHub's /pull/ path should still work (not just /pulls/)
        let output = "https://github.com/owner/repo/pull/456";
        let events = parse_forge_events(output, GITHUB_HOST);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, PromptForgeEventType::PrCreated);
        assert_eq!(events[0].pr_number, Some(456));
    }
}
