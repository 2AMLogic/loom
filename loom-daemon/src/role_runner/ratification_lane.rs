//! The ratification reviewer lane's selection layer (#13187, step 1 of 3 of
//! #13186).
//!
//! Finds open PRs that need key reviews: exactly the PRs Champion's
//! ratification hold would hold ([`ratification::scan`] is `Hold`; one shared
//! predicate, #13553). Selection is
//! [`find_candidates`]; [`run_lane`] (step 3, #13345) ties selection, the
//! step-2 dispatch and marker posting together. The forge is injected
//! ([`PrSource`], [`CommentSink`]) so every rule is unit-testable offline.
//!
//! # Fail closed
//!
//! A missing, disabled or malformed `ratificationLane` block yields
//! [`Config::disabled`]; [`find_candidates`] on a disabled config returns
//! nothing and never touches the source.
//!
//! # Key marker (wire format, emitted by [`post_markers`])
//!
//! `<!-- ratify:key=<ee|market> sha=<40-hex head> -->`. A key is complete for
//! a head when a trusted comment author (the source returns trusted comments
//! only; a marker from an untrusted author is prose,
//! `.loom/docs/comment-trust.md`) posted that marker for the **current**
//! head — whatever verdict it records. A PR is selected while any key is
//! incomplete, and a tick dispatches and posts **only the incomplete keys**
//! (#14797): the completed key consumes no instruction read, session or post
//! and no withheld count. A marker is
//! posted only when the session's output parses as that key's review
//! ([`parse_review`], #13762): a session that stops without a verdict posts
//! nothing, so its head stays marker-less for that key and it is retried.

use super::ratification_attempts::{self, LaneMemory};
use crate::champion_hold::ratification::{self, ChangedFile, Comment, Verdict};
use crate::park_record::blank_block_code;
use regex::Regex;
use serde_json::Value;

/// Where an instruction set is pinned (fetched in step 2, only parsed here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    pub repo: String,
    pub path: String,
    pub commit: String,
}

/// The `ratificationLane` block of `.loom/config.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub enabled: bool,
    pub ee: Option<Pin>,
    pub market: Option<Pin>,
    /// The hold's detector config (`champion.ratificationPaths` / `Phrases`),
    /// so selection and Champion's hold read the same predicate.
    pub hold: ratification::Config,
}

impl Config {
    /// The fail-closed value: the lane never runs.
    #[must_use]
    pub fn disabled() -> Self {
        Config {
            enabled: false,
            ee: None,
            market: None,
            hold: ratification::Config::default(),
        }
    }

    /// From the effective config document. Anything but a well-formed block
    /// with `enabled: true` yields [`Config::disabled`].
    #[must_use]
    pub fn from_effective(effective: &Value) -> Self {
        let Some(block) = crate::config_resolver::get_path(effective, "ratificationLane")
            .filter(|v| v.is_object())
        else {
            return Self::disabled();
        };
        if block.get("enabled").and_then(Value::as_bool) != Some(true) {
            return Self::disabled();
        }
        let pin = |key: &str| -> Option<Pin> {
            let p = block.get(key)?;
            let field = |f: &str| {
                p.get(f)
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .map(str::to_string)
            };
            Some(Pin {
                repo: field("repo")?,
                path: field("path")?,
                commit: field("commit")?,
            })
        };
        let (ee, market) = (pin("ee"), pin("market"));
        // Both instruction pins are required; a half-configured lane is off.
        if ee.is_none() || market.is_none() {
            return Self::disabled();
        }
        Config {
            enabled: true,
            ee,
            market,
            hold: ratification::Config::from_effective(effective),
        }
    }
}

/// An open PR as the lane sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPr {
    pub number: u64,
    pub author: String,
    pub head_sha: String,
}

/// The forge, injected.
pub trait PrSource {
    fn open_prs(&self) -> Vec<OpenPr>;
    /// The PR's change list with head content, as the hold's [`ratification::scan`]
    /// reads it.
    fn files(&self, pr: u64, hold: &ratification::Config) -> Vec<ChangedFile>;
    /// The PR's comments by trusted authors only. `Err` when the listing
    /// or any record is unreadable, so incomplete history cannot authorize
    /// a rerun (#14797). A successfully read empty list is `Ok(vec![])`.
    fn trusted_comments(&self, pr: u64) -> Result<Vec<Comment>, String>;
    /// Why the head content of `path` on `pr` could not be read (the `gh`
    /// exit status and bounded stderr), when the last [`PrSource::files`] call
    /// recorded one.
    fn read_failure(&self, _pr: u64, _path: &str) -> Option<String> {
        None
    }
}

/// Log-once-per-change gate. A failure repeats on every tick; the WARN is
/// emitted once per `(pr, head_sha, kind)` and again only when its reason text
/// changes. In-memory: resets with the daemon.
#[derive(Debug, Default)]
pub struct WarnDedup {
    seen: std::collections::HashMap<(u64, String, &'static str), String>,
}

impl WarnDedup {
    /// True when this `(pr, head_sha, kind, reason)` has not been logged yet.
    pub fn should_log(
        &mut self,
        pr: u64,
        head_sha: &str,
        kind: &'static str,
        reason: &str,
    ) -> bool {
        let key = (pr, head_sha.to_string(), kind);
        if self.seen.get(&key).is_some_and(|r| r == reason) {
            return false;
        }
        self.seen.insert(key, reason.to_string());
        true
    }
}

/// The process-wide gate the real lane logs through.
fn warn_once(pr: u64, head_sha: &str, kind: &'static str, reason: &str) -> bool {
    static GATE: std::sync::OnceLock<std::sync::Mutex<WarnDedup>> = std::sync::OnceLock::new();
    GATE.get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .should_log(pr, head_sha, kind, reason)
}

/// The two review keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    Ee,
    Market,
}

/// What the finder decided for one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Needs the key reviews in `missing` (empty never appears: a PR with
    /// both keys complete is not selected at all).
    Candidate {
        pr: u64,
        head_sha: String,
        /// The keys still missing a current-head marker, EE-first (#14797).
        missing: Vec<Key>,
    },
    /// Matches but must not be reviewed; the reason is recorded.
    Refused { pr: u64, reason: String },
}

/// Whether Champion's ratification hold would hold this PR. An unreadable
/// candidate file is not selected (fail closed: never marker an unread file)
/// and is logged.
#[must_use]
pub fn matches_pr(pr: u64, files: &[ChangedFile], hold: &ratification::Config) -> bool {
    !matches!(
        match_state(pr, "", files, hold, &|_| None),
        MatchState::Clear | MatchState::Unreadable
    )
}

/// Selection's view of one PR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MatchState {
    Hold,
    Clear,
    Unreadable,
}

/// [`ratification::scan`] plus the unreadable WARN, with each path's read
/// failure (`detail`) in the reason and the WARN deduplicated per head.
fn match_state(
    pr: u64,
    head_sha: &str,
    files: &[ChangedFile],
    hold: &ratification::Config,
    detail: &dyn Fn(&str) -> Option<String>,
) -> MatchState {
    match ratification::scan(files, hold) {
        Verdict::Hold(_) => MatchState::Hold,
        Verdict::Clear => MatchState::Clear,
        Verdict::Unreadable(paths) => {
            let reason = paths
                .iter()
                .map(|p| match detail(p) {
                    Some(d) => format!("{p} ({d})"),
                    None => p.clone(),
                })
                .collect::<Vec<_>>()
                .join(", ");
            if warn_once(pr, head_sha, "unreadable", &reason) {
                log::warn!("ratification_lane: PR #{pr} not selected; unreadable: [{reason}]");
            }
            MatchState::Unreadable
        }
    }
}

/// The sweep ids of the builder sessions recorded in `comments` by PR-lease
/// records (`<!-- loom:pr-lease[-released] role=builder ... sweep=<id> ... -->`).
#[must_use]
pub fn builder_sessions(comments: &[Comment]) -> Vec<String> {
    let re =
        Regex::new(r"<!--\s*loom:pr-lease(?:-released)?\s+role=builder\b[^>]*?\bsweep=([^\s>]+)")
            .expect("static");
    comments
        .iter()
        .flat_map(|c| re.captures_iter(&c.body).map(|m| m[1].to_string()))
        .collect()
}

/// Which keys carry a marker for `head_sha` in `comments`.
#[must_use]
pub fn keys_marked(comments: &[Comment], head_sha: &str) -> Vec<Key> {
    let re =
        Regex::new(r"<!--\s*ratify:key=(ee|market)\s+sha=([0-9a-fA-F]{40})\s*-->").expect("static");
    let mut keys = Vec::new();
    for c in comments {
        for cap in re.captures_iter(&c.body) {
            if !cap[2].eq_ignore_ascii_case(head_sha) {
                continue;
            }
            let k = if &cap[1] == "ee" {
                Key::Ee
            } else {
                Key::Market
            };
            if !keys.contains(&k) {
                keys.push(k);
            }
        }
    }
    keys
}

/// The keys still missing a current-head marker, in dispatch order (EE, then
/// market). A key is complete regardless of the verdict its marker records:
/// `escalate` and `request-changes` reviews were still performed (#14797).
#[must_use]
pub fn missing_keys(comments: &[Comment], head_sha: &str) -> Vec<Key> {
    let marked = keys_marked(comments, head_sha);
    [Key::Ee, Key::Market]
        .into_iter()
        .filter(|k| !marked.contains(k))
        .collect()
}

/// Select the PRs needing key reviews. Independence is by session, not author
/// login: in a fleet the Builder opens the PR as the same identity that
/// dispatches the lane. A PR is refused only when `lane_session` is the
/// session recorded as having built it.
#[must_use]
pub fn find_candidates(
    cfg: &Config,
    lane_session: Option<&str>,
    src: &dyn PrSource,
) -> Vec<Selection> {
    find_candidates_counted(cfg, lane_session, src).0
}

/// [`find_candidates`] plus the number of PRs not selected because a candidate
/// file or comment history was unreadable.
#[must_use]
pub fn find_candidates_counted(
    cfg: &Config,
    lane_session: Option<&str>,
    src: &dyn PrSource,
) -> (Vec<Selection>, usize) {
    if !cfg.enabled {
        return (Vec::new(), 0);
    }
    let mut out = Vec::new();
    let mut unreadable = 0;
    for pr in src.open_prs() {
        let files = src.files(pr.number, &cfg.hold);
        let detail = |path: &str| src.read_failure(pr.number, path);
        match match_state(pr.number, &pr.head_sha, &files, &cfg.hold, &detail) {
            MatchState::Hold => {}
            MatchState::Clear => continue,
            MatchState::Unreadable => {
                unreadable += 1;
                continue;
            }
        }
        let comments = match src.trusted_comments(pr.number) {
            Ok(c) => c,
            Err(reason) => {
                // Fail closed (#14797): an unreadable comment list cannot
                // establish marker state or the builder-session check, so it
                // must not authorize a rerun.
                unreadable += 1;
                if warn_once(pr.number, &pr.head_sha, "comments-unreadable", &reason) {
                    log::warn!(
                        "ratification_lane: PR #{} not selected; comment list unreadable: {reason}",
                        pr.number
                    );
                }
                continue;
            }
        };
        if let Some(sid) =
            lane_session.filter(|l| builder_sessions(&comments).iter().any(|b| b == l))
        {
            out.push(Selection::Refused {
                pr: pr.number,
                reason: format!("lane session {sid} is the session that built the PR"),
            });
            continue;
        }
        let missing = missing_keys(&comments, &pr.head_sha);
        if missing.is_empty() {
            continue;
        }
        out.push(Selection::Candidate {
            pr: pr.number,
            head_sha: pr.head_sha,
            missing,
        });
    }
    (out, unreadable)
}

// ---------------------------------------------------------------------------
// Marker posting and the lane tick (#13345, step 3 of 3 of #13186)
// ---------------------------------------------------------------------------

/// Posts one comment on a PR. Injected so posting is testable offline.
pub trait CommentSink {
    /// # Errors
    ///
    /// The forge refused or could not be reached; the text is logged.
    fn post(&self, pr: u64, body: &str) -> Result<(), String>;
}

impl Key {
    /// The wire name used in the marker.
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Key::Ee => "ee",
            Key::Market => "market",
        }
    }

    /// The heading that starts this key's review in the reviewer's final
    /// message (the pinned output contract, #13762).
    #[must_use]
    pub fn review_heading(self) -> &'static str {
        match self {
            Key::Ee => "## EE-key review:",
            Key::Market => "## Market-key review:",
        }
    }

    /// The verdict tokens this key's review may carry.
    #[must_use]
    pub fn allowed_tokens(self) -> &'static [&'static str] {
        match self {
            Key::Ee => &["approve", "request-changes"],
            Key::Market => &[
                "competitive",
                "adequate-for-catalog",
                "uncompetitive",
                "escalate",
            ],
        }
    }
}

/// A key review parsed from a reviewer session's output (#13762).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedReview {
    /// The verdict token from the `**Overall verdict:**` line, normalized
    /// ([`normalize_verdict_token`]). Gate only: the posted comment carries
    /// the review text verbatim.
    pub token: String,
    /// The review text: from the key's heading (its last occurrence in the
    /// session output) to the first launcher/wrapper line after it — the
    /// wrapper writes its own lines after the model's final message, and
    /// they are never posted.
    pub text: String,
}

/// Why a session's output did not parse as this key's review (#13762,
/// #15315). Both variants withhold the marker; they differ only in the fixed
/// class the tick counts them under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewError {
    /// The review block is the instruction template echoed back: its heading
    /// still reads `<block name / class>`, or a verdict line still carries
    /// the `<…>` placeholder or a `|` list of alternatives
    /// ([`WithheldClass::VerdictPlaceholder`]).
    Placeholder(String),
    /// Anything else: no heading, no verdict line, or an empty, unknown,
    /// ambiguous or conflicting token ([`WithheldClass::VerdictInvalid`]).
    Invalid(String),
}

impl ReviewError {
    /// The reason the lane logs as withheld.
    #[must_use]
    pub fn reason(&self) -> &str {
        match self {
            Self::Placeholder(r) | Self::Invalid(r) => r,
        }
    }
}

impl std::fmt::Display for ReviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason())
    }
}

/// The pinned verdict label, as it appears in error reasons.
const VERDICT_LINE: &str = "**Overall verdict:**";

/// Extract this key's review from a reviewer session's output. The heading
/// ([`Key::review_heading`]) is the start delimiter: the review runs from its
/// **last** occurrence in `output` to the first launcher/wrapper line after
/// it ([`is_wrapper_line`]), so launcher or wrapper noise on either side of
/// the review is never read as a verdict or posted — no auth-mode line, no
/// wrapper tail.
///
/// A verdict line starts with the `**Overall verdict:**` label or one of its
/// harmless variants ([`verdict_label`]); beside a bold one, an unbolded
/// `Overall verdict:` line whose first word is no verdict word is prose and
/// is not read. Its verdict is the **first word** after the label
/// ([`first_word`], terminated by a comma or whitespace), normalized by
/// [`normalize_verdict_token`] — a trailing `.` `;` `:` `!`, a balanced
/// `**` `__` `` ` `` `*` `_` wrapping and ASCII case are harmless (#15315) —
/// and must then **exactly** equal one of [`Key::allowed_tokens`].
///
/// Only the review block is read, and the template is checked first: a
/// heading whose title starts with `<`, or a verdict line in one of the
/// template's shapes ([`is_placeholder_verdict`]: the `<…>` placeholder or a
/// `|` list of verdict words), is [`ReviewError::Placeholder`] whatever else
/// the block says. Anything else that is not a verdict — no heading, no
/// verdict line, an empty or unknown token, a first clause
/// ([`verdict_clause`]) that names a second allowed token
/// (`approve or request-changes`, `approve. request-changes`), or two lines
/// whose normalized tokens differ — is [`ReviewError::Invalid`]. Text after
/// the first comma is the template's "one sentence why" and is not read for
/// a second verdict word.
///
/// # Errors
///
/// The output is not a review for `key`; the reason is logged as withheld.
pub fn parse_review(key: Key, output: &str) -> Result<ParsedReview, ReviewError> {
    use ReviewError::{Invalid, Placeholder};
    let heading = key.review_heading();
    let Some(at) = output.rfind(heading) else {
        return Err(Invalid(format!("no {heading} heading in session output")));
    };
    let start = output[..at].rfind('\n').map_or(0, |i| i + 1);
    let lines: Vec<&str> = output[start..].lines().collect();
    let end = lines
        .iter()
        .position(|line| is_wrapper_line(line))
        .unwrap_or(lines.len());
    let joined = lines[..end].join("\n");
    let review = joined.trim_end();
    // The template's own heading (`<block name / class>`) is never a review,
    // even when a filled-looking verdict follows it.
    let title = output[at + heading.len()..].lines().next().unwrap_or("");
    if title.trim().starts_with('<') {
        return Err(Placeholder("review heading is the template placeholder".into()));
    }
    let labelled: Vec<(bool, &str)> = review.lines().filter_map(verdict_label).collect();
    // Beside a bold verdict line, an unbolded `Overall verdict:` line whose
    // first word is no verdict word is a sentence, not a second verdict.
    let bold = labelled.iter().any(|(b, _)| *b);
    let verdicts: Vec<&str> = labelled
        .into_iter()
        .filter(|(b, rest)| {
            *b || !bold
                || key
                    .allowed_tokens()
                    .contains(&normalize_verdict_token(first_word(rest)).as_str())
        })
        .map(|(_, rest)| rest)
        .collect();
    // Placeholder outranks every other check: a block that holds both a
    // template line and a valid token is still the template.
    if verdicts
        .iter()
        .any(|rest| is_placeholder_verdict(key, rest))
    {
        return Err(Placeholder("verdict line is the template placeholder".into()));
    }
    let mut token: Option<(String, String)> = None;
    for rest in verdicts {
        let raw = first_word(rest).to_string();
        let normalized = normalize_verdict_token(&raw);
        if normalized.is_empty() {
            return Err(Invalid(format!("empty verdict token on an {VERDICT_LINE} line")));
        }
        if let Some(other) = second_allowed_token(key, rest, &normalized) {
            return Err(Invalid(format!(
                "ambiguous verdict clause names both {normalized:?} and {other:?}"
            )));
        }
        match &token {
            Some((prev, prev_raw)) if *prev != normalized => {
                return Err(Invalid(format!(
                    "conflicting verdict tokens {prev_raw:?} and {raw:?}"
                )));
            }
            Some(_) => {}
            None => token = Some((normalized, raw)),
        }
    }
    let (token, raw) =
        token.ok_or_else(|| Invalid(format!("no {VERDICT_LINE} line after {heading}")))?;
    if !key.allowed_tokens().contains(&token.as_str()) {
        return Err(Invalid(format!("unknown verdict token {raw:?}")));
    }
    Ok(ParsedReview {
        token,
        text: review.to_string(),
    })
}

/// The text after a verdict label at the start of `line` (leading
/// whitespace allowed), and whether the label is bold: the pinned
/// `**Overall verdict:**`, the harmless variants `**Overall verdict**:` and
/// the unbolded `Overall verdict:`, the words in any ASCII case (#15315).
/// `None` for any other line.
fn verdict_label(line: &str) -> Option<(bool, &str)> {
    const WORDS: &str = "overall verdict";
    let line = line.trim_start();
    let (bold, after) = match line.strip_prefix("**") {
        Some(rest) => (true, rest),
        None => (false, line),
    };
    if !after.get(..WORDS.len())?.eq_ignore_ascii_case(WORDS) {
        return None;
    }
    let tail = &after[WORDS.len()..];
    let rest = if bold {
        tail.strip_prefix(":**")
            .or_else(|| tail.strip_prefix("**:"))
    } else {
        tail.strip_prefix(':')
    }?;
    Some((bold, rest))
}

/// The first word of a verdict line (the text after its label): up to the
/// first comma or whitespace. It decides the verdict.
fn first_word(rest: &str) -> &str {
    let rest = rest.trim_start();
    let end = rest
        .find(|c: char| c == ',' || c.is_whitespace())
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Whether a verdict line (the text after its label) is the template's
/// placeholder, in one of the template's own shapes: after any leading
/// `` ` `` / `*` it starts with `<`; its first word carries `|`, `<` or `>`;
/// its first clause ([`verdict_clause`]) puts a `|` beside one of `key`'s
/// verdict words (`approve | request-changes`); or the line anywhere holds a
/// `<word|word>` list. A `<` or `>` elsewhere is a comparison and a `|x|` a
/// magnitude (`approve because Vth < 0.4 V`), never a placeholder.
fn is_placeholder_verdict(key: Key, rest: &str) -> bool {
    static TEMPLATE_LIST: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let template_list = TEMPLATE_LIST.get_or_init(|| {
        Regex::new(r"(?i)<\s*[a-z][a-z-]*(?:\s*\|\s*[a-z][a-z-]*)+\s*>").expect("static")
    });
    let rest = rest.trim_start();
    rest.trim_start_matches(['`', '*']).starts_with('<')
        || first_word(rest).contains(['|', '<', '>'])
        || names_alternatives(key, verdict_clause(rest))
        || template_list.is_match(rest)
}

/// Whether a `|` in `clause` sits directly beside one of `key`'s verdict
/// words (emphasis and whitespace aside): the template's list of
/// alternatives, written out.
fn names_alternatives(key: Key, clause: &str) -> bool {
    let clause = clause.to_ascii_lowercase();
    let is_token = |word: Option<&str>| {
        word.is_some_and(|w| {
            key.allowed_tokens()
                .contains(&w.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '-')))
        })
    };
    let parts: Vec<&str> = clause.split('|').collect();
    parts.windows(2).any(|pair| {
        is_token(pair[0].split_whitespace().last()) || is_token(pair[1].split_whitespace().next())
    })
}

/// What ends the first clause after a bare first word: a comma, sentence
/// punctuation, a parenthesis or a Unicode dash.
const CLAUSE_ENDS: [char; 9] = [',', '.', ';', ':', '!', '?', '(', '\u{2014}', '\u{2013}'];

/// The first clause of a verdict line: its first word ([`first_word`]),
/// then the text up to the next comma, sentence punctuation, dash or
/// parenthesis. Wide enough to catch alternatives written as one clause
/// (`approve | request-changes`, `approve or request-changes`), narrow
/// enough that the sentence of reasoning after a bare verdict word
/// (`approve — the margin is > 3 dB`) is not read as part of it.
///
/// When the first word itself carries one of those marks (`approve.`,
/// `**approve.**`, `` `approve`: ``) — which [`normalize_verdict_token`]
/// strips, and which the gate before #15315 rejected outright — the clause
/// runs on to the first comma or the end of the line instead (critique A5's
/// boundary), so normalization can never hide a second verdict behind the
/// mark it stripped (`approve. request-changes`).
fn verdict_clause(rest: &str) -> &str {
    let rest = rest.trim_start();
    let word = first_word(rest).len();
    let ends: &[char] = if rest[..word].contains(&CLAUSE_ENDS[1..]) {
        &CLAUSE_ENDS[..1]
    } else {
        &CLAUSE_ENDS
    };
    let end = rest[word..].find(ends).map_or(rest.len(), |i| word + i);
    &rest[..end]
}

/// Another of `key`'s allowed tokens named, as a whole word in any ASCII
/// case, in the first clause of a verdict line whose first word normalized
/// to `token` — an ambiguous verdict, withheld.
fn second_allowed_token(key: Key, rest: &str, token: &str) -> Option<&'static str> {
    if !key.allowed_tokens().contains(&token) {
        return None;
    }
    let clause = verdict_clause(rest).to_ascii_lowercase();
    clause
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
        .find_map(|word| {
            key.allowed_tokens()
                .iter()
                .copied()
                .find(|t| *t == word && *t != token)
        })
}

/// Normalize a raw verdict token's harmless formatting (#15315): repeat,
/// until nothing changes, (a) strip a trailing run of `.` `;` `:` `!` and
/// (b) remove one balanced emphasis pair (`**` `__` `` ` `` `*` `_` on both
/// ends, only when something is left between them); then fold ASCII case.
/// Never strips `?`, `)`, `|`, `<` or `>`, and never removes a one-sided
/// marker, so `approve?`, `**approve` and `<approve>` stay non-tokens.
#[must_use]
pub fn normalize_verdict_token(raw: &str) -> String {
    let mut t = raw;
    loop {
        let before = t.len();
        t = t.trim_end_matches(['.', ';', ':', '!']);
        for pair in ["**", "__", "`", "*", "_"] {
            if t.len() > 2 * pair.len() && t.starts_with(pair) && t.ends_with(pair) {
                t = &t[pair.len()..t.len() - pair.len()];
                break;
            }
        }
        if t.len() == before {
            break;
        }
    }
    t.to_ascii_lowercase()
}

/// A launcher or `claude-wrapper.sh` line rather than review prose: a
/// timestamped log line (`[<ISO-8601>] [INFO|WARN|ERROR] …`) or a
/// `[claude-wrapper]` diagnostic. The wrapper writes these *after* the
/// model's final message, so in a captured role log they follow the review;
/// they end the extracted review and are never posted (#13762).
fn is_wrapper_line(line: &str) -> bool {
    let line = line.trim_start();
    if line.starts_with("[claude-wrapper]") {
        return true;
    }
    let Some(rest) = line.strip_prefix('[') else {
        return false;
    };
    let Some((stamp, after)) = rest.split_once(']') else {
        return false;
    };
    let after = after.trim_start();
    is_timestamp(stamp)
        && (after.starts_with("[INFO]")
            || after.starts_with("[WARN]")
            || after.starts_with("[ERROR]"))
}

/// Plausible ISO-8601 timestamp: only digits and ISO-8601 separators, at
/// least a full date long, with a date separator — so a markdown link or a
/// short citation such as `[2026]` is not mistaken for a log line.
fn is_timestamp(s: &str) -> bool {
    s.len() >= 10
        && s.contains('-')
        && s.bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'-' | b':' | b'T' | b'Z' | b'+' | b'.'))
}

/// The comment body for one key: the wire marker (exactly the shape
/// [`keys_marked`] parses), then a visible `reviewer=` line. `head_sha` is the
/// head captured at selection time, never re-read.
///
/// `verdict` is the extracted review text (#13762), appended after the marker
/// line. It is bounded to [`MAX_VERDICT_CHARS`] (keeping the head, with an
/// explicit truncation notice — the heading and verdict line survive) and its
/// `<!--` / `-->` are escaped so it can
/// never form or alter a marker that [`keys_marked`] would parse.
#[must_use]
pub fn marker_body(key: Key, head_sha: &str, reviewer: &str, verdict: Option<&str>) -> String {
    let mut body = format!(
        "<!-- ratify:key={} sha={head_sha} -->\nreviewer={reviewer}\nReviewed head: {head_sha}\n",
        key.wire()
    );
    if let Some(text) = verdict.map(str::trim).filter(|t| !t.is_empty()) {
        body.push_str("\nVerdict:\n\n");
        body.push_str(&bound_verdict(text));
        body.push('\n');
    }
    body
}

/// Longest verdict text posted, in characters.
pub const MAX_VERDICT_CHARS: usize = 4000;

fn bound_verdict(text: &str) -> String {
    let escaped = text.replace("<!--", "&lt;!--").replace("-->", "--&gt;");
    let total = escaped.chars().count();
    if total <= MAX_VERDICT_CHARS {
        return escaped;
    }
    let head: String = escaped.chars().take(MAX_VERDICT_CHARS).collect();
    format!("{head}\n[truncated: last {} characters omitted]", total - MAX_VERDICT_CHARS)
}

/// What the lane did for one key of one PR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPost {
    Posted,
    /// No marker posted. Only the fixed class goes to telemetry.
    Withheld(WithheldClass, String),
}

/// Fixed causes of a withheld marker, in tick attribute order after `posted`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WithheldClass {
    PreLaunchFailed = 1,
    InvocationFailed,
    OutputMissing,
    VerdictInvalid,
    /// The output was the instruction template echoed back (#15315).
    VerdictPlaceholder,
    PostFailed,
}

impl WithheldClass {
    /// The class's span-key suffix (`loom.ratification.<key>.<wire>`).
    #[must_use]
    pub fn wire(self) -> &'static str {
        match self {
            Self::PreLaunchFailed => "pre_launch_failed",
            Self::InvocationFailed => "invocation_failed",
            Self::OutputMissing => "output_missing",
            Self::VerdictInvalid => "verdict_invalid",
            Self::VerdictPlaceholder => "verdict_placeholder",
            Self::PostFailed => "post_failed",
        }
    }
}

/// Post a marker for each **attempted** key whose session ended in
/// `KeyOutcome::Ran(RoleTickOutcome::Success)` **and** whose output parses as
/// that key's review ([`parse_review`]) — a session that stops without a
/// verdict posts nothing (#13762). Anything else (not launched, a session
/// that ran but did not succeed, or no parsed verdict) posts nothing: fail
/// closed. One key's outcome never affects the other. A key this pass did
/// not attempt (already complete for this head, #14797) contributes no
/// result: no post and no withheld count.
pub fn post_markers(
    pr: u64,
    head_sha: &str,
    dispatched: &super::ratification_dispatch::Dispatched,
    sink: &dyn CommentSink,
) -> Vec<(Key, KeyPost)> {
    use super::ratification_dispatch::{role_name, KeyOutcome};
    use super::RoleTickOutcome;
    use WithheldClass::{
        InvocationFailed, OutputMissing, PostFailed, PreLaunchFailed, VerdictInvalid,
        VerdictPlaceholder,
    };
    [Key::Ee, Key::Market]
        .into_iter()
        .filter_map(|key| {
            let result = match dispatched.get(key)? {
                KeyOutcome::Ran(RoleTickOutcome::Success) => {
                    match dispatched.output(key).map(|out| parse_review(key, out)) {
                        Some(Ok(review)) => sink
                            .post(
                                pr,
                                &marker_body(key, head_sha, role_name(key), Some(&review.text)),
                            )
                            .map(|()| KeyPost::Posted)
                            .unwrap_or_else(|e| {
                                KeyPost::Withheld(PostFailed, format!("marker post failed: {e}"))
                            }),
                        Some(Err(ReviewError::Placeholder(reason))) => {
                            KeyPost::Withheld(VerdictPlaceholder, reason)
                        }
                        Some(Err(ReviewError::Invalid(reason))) => {
                            KeyPost::Withheld(VerdictInvalid, reason)
                        }
                        None => {
                            KeyPost::Withheld(OutputMissing, "no session output captured".into())
                        }
                    }
                }
                KeyOutcome::Ran(other) => KeyPost::Withheld(
                    InvocationFailed,
                    format!("session did not succeed: {other:?}"),
                ),
                KeyOutcome::FailedClosed(reason) => {
                    KeyPost::Withheld(PreLaunchFailed, reason.clone())
                }
            };
            if let KeyPost::Withheld(_, reason) = &result {
                let kind = match key {
                    Key::Ee => "withheld:ee",
                    Key::Market => "withheld:market",
                };
                if warn_once(pr, head_sha, kind, reason) {
                    log::warn!("ratification_lane: PR #{pr} key {} withheld: {reason}", key.wire());
                }
            }
            Some((key, result))
        })
        .collect()
}

/// Per-PR key results of one lane pass.
pub type LaneResults = Vec<(u64, Vec<(Key, KeyPost)>)>;

/// One lane pass: select, dispatch both key sessions per candidate, post
/// markers for the keys that succeeded. A disabled `cfg` returns at once
/// without touching any source. Returns the per-PR key results.
pub fn run_lane(
    cfg: &Config,
    lane_session: Option<&str>,
    target_repo: &str,
    prs: &dyn PrSource,
    instructions: &dyn super::ratification_dispatch::InstructionSource,
    sink: &dyn CommentSink,
    runner: &mut dyn super::RoleInvocationRunner,
) -> LaneResults {
    run_lane_counted(cfg, lane_session, target_repo, prs, instructions, sink, runner).0
}

/// [`run_lane`] plus the number of PRs selection refused (the tick's `skipped`).
pub fn run_lane_counted(
    cfg: &Config,
    lane_session: Option<&str>,
    target_repo: &str,
    prs: &dyn PrSource,
    instructions: &dyn super::ratification_dispatch::InstructionSource,
    sink: &dyn CommentSink,
    runner: &mut dyn super::RoleInvocationRunner,
) -> (LaneResults, usize) {
    let (results, refused, _unreadable) =
        run_lane_stats(cfg, lane_session, target_repo, prs, instructions, sink, runner);
    (results, refused)
}

/// [`run_lane_counted`] plus the number of PRs left unselected because a
/// candidate file or comment history was unreadable. One pass with a fresh
/// [`LaneMemory`] and no ledger file: the attempt bound holds within the
/// pass only ([`run_lane_stats_with`] is the bounded lane).
pub fn run_lane_stats(
    cfg: &Config,
    lane_session: Option<&str>,
    target_repo: &str,
    prs: &dyn PrSource,
    instructions: &dyn super::ratification_dispatch::InstructionSource,
    sink: &dyn CommentSink,
    runner: &mut dyn super::RoleInvocationRunner,
) -> (LaneResults, usize, usize) {
    let memory = std::sync::Mutex::new(LaneMemory::default());
    let state = LaneState {
        memory: &memory,
        ledger: None,
        now: chrono::Utc::now(),
    };
    let pass = run_lane_stats_with(
        cfg,
        lane_session,
        target_repo,
        prs,
        instructions,
        sink,
        runner,
        &state,
    );
    (pass.results, pass.refused, pass.unreadable)
}

/// What the attempt bound (#15315) reads and writes during one pass.
pub struct LaneState<'a> {
    /// The backoffs, the pin cache and unsaved ledger records; locked only
    /// briefly, never across a session.
    pub memory: &'a std::sync::Mutex<LaneMemory>,
    /// The host-local attempt ledger ([`ratification_attempts::ledger_path`]);
    /// `None` keeps the ledger in memory for this pass only.
    pub ledger: Option<&'a std::path::Path>,
    pub now: chrono::DateTime<chrono::Utc>,
}

/// What one bounded lane pass did.
#[derive(Debug, Default)]
pub struct LanePass {
    /// Per-PR key results of the candidates that dispatched a key.
    pub results: LaneResults,
    /// PRs selection refused.
    pub refused: usize,
    /// PRs not selected because a candidate file or comment history was
    /// unreadable.
    pub unreadable: usize,
    /// Candidates with no key dispatched: every missing key capped or
    /// deferred.
    pub held: usize,
    /// Missing keys not dispatched because they reached the attempt cap.
    pub capped: usize,
    /// Missing keys not dispatched because of a backoff.
    pub deferred: usize,
}

/// The bounded lane pass (#15315): selection exactly as
/// [`find_candidates_counted`] makes it, then, per candidate, only the
/// missing keys that are neither **capped** (at
/// [`ratification_attempts::MAX_COUNTED_ATTEMPTS_PER_WINDOW`] counted attempts
/// at this head in the window, read from the ledger) nor **deferred** (a
/// backoff in [`LaneMemory`]) are dispatched. After the markers are posted
/// each withheld key is accounted: a counted class is recorded in the
/// ledger, any other class starts its backoff
/// ([`ratification_attempts::backoff_for`]). The ledger is pruned and saved
/// at the end; a ledger that cannot be read is treated as empty and one that
/// cannot be saved is kept in `state.memory`, so the bound fails open and
/// still holds on this host.
#[allow(clippy::too_many_arguments)]
pub fn run_lane_stats_with(
    cfg: &Config,
    lane_session: Option<&str>,
    target_repo: &str,
    prs: &dyn PrSource,
    instructions: &dyn super::ratification_dispatch::InstructionSource,
    sink: &dyn CommentSink,
    runner: &mut dyn super::RoleInvocationRunner,
    state: &LaneState<'_>,
) -> LanePass {
    use ratification_attempts::{backoff_for, counts_toward_cap, lock, AttemptRecord, KeySlot};
    let (true, Some(ee), Some(market)) = (cfg.enabled, cfg.ee.as_ref(), cfg.market.as_ref()) else {
        return LanePass::default();
    };
    let now = state.now;
    let (mut ledger, mut dirty) = load_ledger(state);
    let instructions = ratification_attempts::CachedInstructions::new(instructions, state.memory);
    let (selections, unreadable) = find_candidates_counted(cfg, lane_session, prs);
    let mut pass = LanePass {
        unreadable,
        ..LanePass::default()
    };
    let mut current = std::collections::HashSet::new();
    for sel in selections {
        match sel {
            Selection::Refused { pr, reason } => {
                pass.refused += 1;
                log::info!("ratification_lane: PR #{pr} refused: {reason}");
            }
            Selection::Candidate {
                pr,
                head_sha,
                missing,
            } => {
                current.insert((pr, head_sha.to_ascii_lowercase()));
                let slot = |key: Key| KeySlot {
                    repo: target_repo,
                    pr,
                    head_sha: &head_sha,
                    key,
                    pin: match key {
                        Key::Ee => ee,
                        Key::Market => market,
                    },
                };
                let mut run = Vec::new();
                for key in missing {
                    if ledger
                        .capped_until(target_repo, pr, &head_sha, key, now)
                        .is_some()
                    {
                        pass.capped += 1;
                    } else if lock(state.memory).deferred(&slot(key), now) {
                        pass.deferred += 1;
                    } else {
                        run.push(key);
                    }
                }
                if run.is_empty() {
                    pass.held += 1;
                    continue;
                }
                let pr_ref = format!("{target_repo}#{pr}@{head_sha}");
                // Only the keys still missing a marker, and neither capped
                // nor deferred, are dispatched and posted (#14797, #15315);
                // any other key consumes no instruction read, visibility
                // read, session or post.
                let dispatched = super::ratification_dispatch::dispatch(
                    run.contains(&Key::Ee).then_some(ee),
                    run.contains(&Key::Market).then_some(market),
                    target_repo,
                    &pr_ref,
                    &instructions,
                    runner,
                );
                let posts = post_markers(pr, &head_sha, &dispatched, sink);
                for (key, post) in &posts {
                    let KeyPost::Withheld(class, _) = post else {
                        continue;
                    };
                    if counts_toward_cap(*class) {
                        ledger.record(AttemptRecord {
                            repo: target_repo.to_string(),
                            pr,
                            head_sha: head_sha.clone(),
                            key: key.wire().to_string(),
                            class: class.wire().to_string(),
                            at: now,
                        });
                        dirty = true;
                    } else {
                        let kind = backoff_for(*class, dispatched.failure(*key));
                        lock(state.memory).back_off(kind, &slot(*key), now);
                    }
                }
                pass.results.push((pr, posts));
            }
        }
    }
    // Forget closed PRs and moved heads only when this pass read the whole
    // open-PR set: an unreadable PR (or an empty listing, which may be a
    // failed read) must not release a cap (critique A10).
    let complete = unreadable == 0 && !current.is_empty();
    let current = complete.then_some(&current);
    dirty |= ledger.prune(now, current);
    lock(state.memory).prune(target_repo, now, current);
    if dirty {
        save_ledger(state, &ledger);
    }
    pass
}

/// The pass's working ledger: the file (an unreadable one is empty, logged
/// once) plus any records an earlier failed save left in memory. The flag
/// says whether it must be saved even if nothing is recorded (it was
/// unreadable, or memory held records the file lacks).
fn load_ledger(state: &LaneState<'_>) -> (ratification_attempts::Ledger, bool) {
    use ratification_attempts::{lock, Ledger};
    let Some(path) = state.ledger else {
        return (Ledger::default(), false);
    };
    let (mut ledger, mut dirty) = match ratification_attempts::load(path) {
        Ok(l) => (l, false),
        Err(reason) => {
            if warn_once(0, &path.display().to_string(), "ledger-unreadable", &reason) {
                log::warn!("ratification_lane: attempt ledger treated as empty: {reason}");
            }
            (Ledger::default(), true)
        }
    };
    let before = ledger.attempts.len();
    ledger.merge(lock(state.memory).unsaved(path));
    dirty |= ledger.attempts.len() != before;
    (ledger, dirty)
}

/// Save the pass's ledger; on failure keep its records in memory so this
/// host still stops at the cap, and log once.
fn save_ledger(state: &LaneState<'_>, ledger: &ratification_attempts::Ledger) {
    use ratification_attempts::lock;
    let Some(path) = state.ledger else {
        return;
    };
    match ratification_attempts::save(path, ledger) {
        Ok(()) => lock(state.memory).set_unsaved(path, Vec::new()),
        Err(reason) => {
            if warn_once(0, &path.display().to_string(), "ledger-unsaved", &reason) {
                log::warn!("ratification_lane: attempt ledger kept in memory only: {reason}");
            }
            lock(state.memory).set_unsaved(path, ledger.attempts.clone());
        }
    }
}

/// What one lane tick did: a distinct `reason` for every way a tick can end,
/// so "never ran" and "nothing to do" are different records (#13707).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickOutcome {
    /// `disabled` | `repo_unresolved` | `in_flight` | `nothing_to_do` | `held`
    /// | `degraded` | `ran`, in increasing precedence among the last four.
    /// `degraded`: nothing was dispatched but some PRs were unreadable, so
    /// "no ratification PRs here" would be false. `held`: candidates were
    /// selected, all readable, but every missing key was capped or deferred
    /// (#15315), so nothing was dispatched.
    pub reason: &'static str,
    /// Candidates selected for key review, dispatched or held.
    pub selected: usize,
    /// PRs selection refused.
    pub skipped: usize,
    /// Candidates whose key sessions were dispatched.
    pub ran: usize,
    /// Key markers posted.
    pub posted: usize,
    /// PRs not selected because a candidate file's head content was unreadable.
    pub unreadable: usize,
    /// Keys not posted.
    pub withheld: usize,
    /// Missing keys not dispatched because they reached the attempt cap.
    pub capped: usize,
    /// Missing keys not dispatched because of a backoff.
    pub deferred: usize,
    /// One count per fixed class, first for EE and then for market.
    key_counts: [[usize; CLASSES_PER_KEY]; 2],
}

const CLASSES_PER_KEY: usize = 1 + WithheldClass::PostFailed as usize;

impl TickOutcome {
    fn bare(reason: &'static str) -> Self {
        Self {
            reason,
            selected: 0,
            skipped: 0,
            ran: 0,
            posted: 0,
            unreadable: 0,
            withheld: 0,
            capped: 0,
            deferred: 0,
            key_counts: Default::default(),
        }
    }

    /// Summarize a [`run_lane_stats_with`] pass, holds included.
    #[must_use]
    pub fn from_pass(pass: &LanePass) -> Self {
        let mut o = Self::from_run_stats(&pass.results, pass.refused, pass.unreadable);
        o.selected += pass.held;
        o.capped = pass.capped;
        o.deferred = pass.deferred;
        if o.reason == "nothing_to_do" && pass.held > 0 {
            o.reason = "held";
        }
        o
    }

    /// Summarize a [`run_lane_counted`] result.
    #[must_use]
    pub fn from_run(results: &[(u64, Vec<(Key, KeyPost)>)], skipped: usize) -> Self {
        Self::from_run_stats(results, skipped, 0)
    }

    /// [`TickOutcome::from_run`] with the unreadable-PR count.
    #[must_use]
    pub fn from_run_stats(
        results: &[(u64, Vec<(Key, KeyPost)>)],
        skipped: usize,
        unreadable: usize,
    ) -> Self {
        let mut key_counts = [[0; CLASSES_PER_KEY]; 2];
        for (key, post) in results.iter().flat_map(|(_, keys)| keys) {
            let key_index = match key {
                Key::Ee => 0,
                Key::Market => 1,
            };
            let class_index = match post {
                KeyPost::Posted => 0,
                KeyPost::Withheld(class, _) => *class as usize,
            };
            key_counts[key_index][class_index] += 1;
        }
        let posted = key_counts.iter().map(|counts| counts[0]).sum();
        let withheld = key_counts.iter().flatten().sum::<usize>() - posted;
        Self {
            reason: if !results.is_empty() {
                "ran"
            } else if unreadable > 0 {
                "degraded"
            } else {
                "nothing_to_do"
            },
            selected: results.len(),
            skipped,
            ran: results.len(),
            posted,
            unreadable,
            withheld,
            capped: 0,
            deferred: 0,
            key_counts,
        }
    }
}

/// Log the tick and emit its span (when spans are exported on this host).
fn record_tick(root: &std::path::Path, source: &str, o: &TickOutcome) {
    log::info!(
        "ratification_lane: tick root={} source={source} reason={} selected={} skipped={} ran={} posted={} unreadable={} withheld={} capped={} deferred={}",
        root.display(),
        o.reason,
        o.selected,
        o.skipped,
        o.ran,
        o.posted,
        o.unreadable,
        o.withheld,
        o.capped,
        o.deferred
    );
    if crate::observability::ops::spans_exported() {
        crate::observability::ops::emit_span(tick_span(root, source, o, chrono::Utc::now()));
    }
}

/// Span attribute keys on `ratification_lane.tick`. They must be namespaced
/// and registered in `bounded_attributes()`; bare keys are dropped at export.
pub const KEY_REASON: &str = "loom.ratification.reason";
pub const KEY_SOURCE: &str = "loom.ratification.source";
pub const KEY_SELECTED: &str = "loom.ratification.selected";
pub const KEY_SKIPPED: &str = "loom.ratification.skipped";
pub const KEY_RAN: &str = "loom.ratification.ran";
pub const KEY_POSTED: &str = "loom.ratification.posted";
pub const KEY_UNREADABLE: &str = "loom.ratification.unreadable";
pub const KEY_WITHHELD: &str = "loom.ratification.withheld";
/// Missing keys the attempt cap kept from dispatch this tick (#15315).
pub const KEY_CAPPED: &str = "loom.ratification.capped";
/// Missing keys a backoff kept from dispatch this tick (#15315).
pub const KEY_DEFERRED: &str = "loom.ratification.deferred";
const KEY_COUNT_START: usize = 10;
pub const KEYS: [&str; KEY_COUNT_START + 2 * CLASSES_PER_KEY] = [
    KEY_REASON,
    KEY_SOURCE,
    KEY_SELECTED,
    KEY_SKIPPED,
    KEY_RAN,
    KEY_POSTED,
    KEY_UNREADABLE,
    KEY_WITHHELD,
    KEY_CAPPED,
    KEY_DEFERRED,
    "loom.ratification.ee.posted",
    "loom.ratification.ee.pre_launch_failed",
    "loom.ratification.ee.invocation_failed",
    "loom.ratification.ee.output_missing",
    "loom.ratification.ee.verdict_invalid",
    "loom.ratification.ee.verdict_placeholder",
    "loom.ratification.ee.post_failed",
    "loom.ratification.market.posted",
    "loom.ratification.market.pre_launch_failed",
    "loom.ratification.market.invocation_failed",
    "loom.ratification.market.output_missing",
    "loom.ratification.market.verdict_invalid",
    "loom.ratification.market.verdict_placeholder",
    "loom.ratification.market.post_failed",
];

/// The `ratification_lane.tick` span: an instant, its own root.
#[must_use]
pub fn tick_span(
    root: &std::path::Path,
    source: &str,
    o: &TickOutcome,
    at: chrono::DateTime<chrono::Utc>,
) -> crate::telemetry::trace::SpanRecord {
    use crate::telemetry::trace::{
        instant, SpanName, SpanRecord, SpanStatus, TraceAttributes, TraceContext,
    };
    let name = SpanName::RatificationLaneTick;
    let root_s = root.display().to_string();
    let context = TraceContext::derived(name.as_str(), &[&root_s, source, &instant(at)]);
    let mut attributes = TraceAttributes::new();
    let mut put = |k: &str, v: String| {
        attributes.insert(k.to_string(), v);
    };
    put(KEY_REASON, o.reason.to_string());
    put(KEY_SOURCE, source.to_string());
    put(KEY_SELECTED, o.selected.to_string());
    put(KEY_SKIPPED, o.skipped.to_string());
    put(KEY_RAN, o.ran.to_string());
    put(KEY_POSTED, o.posted.to_string());
    put(KEY_UNREADABLE, o.unreadable.to_string());
    put(KEY_WITHHELD, o.withheld.to_string());
    put(KEY_CAPPED, o.capped.to_string());
    put(KEY_DEFERRED, o.deferred.to_string());
    for (key, count) in KEYS[KEY_COUNT_START..]
        .iter()
        .zip(o.key_counts.iter().flatten())
    {
        put(key, count.to_string());
    }
    crate::telemetry::trace::provenance::stamp(&mut attributes);
    SpanRecord {
        context,
        parent_span_id: None,
        name,
        started_at: at,
        ended_at: at,
        status: SpanStatus::Ok,
        attributes,
        events: Vec::new(),
        links: Vec::new(),
    }
}

/// The real forge for [`PrSource`]: `gh api` against `nwo`, trusting only
/// comments `policy` believes.
pub struct GhPrSource<'a> {
    pub root: &'a std::path::Path,
    pub nwo: &'a str,
    pub policy: &'a crate::comment_trust::TrustPolicy,
    refs: std::cell::RefCell<std::collections::HashMap<u64, (String, String)>>,
    /// `(pr, path)` -> why the head content could not be read, this pass.
    failures: std::cell::RefCell<std::collections::HashMap<(u64, String), String>>,
}

impl<'a> GhPrSource<'a> {
    #[must_use]
    pub fn new(
        root: &'a std::path::Path,
        nwo: &'a str,
        policy: &'a crate::comment_trust::TrustPolicy,
    ) -> Self {
        Self {
            root,
            nwo,
            policy,
            refs: std::cell::RefCell::default(),
            failures: std::cell::RefCell::default(),
        }
    }

    /// `endpoint`'s paginated listing, with the reason it could not be read.
    /// `--paginate` reads every page (#13502); `parse_listing` flattens the
    /// concatenated arrays. Any failed or unparseable read is an `Err` with
    /// the bounded `gh` failure detail.
    fn get_detailed(&self, endpoint: &str) -> Result<Value, String> {
        let gh = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
        let inv = crate::claim_reconciliation::gh_call::read("ratification.pr_get", &gh, self.root)
            .args(["api", endpoint, "--paginate"]);
        match inv.run() {
            crate::cmd_out::CmdOutcome::Ran(out) if out.status.success() => {
                crate::comment_trust::parse_listing(&out.stdout)
                    .map(Value::Array)
                    .ok_or_else(|| "listing did not parse".to_string())
            }
            other => Err(super::ratification_dispatch::gh_failure_detail(&other)),
        }
    }

    /// [`Self::get_detailed`], fail-open: `None` on any failure.
    fn get(&self, endpoint: &str) -> Option<Value> {
        self.get_detailed(endpoint).ok()
    }

    fn raw(&self, path: &str, git_ref: &str) -> Result<String, String> {
        let endpoint = format!("repos/{}/contents/{path}?ref={git_ref}", self.nwo);
        super::ratification_dispatch::read_raw(self.root, &endpoint)
    }
}

impl PrSource for GhPrSource<'_> {
    /// Every open PR; an unreadable list is "no candidates".
    fn open_prs(&self) -> Vec<OpenPr> {
        let Some(Value::Array(list)) =
            self.get(&format!("repos/{}/pulls?state=open&per_page=100", self.nwo))
        else {
            return Vec::new();
        };
        let mut prs = Vec::new();
        for p in &list {
            let (Some(number), Some(author), Some(head), Some(base)) = (
                p.get("number").and_then(Value::as_u64),
                p.pointer("/user/login").and_then(Value::as_str),
                p.pointer("/head/sha").and_then(Value::as_str),
                p.pointer("/base/sha").and_then(Value::as_str),
            ) else {
                continue;
            };
            self.refs
                .borrow_mut()
                .insert(number, (head.to_string(), base.to_string()));
            prs.push(OpenPr {
                number,
                author: author.to_string(),
                head_sha: head.to_string(),
            });
        }
        prs
    }

    /// Head content is read only for paths the hold would scan.
    fn files(&self, pr: u64, hold: &ratification::Config) -> Vec<ChangedFile> {
        let Some(Value::Array(list)) =
            self.get(&format!("repos/{}/pulls/{pr}/files?per_page=100", self.nwo))
        else {
            return Vec::new();
        };
        let (head, _base) = self.refs.borrow().get(&pr).cloned().unwrap_or_default();
        list.iter()
            .filter_map(|f| {
                let mut file = ChangedFile {
                    filename: f.get("filename").and_then(Value::as_str)?.to_string(),
                    previous_filename: f
                        .get("previous_filename")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    status: f.get("status").and_then(Value::as_str)?.to_string(),
                    head_content: None,
                };
                if file.status != "removed"
                    && !head.is_empty()
                    && ratification::is_candidate(&file, hold)
                {
                    match self.raw(&file.filename, &head) {
                        Ok(text) => file.head_content = Some(text),
                        Err(why) => {
                            self.failures
                                .borrow_mut()
                                .insert((pr, file.filename.clone()), why);
                        }
                    }
                }
                Some(file)
            })
            .collect()
    }

    fn read_failure(&self, pr: u64, path: &str) -> Option<String> {
        self.failures.borrow().get(&(pr, path.to_string())).cloned()
    }

    fn trusted_comments(&self, pr: u64) -> Result<Vec<Comment>, String> {
        let list = self
            .get_detailed(&format!("repos/{}/issues/{pr}/comments?per_page=100", self.nwo))
            .map_err(|why| format!("comments could not be read: {why}"))?;
        let Value::Array(list) = list else {
            return Err("comments could not be read: listing was not an array".to_string());
        };
        // Incomplete history cannot prove a marker is absent. Check every
        // record before filtering by trust, even on later pages.
        const MALFORMED: &str = "comments could not be read: malformed comment record";
        let mut comments = Vec::new();
        for row in &list {
            let body = row.get("body").and_then(Value::as_str).ok_or(MALFORMED)?;
            if row.get("user") == Some(&Value::Null) {
                // A deleted author is a readable record with no marker authority.
                continue;
            }
            let author = row
                .pointer("/user/login")
                .and_then(Value::as_str)
                .filter(|login| !login.trim().is_empty())
                .ok_or(MALFORMED)?;
            if self.policy.trusts_json(row) {
                comments.push(Comment::new(author, body));
            }
        }
        Ok(comments)
    }
}

// ---------------------------------------------------------------------------
// Read-only status report (#14834)
// ---------------------------------------------------------------------------

/// One **trusted** PR comment with the forge identity the report links by:
/// [`PrSource::trusted_comments`] discards id, creation time and link after
/// the trust filter; the status report needs them back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerComment {
    /// The forge-assigned comment id — the tie-breaker between equal
    /// creation times (newer ids are newer comments).
    pub id: u64,
    /// The forge comment's creation time: the **only** ordering signal that
    /// counts. An edit, a date written inside the review, or a more
    /// favourable verdict never re-dates or re-ranks a comment (#14834).
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// `html_url`: the link the report prints.
    pub url: String,
    pub body: String,
}

impl MarkerComment {
    /// From a REST issue-comment record. `None` when the body or an identity
    /// field the report needs (id, creation time, link) is missing or
    /// malformed; the caller treats that as an unreadable history, the same
    /// fail-closed contract as a malformed record in
    /// [`PrSource::trusted_comments`].
    #[must_use]
    pub fn from_record(row: &Value) -> Option<Self> {
        Some(MarkerComment {
            id: row.get("id").and_then(Value::as_u64)?,
            created_at: row
                .get("created_at")
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())?
                .with_timezone(&chrono::Utc),
            url: row.get("html_url").and_then(Value::as_str)?.to_string(),
            body: row.get("body").and_then(Value::as_str)?.to_string(),
        })
    }
}

/// The two wire shapes a key marker appears in (#14834). Both compete for
/// "effective" on exactly the same terms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkerFormat {
    /// The lane's own `<!-- ratify:key=<k> sha=<40-hex> -->` first line.
    Lane,
    /// The hand-applied `<!-- RATIFY-KEY: <k> verdict=… block=… sha=… -->`
    /// comment.
    Hand,
}

impl MarkerFormat {
    /// The name the report prints beside a link.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MarkerFormat::Lane => "ratify:key",
            MarkerFormat::Hand => "RATIFY-KEY",
        }
    }
}

/// One key marker found in a comment body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerFound {
    pub key: Key,
    /// The full 40-hex head the marker names.
    pub sha: String,
    pub format: MarkerFormat,
    /// The `block=owner/repo#<n>` PR identity the marker carries, when it has
    /// one. A block naming another repo or PR disqualifies the marker for
    /// this PR.
    pub block: Option<String>,
}

/// Every standalone key marker in `body` outside indented or fenced code
/// and block quotes (#14834). A hand marker must carry a full 40-hex `sha` attribute.
#[must_use]
pub fn scan_markers(body: &str) -> Vec<MarkerFound> {
    let lane_re = Regex::new(r"^<!--\s*ratify:key=(ee|market)\s+sha=([0-9a-fA-F]{40})\s*-->$")
        .expect("static");
    let hand_re = Regex::new(r"^<!--\s*RATIFY-KEY:\s*(ee|market)((?:\s+[a-z-]+=[^\s<>]+)+)\s*-->$")
        .expect("static");
    let mut out = Vec::new();
    let visible = blank_block_code(body);
    for (line, visible_line) in body.lines().zip(visible.lines()) {
        let after_spaces = line.trim_start_matches(' ');
        // Match original standalone lines only. The mask cannot create one.
        if line != visible_line
            || line.len() - after_spaces.len() >= 4
            || after_spaces.starts_with('\t')
        {
            continue;
        }
        let t = line.trim();
        if let Some(cap) = lane_re.captures(t) {
            out.push(MarkerFound {
                key: wire_key(&cap[1]),
                sha: cap[2].to_string(),
                format: MarkerFormat::Lane,
                block: None,
            });
        } else if let Some(cap) = hand_re.captures(t) {
            let mut sha = None;
            let mut block = None;
            for attr in cap[2].split_whitespace() {
                let Some((name, value)) = attr.split_once('=') else {
                    continue;
                };
                match name {
                    "sha" => sha = Some(value.to_string()),
                    "block" => block = Some(value.to_string()),
                    _ => {}
                }
            }
            let Some(sha) = sha.filter(|s| is_full_sha(s)) else {
                continue;
            };
            out.push(MarkerFound {
                key: wire_key(&cap[1]),
                sha,
                format: MarkerFormat::Hand,
                block,
            });
        }
    }
    out
}

fn is_full_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `ee`/`market` from a marker capture.
fn wire_key(name: &str) -> Key {
    if name == "ee" {
        Key::Ee
    } else {
        Key::Market
    }
}

/// The link the report prints for one marker-carrying comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerLink {
    pub comment_id: u64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub url: String,
    pub format: MarkerFormat,
}

impl MarkerLink {
    fn of(comment: &MarkerComment, format: MarkerFormat) -> Self {
        MarkerLink {
            comment_id: comment.id,
            created_at: comment.created_at,
            url: comment.url.clone(),
            format,
        }
    }
}

/// What the report says about one key at one head (#14834).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyStatus {
    pub key: Key,
    /// The **effective** marker: the newest trusted marker naming exactly the
    /// head — the review to read first. `None` when a complete read found
    /// none: the key is missing at this head.
    pub effective: Option<MarkerLink>,
    /// Every other eligible marker comment at the same head, oldest first —
    /// the superseded duplicates the pre-#14800 retry pileup posted. Each
    /// comment is linked once; nothing is deleted or edited.
    pub superseded: Vec<MarkerLink>,
}

/// The whole report: one row per key for the PR's current head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneStatus {
    pub head_sha: String,
    pub ee: KeyStatus,
    pub market: KeyStatus,
}

/// Resolve the report for `head_sha` from the PR's **complete, trusted**
/// comment history (`MarkerComment`s; the caller applied the same trust
/// policy as every marker read). A marker is eligible when it names the exact
/// head (a full 40-hex SHA), and its `block=` identity — when it carries one
/// — names exactly this PR. Newest by forge comment creation time wins, the
/// numeric comment id breaks ties, and both formats compete on the same
/// terms. A marker naming another head or another PR is neither effective nor
/// superseded: it is not a marker for this head.
#[must_use]
pub fn resolve_status(
    head_sha: &str,
    nwo: &str,
    pr: u64,
    comments: &[MarkerComment],
) -> LaneStatus {
    let block_id = format!("{nwo}#{pr}");
    let row = |key: Key| {
        let mut eligible: Vec<MarkerLink> = comments
            .iter()
            .filter_map(|c| {
                let marker = scan_markers(&c.body).into_iter().find(|m| {
                    m.key == key
                        && m.sha.eq_ignore_ascii_case(head_sha)
                        && m.block.as_deref().is_none_or(|b| b == block_id)
                })?;
                Some(MarkerLink::of(c, marker.format))
            })
            .collect();
        eligible.sort_by_key(|m| (m.created_at, m.comment_id));
        let effective = eligible.pop();
        KeyStatus {
            key,
            effective,
            superseded: eligible,
        }
    };
    LaneStatus {
        head_sha: head_sha.to_string(),
        ee: row(Key::Ee),
        market: row(Key::Market),
    }
}

impl GhPrSource<'_> {
    /// The PR's current head SHA, or why it could not be read (#14834).
    pub fn pr_head(&self, pr: u64) -> Result<String, String> {
        let endpoint = format!("repos/{}/pulls/{pr}", self.nwo);
        let gh = std::path::PathBuf::from(crate::gh_invocation::gh_bin());
        let inv =
            crate::claim_reconciliation::gh_call::read("ratification.pr_head", &gh, self.root)
                .args(["api", &endpoint]);
        match inv.run() {
            crate::cmd_out::CmdOutcome::Ran(out) if out.status.success() => {
                let record: Value = serde_json::from_slice(&out.stdout)
                    .map_err(|_| "PR record was not valid JSON".to_string())?;
                let sha = record
                    .pointer("/head/sha")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "head sha missing from the PR record".to_string())?;
                if !is_full_sha(sha) {
                    return Err("head sha in the PR record is not a full 40-hex SHA".to_string());
                }
                Ok(sha.to_string())
            }
            other => Err(super::ratification_dispatch::gh_failure_detail(&other)),
        }
    }

    /// The PR's complete issue-comment history as [`MarkerComment`]s —
    /// trusted authors only, identity retained. `Err` when the listing or any
    /// readable record is unreadable: the same fail-closed contract as
    /// [`PrSource::trusted_comments`] (#14797), because an incomplete
    /// history cannot prove which marker is effective. A successfully read
    /// empty history is `Ok(vec![])`.
    pub fn marker_comments(&self, pr: u64) -> Result<Vec<MarkerComment>, String> {
        let list = self
            .get_detailed(&format!("repos/{}/issues/{pr}/comments?per_page=100", self.nwo))
            .map_err(|why| format!("comments could not be read: {why}"))?;
        let Value::Array(list) = list else {
            return Err("comments could not be read: listing was not an array".to_string());
        };
        const MALFORMED: &str = "comments could not be read: malformed comment record";
        let mut out = Vec::new();
        for row in &list {
            if row.get("body").and_then(Value::as_str).is_none() {
                return Err(MALFORMED.to_string());
            }
            if row.get("user") == Some(&Value::Null) {
                // A deleted author is a readable record with no marker authority.
                continue;
            }
            if !row
                .pointer("/user/login")
                .and_then(Value::as_str)
                .is_some_and(|login| !login.trim().is_empty())
            {
                return Err(MALFORMED.to_string());
            }
            if !self.policy.trusts_json(row) {
                continue;
            }
            let comment = MarkerComment::from_record(row).ok_or(MALFORMED.to_string())?;
            out.push(comment);
        }
        Ok(out)
    }
}

/// The real [`CommentSink`]: `forge_comment::post_comment` as the fleet identity.
pub struct GhCommentSink<'a> {
    pub root: &'a std::path::Path,
    pub nwo: &'a str,
}

impl CommentSink for GhCommentSink<'_> {
    fn post(&self, pr: u64, body: &str) -> Result<(), String> {
        crate::forge_comment::post_comment(
            crate::gh_invocation::gh_bin(),
            Some(self.root),
            self.nwo,
            pr,
            true,
            body,
        )
        .map(|_| ())
    }
}

/// Adapts a (possibly unsized) runner reference to the sized `dyn` the lane takes.
struct RunnerRef<'r, R: super::RoleInvocationRunner + ?Sized>(&'r mut R);

impl<R: super::RoleInvocationRunner + ?Sized> super::RoleInvocationRunner for RunnerRef<'_, R> {
    fn invoke(&mut self, role: &str, prompt: &str) -> super::RoleTickOutcome {
        self.0.invoke(role, prompt)
    }
    fn last_output(&self) -> Option<String> {
        self.0.last_output()
    }
    fn unlaunched_failure(&self) -> Option<crate::observability::lifecycle::RoleFailure> {
        self.0.unlaunched_failure()
    }
    /// Forwarded so the lane sees a launched child's failure class (#15315).
    fn trace_context(&self) -> Option<crate::observability::lifecycle::RoleTrace> {
        self.0.trace_context()
    }
}

/// Roots with a lane tick in flight, process-wide: the role-tick and
/// step-dispatch entry points share it, so a root is ticked by at most one
/// path at a time (#13707).
fn in_flight() -> &'static std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>> {
    static SET: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashSet<std::path::PathBuf>>,
    > = std::sync::OnceLock::new();
    SET.get_or_init(Default::default)
}

struct InFlight(std::path::PathBuf);

impl InFlight {
    fn acquire(root: &std::path::Path) -> Option<Self> {
        let mut set = in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        set.insert(root.to_path_buf())
            .then(|| Self(root.to_path_buf()))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        in_flight()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.0);
    }
}

/// The role-tick entry point: does nothing unless `ratificationLane.enabled`
/// is set for `root`, and fails closed when the repo cannot be resolved. The
/// lane's own session id (`LOOM_SWEEP_ID`) feeds the builder-session check.
/// Every tick, including a no-op one, logs and emits its outcome (#13707).
pub fn tick_for_root<R: super::RoleInvocationRunner + ?Sized>(
    root: &std::path::Path,
    runner: &mut R,
) -> TickOutcome {
    tick_for_root_from(root, runner, "role_tick")
}

fn tick_for_root_from<R: super::RoleInvocationRunner + ?Sized>(
    root: &std::path::Path,
    runner: &mut R,
    source: &str,
) -> TickOutcome {
    let outcome = tick_inner(root, runner);
    record_tick(root, source, &outcome);
    outcome
}

fn tick_inner<R: super::RoleInvocationRunner + ?Sized>(
    root: &std::path::Path,
    runner: &mut R,
) -> TickOutcome {
    let effective = crate::config_resolver::resolve_effective_config(root);
    let cfg = Config::from_effective(&effective);
    if !cfg.enabled {
        return TickOutcome::bare("disabled");
    }
    let policy = crate::comment_trust::TrustPolicy::for_root(root);
    let Some(nwo) = crate::credential_preflight::nwo_from_git_remote(root) else {
        log::warn!("ratification_lane: repo unresolved; skipping");
        return TickOutcome::bare("repo_unresolved");
    };
    let Some(_guard) = InFlight::acquire(root) else {
        return TickOutcome::bare("in_flight");
    };
    let lane_session = std::env::var("LOOM_SWEEP_ID")
        .ok()
        .filter(|s| !s.is_empty());
    let prs = GhPrSource::new(root, &nwo, &policy);
    let instructions = super::ratification_dispatch::GhInstructionSource { root };
    let sink = GhCommentSink { root, nwo: &nwo };
    let ledger = ratification_attempts::ledger_path(root);
    let state = LaneState {
        memory: ratification_attempts::process_memory(),
        ledger: Some(&ledger),
        now: chrono::Utc::now(),
    };
    let pass = run_lane_stats_with(
        &cfg,
        lane_session.as_deref(),
        &nwo,
        &prs,
        &instructions,
        &sink,
        &mut RunnerRef(runner),
        &state,
    );
    TickOutcome::from_pass(&pass)
}

/// How often the step-dispatch driver ticks the lane for a root.
pub const STEP_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether `root` is due a step-dispatch tick at `now`, recording it as ticked
/// when so. Pure over `last` so the cadence is testable.
fn step_tick_due(
    last: &mut std::collections::HashMap<std::path::PathBuf, std::time::Instant>,
    root: &std::path::Path,
    now: std::time::Instant,
    interval: std::time::Duration,
) -> bool {
    match last.get(root) {
        Some(t) if now.duration_since(*t) < interval => false,
        _ => {
            last.insert(root.to_path_buf(), now);
            true
        }
    }
}

#[cfg(test)]
pub(crate) fn step_tick_due_for_test(
    last: &mut std::collections::HashMap<std::path::PathBuf, std::time::Instant>,
    root: &std::path::Path,
    now: std::time::Instant,
) -> bool {
    step_tick_due(last, root, now, STEP_TICK_INTERVAL)
}

/// The step-dispatch entry point (#13707): a fleet host that dispatches by
/// steps never reaches the champion role tick, so the driver calls this each
/// tick. Per root, at most once per [`STEP_TICK_INTERVAL`], on its own thread
/// (the lane shells out and runs reviewer sessions, which must not stall the
/// driver tick). The shared in-flight guard stops it overlapping a role tick.
pub fn spawn_step_ticks(roots: &[std::path::PathBuf]) {
    static LAST: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<std::path::PathBuf, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    let last = LAST.get_or_init(Default::default);
    for root in roots {
        let due = {
            let mut m = last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            step_tick_due(&mut m, root, std::time::Instant::now(), STEP_TICK_INTERVAL)
        };
        if !due {
            continue;
        }
        let root = root.clone();
        let spawned = std::thread::Builder::new()
            .name("ratification-lane".into())
            .spawn(move || {
                let mut runner = super::ScriptRoleInvocationRunner::new(root.clone());
                tick_for_root_from(&root, &mut runner, "step_dispatch");
            });
        if let Err(e) = spawned {
            log::warn!("ratification_lane: could not spawn step tick: {e}");
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn production_adapter_preserves_typed_launch_evidence() {
        use crate::observability::lifecycle::{pre_launch, RoleFailure, UNCLASSIFIED_PRE_LAUNCH};
        use crate::role_runner::{
            ratification_dispatch::InstructionSource, RoleInvocationRunner, RoleTickOutcome,
        };
        use std::cell::{Cell, RefCell};

        #[derive(Default)]
        struct Inputs {
            instruction_repos: RefCell<Vec<String>>,
            visibility_reads: Cell<usize>,
        }

        impl InstructionSource for Inputs {
            fn fetch(&self, pin: &Pin) -> Result<String, String> {
                self.instruction_repos.borrow_mut().push(pin.repo.clone());
                Ok("fixture instructions".into())
            }

            fn repo_is_private(&self, _repo: &str) -> Option<bool> {
                self.visibility_reads.set(self.visibility_reads.get() + 1);
                Some(false)
            }
        }

        #[derive(Default)]
        struct Sink(RefCell<Vec<(u64, String)>>);

        impl CommentSink for Sink {
            fn post(&self, pr: u64, body: &str) -> Result<(), String> {
                self.0.borrow_mut().push((pr, body.into()));
                Ok(())
            }
        }

        struct Runner {
            evidence: &'static str,
            calls: Vec<String>,
        }

        impl RoleInvocationRunner for Runner {
            fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
                self.calls.push(role.into());
                if role == "ratification-ee" {
                    RoleTickOutcome::Failure("fixture failure".into())
                } else {
                    RoleTickOutcome::Success
                }
            }

            fn last_output(&self) -> Option<String> {
                self.calls
                    .last()
                    .filter(|role| *role == "ratification-market")
                    .map(|_| {
                        "## Market-key review: fixture\n**Overall verdict:** uncompetitive\n".into()
                    })
            }

            fn unlaunched_failure(&self) -> Option<RoleFailure> {
                // The Market call replaces the evidence from EE, as the production runner does.
                self.calls
                    .last()
                    .filter(|role| *role == "ratification-ee")
                    .map(|_| RoleFailure::new(self.evidence, None, "fixture-only typed detail"))
            }
        }

        let mut cfg = enabled();
        cfg.ee.as_mut().unwrap().commit = SHA.into();
        cfg.market.as_mut().unwrap().commit = SHA.into();
        let prs = one_pr("alice", request(), vec![]);
        for (evidence, counts) in [
            (UNCLASSIFIED_PRE_LAUNCH, ("0", "1", "1")),
            (pre_launch::WRITE_SCOPE_REFUSED.class, ("1", "0", "1")),
        ] {
            let inputs = Inputs::default();
            let sink = Sink::default();
            let mut runner = Runner {
                evidence,
                calls: vec![],
            };
            let mut adapter = RunnerRef(&mut runner as &mut dyn RoleInvocationRunner);
            let (results, skipped, unreadable) =
                run_lane_stats(&cfg, None, "fixture/example", &prs, &inputs, &sink, &mut adapter);
            assert_eq!(inputs.instruction_repos.borrow().as_slice(), ["o/ee", "o/mk"]);
            assert_eq!(inputs.visibility_reads.get(), 1);
            assert_eq!(runner.calls, ["ratification-ee", "ratification-market"]);
            let posts = sink.0.borrow();
            assert_eq!(posts.len(), 1);
            assert_eq!(posts[0].0, 7);
            assert!(posts[0]
                .1
                .starts_with(&format!("<!-- ratify:key=market sha={SHA} -->")));
            assert!(posts[0].1.contains("**Overall verdict:** uncompetitive"));

            let tick = TickOutcome::from_run_stats(&results, skipped, unreadable);
            assert_eq!((tick.posted, tick.withheld, tick.unreadable), (1, 1, 0));
            let span =
                tick_span(std::path::Path::new("/fixture-only"), "test", &tick, chrono::Utc::now())
                    .bounded();
            let count = |key: &str| span.attributes.get(key).unwrap().as_str();
            assert_eq!(
                (
                    count("loom.ratification.ee.pre_launch_failed"),
                    count("loom.ratification.ee.invocation_failed"),
                    count("loom.ratification.market.posted"),
                ),
                counts,
                "typed evidence: {evidence}"
            );
            assert!(span
                .attributes
                .values()
                .all(|value| !value.contains("fixture-only typed detail")));
        }
    }

    #[test]
    fn runner_ref_forwards_trace_context() {
        use crate::observability::lifecycle::{RoleFailure, RoleTrace};
        use crate::role_runner::{RoleInvocationRunner, RoleTickOutcome};

        #[derive(Default)]
        struct Sink;
        impl CommentSink for Sink {
            fn post(&self, _pr: u64, _body: &str) -> Result<(), String> {
                Ok(())
            }
        }

        /// A launched EE child whose trace names a launch-refusal class; its
        /// output carries no rejection marker, so only the forwarded trace
        /// can classify it.
        struct Traced {
            last_ee: bool,
        }
        impl RoleInvocationRunner for Traced {
            fn invoke(&mut self, role: &str, _prompt: &str) -> RoleTickOutcome {
                self.last_ee = role == "ratification-ee";
                RoleTickOutcome::Failure("exit 78".into())
            }
            fn trace_context(&self) -> Option<RoleTrace> {
                self.last_ee.then(|| RoleTrace {
                    context: crate::telemetry::trace::TraceContext::derived("fixture", &["t"]),
                    execution: "fixture".into(),
                    started_at: chrono::Utc::now(),
                    failure: Some(RoleFailure::new("preflight-no-cli-start", Some(78), "x")),
                    judge: None,
                })
            }
        }

        struct Instr;
        impl crate::role_runner::ratification_dispatch::InstructionSource for Instr {
            fn fetch(&self, _pin: &Pin) -> Result<String, String> {
                Ok("fixture instructions".into())
            }
            fn repo_is_private(&self, _repo: &str) -> Option<bool> {
                Some(true)
            }
        }

        let mut cfg = enabled();
        cfg.ee.as_mut().unwrap().commit = SHA.into();
        cfg.market.as_mut().unwrap().commit = SHA.into();
        let prs = one_pr("alice", request(), vec![]);
        let mut runner = Traced { last_ee: false };
        let mut adapter = RunnerRef(&mut runner as &mut dyn RoleInvocationRunner);
        let (results, _, _) =
            run_lane_stats(&cfg, None, "fixture/example", &prs, &Instr, &Sink, &mut adapter);
        assert!(
            matches!(
                results[0].1.as_slice(),
                [
                    (Key::Ee, KeyPost::Withheld(WithheldClass::PreLaunchFailed, _)),
                    (Key::Market, KeyPost::Withheld(WithheldClass::InvocationFailed, _)),
                ]
            ),
            "{results:?}"
        );
    }

    fn file(name: &str, head: &str) -> ChangedFile {
        ChangedFile {
            filename: name.into(),
            previous_filename: None,
            status: "modified".into(),
            head_content: Some(head.into()),
        }
    }

    const DECLARES: &str = "This record is ratified on merge.";

    #[derive(Default)]
    struct Fake {
        prs: Vec<OpenPr>,
        files: HashMap<u64, Vec<ChangedFile>>,
        comments: HashMap<u64, Vec<Comment>>,
    }

    impl PrSource for Fake {
        fn open_prs(&self) -> Vec<OpenPr> {
            self.prs.clone()
        }
        fn files(&self, pr: u64, _hold: &ratification::Config) -> Vec<ChangedFile> {
            self.files.get(&pr).cloned().unwrap_or_default()
        }
        fn trusted_comments(&self, pr: u64) -> Result<Vec<Comment>, String> {
            Ok(self.comments.get(&pr).cloned().unwrap_or_default())
        }
    }

    fn enabled() -> Config {
        Config::from_effective(&json!({"ratificationLane": {
            "enabled": true,
            "ee": {"repo": "o/ee", "path": "i.md", "commit": "abc"},
            "market": {"repo": "o/mk", "path": "i.md", "commit": "def"}
        }}))
    }

    fn one_pr(author: &str, files: Vec<ChangedFile>, comments: Vec<Comment>) -> Fake {
        let mut f = Fake::default();
        f.prs.push(OpenPr {
            number: 7,
            author: author.into(),
            head_sha: SHA.into(),
        });
        f.files.insert(7, files);
        f.comments.insert(7, comments);
        f
    }

    fn request() -> Vec<ChangedFile> {
        vec![file("ratification/requests/fft.md", DECLARES)]
    }

    #[test]
    fn config_unset_or_malformed_is_disabled() {
        assert_eq!(Config::from_effective(&json!({})), Config::disabled());
        assert_eq!(Config::from_effective(&json!({"ratificationLane": "yes"})), Config::disabled());
        assert_eq!(
            Config::from_effective(&json!({"ratificationLane": {"enabled": "true"}})),
            Config::disabled()
        );
        // enabled but a pin is missing: still off
        assert_eq!(
            Config::from_effective(&json!({"ratificationLane": {"enabled": true,
                "ee": {"repo": "o/ee", "path": "p", "commit": "c"}}})),
            Config::disabled()
        );
        // explicit disable
        assert!(!Config::from_effective(&json!({"ratificationLane": {"enabled": false}})).enabled);
    }

    #[test]
    fn config_parses_pins() {
        let c = enabled();
        assert!(c.enabled);
        assert_eq!(c.ee.unwrap().repo, "o/ee");
        assert_eq!(c.market.unwrap().commit, "def");
    }

    #[test]
    fn disabled_lane_returns_nothing() {
        let src = one_pr("alice", request(), vec![]);
        assert!(find_candidates(&Config::disabled(), None, &src).is_empty());
    }

    #[test]
    fn request_file_is_candidate() {
        let src = one_pr("alice", request(), vec![]);
        assert_eq!(
            find_candidates(&enabled(), None, &src),
            vec![Selection::Candidate {
                pr: 7,
                head_sha: SHA.into(),
                missing: vec![Key::Ee, Key::Market]
            }]
        );
    }

    #[test]
    fn selection_is_the_holds_predicate() {
        let h = ratification::Config::default();
        // Status line with a ratification clause, and an additive record with a phrase.
        let status = vec![file(
            "spec/fft.md",
            "Status: Proposed\nRatification needs operator approval.",
        )];
        assert!(matches_pr(1, &status, &h));
        let record = vec![file("decisions/7.md", "Ratified by the operator on merge.")];
        assert!(matches_pr(1, &record, &h));
        // A bare status line, a non-candidate path, or no declaration: not selected.
        assert!(!matches_pr(1, &[file("spec/fft.md", "Status: Ratified")], &h));
        assert!(!matches_pr(1, &[file("notes/x.md", DECLARES)], &h));
        assert!(!matches_pr(1, &[file("spec/x.md", "nothing here")], &h));
        // The hold's own path extension applies.
        let ext = ratification::Config {
            paths: vec!["chips/**".into()],
            phrases: vec![],
        };
        assert!(matches_pr(1, &[file("chips/a.md", DECLARES)], &ext));
        assert!(!matches_pr(1, &[file("chips/a.md", DECLARES)], &h));
    }

    #[test]
    fn unreadable_candidate_is_not_selected() {
        let mut f = file("spec/fft.md", "");
        f.head_content = None;
        assert!(!matches_pr(1, &[f], &ratification::Config::default()));
    }

    /// Fake whose one PR has an unreadable file with a recorded failure.
    struct Broken(Fake);

    impl PrSource for Broken {
        fn open_prs(&self) -> Vec<OpenPr> {
            self.0.open_prs()
        }
        fn files(&self, pr: u64, hold: &ratification::Config) -> Vec<ChangedFile> {
            self.0.files(pr, hold)
        }
        fn trusted_comments(&self, pr: u64) -> Result<Vec<Comment>, String> {
            self.0.trusted_comments(pr)
        }
        fn read_failure(&self, _pr: u64, path: &str) -> Option<String> {
            Some(format!("exit 1: HTTP 404 for {path}"))
        }
    }

    #[test]
    fn unreadable_pr_is_counted_not_silently_dropped() {
        let mut f = file("spec/fft.md", "");
        f.head_content = None;
        let src = Broken(one_pr("alice", vec![f], vec![]));
        let (sel, unreadable) = find_candidates_counted(&enabled(), None, &src);
        assert!(sel.is_empty());
        assert_eq!(unreadable, 1);
    }

    #[test]
    fn warn_dedup_logs_once_per_head_and_again_on_a_new_reason() {
        let mut d = WarnDedup::default();
        assert!(d.should_log(7, "aaa", "unreadable", "x (exit 1)"));
        assert!(!d.should_log(7, "aaa", "unreadable", "x (exit 1)"), "same tick repeat");
        assert!(d.should_log(7, "bbb", "unreadable", "x (exit 1)"), "new head");
        assert!(d.should_log(7, "aaa", "withheld:ee", "x (exit 1)"), "other kind");
        assert!(d.should_log(7, "aaa", "unreadable", "x (exit 4)"), "reason changed");
    }

    #[test]
    fn removed_request_does_not_match() {
        let mut f = file("ratification/requests/fft.md", DECLARES);
        f.status = "removed".into();
        assert!(!matches_pr(1, &[f], &ratification::Config::default()));
    }

    #[test]
    fn non_matching_pr_is_not_selected() {
        let src = one_pr("alice", vec![file("src/lib.rs", "fn main(){}")], vec![]);
        assert!(find_candidates(&enabled(), None, &src).is_empty());
    }

    fn marker(key: &str, sha: &str) -> Comment {
        Comment::new("loom[bot]", &format!("<!-- ratify:key={key} sha={sha} -->"))
    }

    #[test]
    fn both_keys_for_current_head_skips() {
        let src = one_pr("alice", request(), vec![marker("ee", SHA), marker("market", SHA)]);
        assert!(find_candidates(&enabled(), None, &src).is_empty());
    }

    #[test]
    fn one_key_or_stale_head_still_candidate() {
        let one = one_pr("alice", request(), vec![marker("ee", SHA)]);
        assert_eq!(
            find_candidates(&enabled(), None, &one),
            vec![Selection::Candidate {
                pr: 7,
                head_sha: SHA.into(),
                missing: vec![Key::Market]
            }]
        );
        let stale = "f".repeat(40);
        let old = one_pr("alice", request(), vec![marker("ee", &stale), marker("market", &stale)]);
        assert_eq!(
            find_candidates(&enabled(), None, &old),
            vec![Selection::Candidate {
                pr: 7,
                head_sha: SHA.into(),
                missing: vec![Key::Ee, Key::Market]
            }]
        );
    }

    #[test]
    fn unreadable_comment_list_is_counted_not_read_as_empty() {
        /// Fake whose comment read always fails (#14797).
        struct Deaf(Fake);
        impl PrSource for Deaf {
            fn open_prs(&self) -> Vec<OpenPr> {
                self.0.open_prs()
            }
            fn files(&self, pr: u64, hold: &ratification::Config) -> Vec<ChangedFile> {
                self.0.files(pr, hold)
            }
            fn trusted_comments(&self, _pr: u64) -> Result<Vec<Comment>, String> {
                Err("exit 1: HTTP 502".into())
            }
        }
        let src = Deaf(one_pr("alice", request(), vec![]));
        let (sel, unreadable) = find_candidates_counted(&enabled(), None, &src);
        assert!(sel.is_empty(), "an unreadable list authorizes no rerun");
        assert_eq!(unreadable, 1);
    }

    #[test]
    fn fleet_authored_pr_is_selected() {
        // Author equals the dispatching identity: no longer a reason to refuse.
        let src = one_pr("loom-bot[bot]", request(), vec![]);
        assert_eq!(
            find_candidates(&enabled(), Some("lane-1"), &src),
            vec![Selection::Candidate {
                pr: 7,
                head_sha: SHA.into(),
                missing: vec![Key::Ee, Key::Market]
            }]
        );
    }

    #[test]
    fn lane_session_that_built_the_pr_is_refused() {
        let lease = Comment::new(
            "loom-bot[bot]",
            "<!-- loom:pr-lease-released role=builder host=h sweep=step-builder-9-1 -->",
        );
        let src = one_pr("loom-bot[bot]", request(), vec![lease]);
        let got = find_candidates(&enabled(), Some("step-builder-9-1"), &src);
        match got.as_slice() {
            [Selection::Refused { pr: 7, reason }] => assert!(reason.contains("built the PR")),
            other => panic!("expected refusal, got {other:?}"),
        }
        // A different lane session is fine.
        assert_eq!(find_candidates(&enabled(), Some("lane-1"), &src).len(), 1);
    }
}
