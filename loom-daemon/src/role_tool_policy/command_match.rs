//! Tool-call-time capability matching for `role-tool-policy check` (#8256).
//!
//! The spawn-time half of the per-role restriction ([`super::deny_specs_for`])
//! is a list of Claude Code `--disallowedTools` specs. Those are prefix
//! globs over the Bash tool's command text, so `bash -c 'ssh …'`, `env ssh …`
//! or a compound command walks straight past them, and the Codex CLI has no
//! such flag at all. This module is the **backstop** the guard hooks consult
//! on every tool call: it maps one Bash command line (or one Edit/Write
//! target path) to the sensitive capabilities it would exercise, so
//! [`super::RoleToolPolicy::check_command`] can deny the ones the acting
//! role's JSON does not declare.
//!
//! # What is matched
//!
//! | Capability | Matched when |
//! |---|---|
//! | `remote-shell` | a **command word** is one of [`REMOTE_SHELL_PROGRAMS`] |
//! | `cloud-cli` | a **command word** is one of [`CLOUD_CLI_PROGRAMS`] |
//! | `forge-secrets` | `gh secret …`, `gh variable …`, `gh auth token/login/refresh/logout/setup-git`, `gh auth status --show-token`, or `gh api` against a `…/secrets` / `…/variables` endpoint |
//! | `credential-store` | **any word** (argument, redirect target, assignment value) names a path inside a home credential store ([`CREDENTIAL_DIRS`]) |
//!
//! "Command word" is resolved through a small shell lexer, not a substring
//! scan: quotes and backslashes are honoured (so `"s"sh` is `ssh`, and the
//! prose `gh pr comment --body "use ssh"` is not a command), `;`/`&&`/`|`/
//! newlines/`(`/`)` separate commands, `$(…)`, backticks and `<(…)` bodies are
//! analyzed as commands of their own, and the common wrappers (`sudo`, `env`,
//! `timeout`, `xargs`, `nohup`, `find -exec`, `eval`, `bash -c`, `su -c`,
//! `script -c`, `busybox`, `tmux`/`screen`, a here-string fed to a shell, …)
//! are looked through. `rsync -e`/`host:path`, `git -c core.sshCommand=…`
//! and `GIT_SSH_COMMAND=…` are `remote-shell` too. A path word is lexically
//! normalized (`..`, `.`, `//`, `/proc/<pid>/root`) before the home test.
//!
//! # What is deliberately NOT matched
//!
//! This is a backstop, not a sandbox. A command assembled at run time
//! (`p=ss; ${p}h host`), a script file the session wrote and then ran, an
//! inline interpreter program (`python3 -c`, `perl -e`, `node -e`) that shells
//! out, or text piped into a shell's stdin is not visible to any static
//! matcher, nor is a symlink into a credential directory that already exists
//! on disk (normalization is lexical); neither is
//! the SSH transport `git fetch` uses under the hood (forge transport is not a
//! remote shell). The credential-store matcher anchors on a home directory
//! (`~`, `$HOME`, the hook's own `$HOME`, `/root`, `/home/<u>`, `/Users/<u>`)
//! or a relative path that *starts* with a credential directory, so a path
//! built from variables is not seen either. Those limits are stated in
//! `guard-hooks.md`; over-denying an obfuscated command is preferred to
//! under-denying a plain one, never the reverse.
//!
//! The limits above bind the ROLE capabilities. [`executes_gh_pr_merge`] (the
//! merge-redirect guard's question, #12236) is stricter, because a miss there
//! is a silent allow: it also looks through a line assembled at run time and
//! handed to an interpreter — a pipe into a shell reading stdin, `eval` / `-c`
//! of a substitution's output, a variable expanded as the command word,
//! an expanding heredoc's `$( )` — and treats an incomplete construct (an
//! unterminated heredoc) as unconfined. See "Generated text" below (#15525).

/// Prefix of every deny answer `role-tool-policy check` prints. The hooks
/// match on it, so an exit 1 from something else is never mistaken for a
/// verdict.
pub const CHECK_DENY_PREFIX: &str = "BLOCKED [role-tool-policy]";

/// Programs that are the `remote-shell` capability when they are a command
/// word — the same set as the `Bash(<prog>:*)` specs.
pub const REMOTE_SHELL_PROGRAMS: [&str; 9] = [
    "ssh",
    "scp",
    "sftp",
    "ssh-add",
    "ssh-agent",
    "ssh-keygen",
    "ssh-keyscan",
    "ssh-copy-id",
    "autossh",
];

/// Programs that are the `cloud-cli` capability when they are a command word.
pub const CLOUD_CLI_PROGRAMS: [&str; 10] = [
    "aws", "gcloud", "az", "doctl", "flyctl", "fly", "wrangler", "heroku", "kubectl", "eksctl",
];

#[cfg(test)]
use paths::glob_match;
use paths::is_credential_path;
pub use paths::CREDENTIAL_DIRS;

#[path = "command_match_paths.rs"]
mod paths;

/// Shells whose `-c` argument (or here-string) is itself a command line.
const SHELLS: [&str; 7] = ["sh", "bash", "zsh", "dash", "ksh", "mksh", "fish"];

/// Reserved words and grouping tokens that can precede a command word.
const KEYWORDS: [&str; 13] = [
    "!", "{", "}", "if", "then", "else", "elif", "do", "while", "until", "time", "coproc", "in",
];

/// Wrappers that run their remaining arguments as a command after skipping
/// their own `-`options. Option VALUES are handled per wrapper below where a
/// wrapper takes one.
const PLAIN_WRAPPERS: [&str; 8] = [
    "builtin",
    "exec",
    "nohup",
    "setsid",
    "caffeinate",
    "chronic",
    "doas",
    "sudo",
];

/// Nesting cap for `$(…)` / `bash -c` / `eval` recursion. Real commands nest
/// two or three deep; the cap only bounds a pathological input, and a command
/// past it is matched on what was seen so far.
const MAX_DEPTH: usize = 16;

/// One sensitive capability a command or path would exercise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// A name from [`super::CAPABILITY_NAMESPACE`].
    pub capability: &'static str,
    /// What matched, for the deny message (`ssh`, `gh secret`, a path, …).
    pub evidence: String,
}

impl Hit {
    fn new(capability: &'static str, evidence: impl Into<String>) -> Self {
        Self {
            capability,
            evidence: evidence.into(),
        }
    }
}

/// Every capability hit in a Bash command line, in source order.
///
/// `home` is the hook process's `$HOME` (the same user the command runs as),
/// used to recognize an absolute path into the home directory.
#[must_use]
pub fn command_hits(command: &str, home: Option<&str>) -> Vec<Hit> {
    reset_generated_text_budget();
    let mut hits = Vec::new();
    analyze_str(command, home, 0, &mut hits);
    hits.retain(|h| h.capability != GH_PR_MERGE);
    hits
}

/// Marker `capability` for a live `gh pr merge`; not a role capability, so
/// [`command_hits`] never returns it.
const GH_PR_MERGE: &str = "gh-pr-merge";

/// Whether the command line would execute `gh pr merge`: plain, chained, in a
/// `$( )`, `bash -c`/`eval` payload or wrapper. Text in a quoted argument or an
/// inert heredoc body does not count.
#[must_use]
pub fn executes_gh_pr_merge(command: &str) -> bool {
    reset_generated_text_budget();
    let mut hits = Vec::new();
    analyze_str(command, None, 0, &mut hits);
    hits.iter().any(|h| h.capability == GH_PR_MERGE)
}

/// The `credential-store` hit for an Edit/Write target path, if any.
#[must_use]
pub fn path_hit(path: &str, home: Option<&str>) -> Option<Hit> {
    is_credential_path(path, home).then(|| Hit::new("credential-store", path))
}

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WordKind {
    Plain,
    /// The target of `<`, `>`, `>>`, … — a path, never a command word.
    Redirect,
    /// The operand of `<<<` — text a command reads on stdin.
    HereString,
}

#[derive(Debug, Clone)]
struct Word {
    text: String,
    kind: WordKind,
    /// Bodies of the `$(…)` / backtick substitutions inside this word. The
    /// word's `text` omits them; their OUTPUT is what the shell splices in.
    subs: Vec<String>,
}

struct Lexed {
    segments: Vec<Vec<Word>>,
    /// Bodies of `$(…)`, backticks and `<(…)`/`>(…)`: commands of their own.
    nested: Vec<String>,
    /// Parallel to `segments`: the segment's stdin is the previous segment's
    /// stdout (`|`, `|&`).
    piped: Vec<bool>,
    /// Every heredoc body, with the index of the segment closing its line.
    heredocs: Vec<(usize, String)>,
    /// Bodies of heredocs that never reach their delimiter line.
    unterminated: Vec<String>,
}

/// Split a command line into simple commands (word lists).
// The trailing `end_segment!()` resets state a final time that nothing reads.
#[allow(unused_assignments)]
fn lex(input: &str) -> Lexed {
    let c: Vec<char> = input.chars().collect();
    let mut out = Lexed {
        segments: Vec::new(),
        nested: Vec::new(),
        piped: Vec::new(),
        heredocs: Vec::new(),
        unterminated: Vec::new(),
    };
    let mut seg: Vec<Word> = Vec::new();
    let mut cur = String::new();
    let mut cur_subs: Vec<String> = Vec::new();
    let mut in_word = false;
    let mut pending = WordKind::Plain;
    // `(delimiter, strip_tabs, quoted)` of each heredoc opened on this line.
    let mut heredocs: Vec<(String, bool, bool)> = Vec::new();
    // The next segment's stdin is the one just ended (`|`, `|&`).
    let mut pipe_pending = false;
    let mut i = 0;

    macro_rules! end_word {
        () => {
            if in_word {
                seg.push(Word {
                    text: std::mem::take(&mut cur),
                    kind: pending,
                    subs: std::mem::take(&mut cur_subs),
                });
                pending = WordKind::Plain;
                in_word = false;
            }
        };
    }
    macro_rules! end_segment {
        () => {
            end_word!();
            pending = WordKind::Plain;
            if !seg.is_empty() {
                out.segments.push(std::mem::take(&mut seg));
                out.piped.push(std::mem::take(&mut pipe_pending));
            }
        };
    }
    // A substitution inside the current word: a command of its own, and text
    // the word splices in.
    macro_rules! add_sub {
        ($body:expr) => {{
            let body: String = $body;
            cur_subs.push(body.clone());
            out.nested.push(body);
        }};
    }

    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        match ch {
            '\\' => {
                match next {
                    Some('\n') => {}
                    Some(n) => {
                        cur.push(n);
                        in_word = true;
                    }
                    None => {}
                }
                i += 2;
                continue;
            }
            '\'' => {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    cur.push(c[i]);
                    i += 1;
                }
                in_word = true;
            }
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    match c[i] {
                        '\\' if matches!(c.get(i + 1), Some('$' | '`' | '"' | '\\' | '\n')) => {
                            if c[i + 1] != '\n' {
                                cur.push(c[i + 1]);
                            }
                            i += 2;
                            continue;
                        }
                        '$' if c.get(i + 1) == Some(&'(') => {
                            let (body, end) = take_balanced(&c, i + 2);
                            add_sub!(body);
                            i = end;
                            continue;
                        }
                        '$' if c.get(i + 1) == Some(&'{') => {
                            let end = take_brace(&c, i + 2, &mut out.nested);
                            cur.extend(&c[i..end]);
                            i = end;
                            continue;
                        }
                        '`' => {
                            let (body, end) = take_backtick(&c, i + 1);
                            add_sub!(body);
                            i = end;
                            continue;
                        }
                        other => cur.push(other),
                    }
                    i += 1;
                }
                in_word = true;
            }
            '$' if next == Some('(') => {
                let (body, end) = take_balanced(&c, i + 2);
                add_sub!(body);
                in_word = true;
                i = end;
                continue;
            }
            '$' if next == Some('{') => {
                let end = take_brace(&c, i + 2, &mut out.nested);
                cur.extend(&c[i..end]);
                in_word = true;
                i = end;
                continue;
            }
            '$' if next == Some('\'') => {
                // ANSI-C quoting: escapes are decoded so `$'\x73sh'` matches `ssh`.
                // Bash truncates the string at the first NUL, so the rest of the
                // quoted body is dropped (but still consumed up to the quote).
                i += 2;
                let mut truncated = false;
                while i < c.len() && c[i] != '\'' {
                    if c[i] == '\\' && i + 1 < c.len() {
                        let (decoded, used) = decode_ansi_c_escape(&c[i + 1..]);
                        match decoded {
                            Some(text) if !truncated => cur.push_str(&text),
                            Some(_) => {}
                            None => truncated = true,
                        }
                        i += 1 + used;
                        continue;
                    }
                    if !truncated {
                        cur.push(c[i]);
                    }
                    i += 1;
                }
                in_word = true;
            }
            '`' => {
                let (body, end) = take_backtick(&c, i + 1);
                add_sub!(body);
                in_word = true;
                i = end;
                continue;
            }
            '#' if !in_word => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            ' ' | '\t' => end_word!(),
            '\n' if !heredocs.is_empty() => {
                end_word!();
                // The body is data unless the command reads it as code (a shell)
                // or the body is written to a file that may be run later.
                let live = seg_runs_shell(&seg) || seg.iter().any(is_file_redirect);
                i += 1;
                // The segment this line ends is the one that will be pushed next.
                let at = out.segments.len();
                for (delim, strip_tabs, quoted) in std::mem::take(&mut heredocs) {
                    let (body, terminated) = take_heredoc_body(&c, &mut i, &delim, strip_tabs);
                    if !terminated {
                        out.unterminated.push(body.clone());
                    }
                    // An unquoted delimiter leaves the body expanding: `$( )` and
                    // backticks in it run, whatever consumes the text.
                    if !quoted {
                        out.nested.extend(heredoc_substitutions(&body));
                    }
                    out.heredocs.push((at, body.clone()));
                    if live {
                        out.nested.push(body);
                    }
                }
                end_segment!();
                continue;
            }
            '(' if next == Some('(') && !in_word && seg.is_empty() => {
                // `(( … ))` arithmetic command: its `<<` is a shift, not a heredoc.
                // Analyzed as one region (so `$( )` inside still counts); a `((`
                // that does not close as `))` is nested subshells and lexes normally.
                let (body, end) = take_balanced(&c, i + 2);
                if c.get(end) == Some(&')') {
                    out.nested.push(body);
                    i = end + 1;
                    continue;
                }
                end_segment!();
            }
            '|' if next == Some('|') => {
                // `||` is an or-list, not a pipe.
                end_segment!();
                i += 2;
                continue;
            }
            '|' => {
                end_segment!();
                pipe_pending = true;
                if next == Some('&') {
                    i += 1;
                }
            }
            '\n' | ';' | '&' | '(' | ')' => {
                end_segment!();
            }
            '<' | '>' if next == Some('(') => {
                end_word!();
                let (body, end) = take_balanced(&c, i + 2);
                out.nested.push(body);
                i = end;
                continue;
            }
            '<' | '>' => {
                end_word!();
                let mut run = 1;
                while matches!(c.get(i + run), Some('<' | '>')) {
                    run += 1;
                }
                if ch == '<' && run == 2 {
                    let strip_tabs = c.get(i + 2) == Some(&'-');
                    let (delim, end, quoted) =
                        take_heredoc_delimiter(&c, i + 2 + usize::from(strip_tabs));
                    // Not a redirect (`<<=` shift-assign, `<< 3` shift): keep lexing
                    // the following lines as commands rather than swallow them.
                    if is_heredoc_opener(&c, i + 2 + usize::from(strip_tabs), &delim) {
                        heredocs.push((delim, strip_tabs, quoted));
                        i = end;
                        continue;
                    }
                }
                pending = if ch == '<' && run >= 3 {
                    WordKind::HereString
                } else {
                    WordKind::Redirect
                };
                i += run;
                continue;
            }
            other => {
                cur.push(other);
                in_word = true;
            }
        }
        i += 1;
    }
    end_segment!();
    out
}

/// A `>`/`>>` target other than `/dev/…` or an fd: a file the body may land in.
fn is_file_redirect(w: &Word) -> bool {
    w.kind == WordKind::Redirect
        && !w.text.starts_with("/dev/")
        && !w.text.chars().all(|ch| ch.is_ascii_digit())
}

/// Whether a `<<` whose delimiter text starts at `start` opens a heredoc: it
/// needs a delimiter word, and `<<=` or a bare numeric word after a space reads
/// as an arithmetic shift, not a redirect.
fn is_heredoc_opener(c: &[char], start: usize, delim: &str) -> bool {
    if delim.is_empty() || c.get(start) == Some(&'=') {
        return false;
    }
    let spaced = matches!(c.get(start), Some(' ' | '\t'));
    !(spaced && delim.chars().all(|ch| ch.is_ascii_digit()))
}

/// The delimiter word of a `<<` heredoc starting at `start` (quotes and
/// backslashes removed), the index just past it, and whether any quoting was
/// present (a quoted delimiter makes the body literal; an unquoted one leaves
/// it expanding).
fn take_heredoc_delimiter(c: &[char], start: usize) -> (String, usize, bool) {
    let mut i = start;
    while matches!(c.get(i), Some(' ' | '\t')) {
        i += 1;
    }
    let mut delim = String::new();
    let mut quoted = false;
    while i < c.len()
        && !c[i].is_whitespace()
        && !matches!(c[i], ';' | '&' | '|' | '(' | ')' | '<' | '>')
    {
        match c[i] {
            q @ ('\'' | '"') => {
                quoted = true;
                i += 1;
                while i < c.len() && c[i] != q {
                    delim.push(c[i]);
                    i += 1;
                }
            }
            '\\' => {
                quoted = true;
                i += 1;
                if let Some(&n) = c.get(i) {
                    delim.push(n);
                }
            }
            other => delim.push(other),
        }
        i += 1;
    }
    (delim, i, quoted)
}

/// Consume heredoc body lines from `*i` up to and including the line equal to
/// `delim` (leading tabs ignored for `<<-`). An unterminated heredoc runs to
/// the end of input, like bash; the flag says whether the delimiter was found.
fn take_heredoc_body(c: &[char], i: &mut usize, delim: &str, strip_tabs: bool) -> (String, bool) {
    let mut body = String::new();
    while *i < c.len() {
        let end = c[*i..]
            .iter()
            .position(|&ch| ch == '\n')
            .map_or(c.len(), |p| *i + p);
        let line: String = c[*i..end].iter().collect();
        *i = (end + 1).min(c.len());
        let cmp = if strip_tabs {
            line.trim_start_matches('\t')
        } else {
            line.as_str()
        };
        if cmp == delim {
            return (body, true);
        }
        body.push_str(&line);
        body.push('\n');
    }
    (body, false)
}

/// The `$( )`, backtick and `${ }`-embedded substitution bodies of an
/// EXPANDING heredoc body (unquoted delimiter): bash runs them.
fn heredoc_substitutions(body: &str) -> Vec<String> {
    let c: Vec<char> = body.chars().collect();
    let mut subs = Vec::new();
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            '\\' => i += 2,
            '$' if c.get(i + 1) == Some(&'(') => {
                let (sub, end) = take_balanced(&c, i + 2);
                subs.push(sub);
                i = end;
            }
            '$' if c.get(i + 1) == Some(&'{') => i = take_brace(&c, i + 2, &mut subs),
            '`' => {
                let (sub, end) = take_backtick(&c, i + 1);
                subs.push(sub);
                i = end;
            }
            _ => i += 1,
        }
    }
    subs
}

/// Where a `case … esac` is inside [`take_balanced`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum CaseState {
    /// Between `case` and `in`.
    Header,
    /// Reading a pattern list, up to its `)`.
    Pattern,
    /// Reading the commands of one arm, up to `;;`.
    Body,
}

/// One open construct inside [`take_balanced`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Frame {
    Paren,
    Case(CaseState),
}

/// Characters that end an unquoted word inside [`take_balanced`].
const WORD_BREAKS: [char; 14] = [
    ';', '&', '|', '(', ')', '{', '}', '<', '>', '"', '\'', '`', '$', '\\',
];

/// Words after which another command word may start.
const COMMAND_STARTERS: [&str; 8] = ["do", "then", "else", "elif", "if", "while", "until", "time"];

/// The body of a `(`-opened region starting at `start` (just past the `(`),
/// and the index just past its matching `)`. Quotes, comments, `${…}`
/// expansions, heredoc bodies and `case` patterns inside are skipped, so a `)`
/// that is not syntax does not close it. An unterminated region runs to the end.
fn take_balanced(c: &[char], start: usize) -> (String, usize) {
    let mut frames = vec![Frame::Paren];
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    // A command word may start here; `case` and `esac` count only then.
    let mut cmd_start = true;
    let mut i = start;
    while i < c.len() {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        match ch {
            '\\' => {
                i += 2;
                cmd_start = false;
                continue;
            }
            '\'' => {
                i += 1;
                while i < c.len() && c[i] != '\'' {
                    i += 1;
                }
                cmd_start = false;
            }
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    if c[i] == '\\' {
                        i += 1;
                    }
                    i += 1;
                }
                cmd_start = false;
            }
            '$' if next == Some('{') => {
                i = take_brace(c, i + 2, &mut Vec::new());
                cmd_start = false;
                continue;
            }
            '#' if i == start || matches!(c[i - 1], ' ' | '\t' | '\n' | ';' | '&' | '|' | '(') => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
                continue;
            }
            '<' if next == Some('<') && c.get(i + 2) == Some(&'<') => {
                i += 3;
                cmd_start = false;
                continue;
            }
            '<' if next == Some('<') => {
                let strip_tabs = c.get(i + 2) == Some(&'-');
                let at = i + 2 + usize::from(strip_tabs);
                let (delim, end, _) = take_heredoc_delimiter(c, at);
                if is_heredoc_opener(c, at, &delim) {
                    heredocs.push((delim, strip_tabs));
                    i = end;
                } else {
                    i += 2;
                }
                cmd_start = false;
                continue;
            }
            '\n' => {
                i += 1;
                for (delim, strip_tabs) in std::mem::take(&mut heredocs) {
                    take_heredoc_body(c, &mut i, &delim, strip_tabs);
                }
                cmd_start = true;
                continue;
            }
            ';' => {
                // `;;` / `;;&` / `;&` end a case arm: a pattern list follows.
                let ends_arm = next == Some(';')
                    || (next == Some('&') && frames.last() == Some(&Frame::Case(CaseState::Body)));
                if ends_arm {
                    if let Some(Frame::Case(state)) = frames.last_mut() {
                        *state = CaseState::Pattern;
                    }
                    i += 2;
                    if c.get(i) == Some(&'&') && next == Some(';') {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
                cmd_start = true;
                continue;
            }
            '(' => {
                // The optional `(` before a case pattern opens nothing.
                if frames.last() != Some(&Frame::Case(CaseState::Pattern)) {
                    frames.push(Frame::Paren);
                }
                cmd_start = true;
            }
            ')' => {
                loop {
                    match frames.last_mut() {
                        Some(Frame::Case(state @ CaseState::Pattern)) => {
                            *state = CaseState::Body;
                            break;
                        }
                        // `case` was an argument, not a keyword, or the arm is
                        // malformed: the `)` belongs to the frame below.
                        Some(Frame::Case(_)) => {
                            frames.pop();
                        }
                        Some(Frame::Paren) => {
                            frames.pop();
                            if frames.is_empty() {
                                return (c[start..i].iter().collect(), i + 1);
                            }
                            break;
                        }
                        None => break,
                    }
                }
                cmd_start = true;
            }
            '|' | '&' | '{' => cmd_start = true,
            _ if ch.is_whitespace() => {}
            '$' | '<' | '>' | '`' | '}' => cmd_start = false,
            _ => {
                let mut j = i;
                while j < c.len() && !c[j].is_whitespace() && !WORD_BREAKS.contains(&c[j]) {
                    j += 1;
                }
                let word: String = c[i..j].iter().collect();
                match (word.as_str(), frames.last().copied()) {
                    ("case", _) if cmd_start => {
                        frames.push(Frame::Case(CaseState::Header));
                        cmd_start = false;
                    }
                    ("in", Some(Frame::Case(CaseState::Header))) => {
                        frames.pop();
                        frames.push(Frame::Case(CaseState::Pattern));
                        cmd_start = false;
                    }
                    ("esac", Some(Frame::Case(CaseState::Pattern | CaseState::Body)))
                        if cmd_start =>
                    {
                        frames.pop();
                        cmd_start = false;
                    }
                    (w, _) => cmd_start = COMMAND_STARTERS.contains(&w),
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    (c[start.min(c.len())..].iter().collect(), c.len())
}

/// Index just past the `}` closing a `${…}` whose body starts at `start`.
/// Executable substitutions inside the expansion (`${v:-$(cmd)}`,
/// `${v:-`cmd`}`) run when bash expands it, so their bodies are pushed onto
/// `nested` and skipped over (a `}` inside one does not close the expansion).
fn take_brace(c: &[char], start: usize, nested: &mut Vec<String>) -> usize {
    let mut depth = 1usize;
    let mut i = start;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '$' if c.get(i + 1) == Some(&'(') => {
                let (body, end) = take_balanced(c, i + 2);
                nested.push(body);
                i = end;
                continue;
            }
            '`' => {
                let (body, end) = take_backtick(c, i + 1);
                nested.push(body);
                i = end;
                continue;
            }
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            _ => {}
        }
        i += 1;
    }
    c.len()
}

/// The body of a backtick substitution starting at `start`, and the index
/// just past its closing backtick.
fn take_backtick(c: &[char], start: usize) -> (String, usize) {
    let mut body = String::new();
    let mut i = start;
    while i < c.len() && c[i] != '`' {
        if c[i] == '\\' && i + 1 < c.len() {
            body.push(c[i + 1]);
            i += 2;
            continue;
        }
        body.push(c[i]);
        i += 1;
    }
    (body, (i + 1).min(c.len()))
}

// ---------------------------------------------------------------------------
// Analysis
// ---------------------------------------------------------------------------

fn analyze_str(command: &str, home: Option<&str>, depth: usize, hits: &mut Vec<Hit>) {
    if depth > MAX_DEPTH {
        return;
    }
    let lexed = lex(command);
    for seg in &lexed.segments {
        let pattern_word = search_pattern_word(seg);
        for (idx, w) in seg.iter().enumerate() {
            // `GIT_SSH_COMMAND=…` (prefix, `env`, `export`) makes the next
            // git transport an arbitrary remote-shell command.
            if w.kind == WordKind::Plain && is_assignment(&w.text) {
                let name = w.text.split_once('=').map_or("", |(n, _)| n);
                if matches!(name, "GIT_SSH_COMMAND" | "GIT_SSH" | "RSYNC_RSH") {
                    hits.push(Hit::new("remote-shell", name));
                }
            }
            // Every word, wherever it sits, can name a credential store —
            // including `--key=~/.ssh/id` and `host:~/.ssh/x`.
            // The one exception is a grep/rg search PATTERN: it is text the
            // tool matches against, never a file it opens.
            for piece in w.text.split(['=', ':']) {
                if Some(idx) != pattern_word && is_credential_path(piece, home) {
                    hits.push(Hit::new("credential-store", w.text.clone()));
                    break;
                }
            }
        }
        for w in seg.iter().filter(|w| w.kind == WordKind::HereString) {
            // `<<<` feeds text to stdin; when that stdin is a shell it is a
            // command line. Analyzing it for any command over-denies only
            // a here-string that itself mentions a capability.
            if seg_runs_shell(seg) {
                analyze_str(&w.text, home, depth + 1, hits);
            }
        }
        let argv: Vec<String> = seg
            .iter()
            .filter(|w| w.kind == WordKind::Plain)
            .map(|w| w.text.clone())
            .collect();
        analyze_argv(&argv, home, depth, hits);
    }
    for body in &lexed.nested {
        analyze_str(body, home, depth + 1, hits);
    }
    // Text an interpreter reads that no static scan of the command words sees:
    // see the "Generated text" section below. It speaks only to `gh pr merge`.
    for body in &lexed.unterminated {
        merge_only(body, depth + 1, hits);
    }
    consume_generated_text(&lexed, depth, hits);
}

// ---------------------------------------------------------------------------
// Generated text (#15525)
// ---------------------------------------------------------------------------
//
// A command can ASSEMBLE the line an interpreter then runs: `echo "gh pr merge
// 1" | bash`, `eval "$(cat <<'EOF' … EOF)"`, `X=$(…); $X`, `for q in "…"; do
// eval "$q"; done`. Nothing in the assembling command is itself a command word,
// so the lexer above never sees the merge. Wherever a segment hands text to an
// interpreter — a pipe into a shell reading stdin, `eval`, a `-c` payload, a
// here-string, a variable expanded as the command word — every string that text
// can be built from (an argument, a heredoc body, a substitution's body, the
// values of the variables it names) is analyzed as a script too. That
// over-denies only a command that routes a quoted phrase INTO an interpreter;
// the same phrase going to `echo`, `gh issue edit --body` or a `--search` stays
// data. The scan answers `gh pr merge` alone: the role-capability matcher keeps
// its documented limits (a command assembled at run time is out of reach).

/// Upper bound on generated-text scans per top-level query. Past it the answer
/// fails closed for `gh pr merge`: a command this tangled is denied.
const GENERATED_TEXT_BUDGET: usize = 20_000;

thread_local! {
    static GENERATED_TEXT_LEFT: std::cell::Cell<usize> =
        const { std::cell::Cell::new(GENERATED_TEXT_BUDGET) };
}

fn reset_generated_text_budget() {
    GENERATED_TEXT_LEFT.with(|left| left.set(GENERATED_TEXT_BUDGET));
}

/// Spend one scan from the budget; false once it is spent.
fn spend_generated_text_budget() -> bool {
    GENERATED_TEXT_LEFT.with(|left| {
        let n = left.get();
        left.set(n.saturating_sub(1));
        n > 0
    })
}

/// Analyze `text` as a script, keeping only a `gh pr merge` finding.
fn merge_only(text: &str, depth: usize, hits: &mut Vec<Hit>) {
    let mut scratch = Vec::new();
    analyze_str(text, None, depth, &mut scratch);
    hits.extend(scratch.into_iter().filter(|h| h.capability == GH_PR_MERGE));
}

/// Variable name → the texts assigned to it anywhere in one command line (an
/// assignment word's value and substitution bodies, a `for` loop's items).
/// Flow-insensitive on purpose: a later reassignment does not hide an earlier
/// value.
type Vars = std::collections::HashMap<String, Vec<String>>;

fn collect_vars(lexed: &Lexed) -> Vars {
    let mut vars = Vars::new();
    for seg in &lexed.segments {
        if let [first, name, keyword, items @ ..] = seg.as_slice() {
            if first.text == "for" && keyword.text == "in" {
                let values = vars.entry(name.text.clone()).or_default();
                for item in items.iter().filter(|w| w.kind == WordKind::Plain) {
                    values.push(item.text.clone());
                    values.extend(item.subs.iter().cloned());
                }
            }
        }
        for w in seg.iter().filter(|w| w.kind == WordKind::Plain) {
            if let Some((name, value)) = w.text.split_once('=').filter(|_| is_assignment(&w.text)) {
                let values = vars.entry(name.to_string()).or_default();
                values.push(value.to_string());
                values.extend(w.subs.iter().cloned());
            }
        }
    }
    vars
}

/// Variable names a word expands (`$x`, `${x}`, `${x:0:9}`, `${x#}`), and for
/// an indirect `${!r}` the name `r` points at.
fn referenced_names(text: &str, vars: &Vars) -> Vec<String> {
    let c: Vec<char> = text.chars().collect();
    let mut names = Vec::new();
    for (i, _) in c.iter().enumerate().filter(|(_, ch)| **ch == '$') {
        let mut j = i + 1;
        if c.get(j) == Some(&'{') {
            j += 1;
        }
        let indirect = c.get(j) == Some(&'!');
        if indirect {
            j += 1;
        }
        let name: String = c[j.min(c.len())..]
            .iter()
            .take_while(|ch| ch.is_ascii_alphanumeric() || **ch == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        if indirect {
            let pointed = vars.get(&name).into_iter().flatten();
            names.extend(pointed.map(|v| v.trim().to_string()));
        }
        names.push(name);
    }
    names.sort();
    names.dedup();
    names
}

/// How many argument tails of one segment are scanned joined (`echo gh pr merge
/// 1` prints one line, however many words the lexer split it into).
const JOINED_TAILS: usize = 8;

/// The state of one generated-text scan.
struct Generated<'a> {
    vars: &'a Vars,
    /// Variables being expanded right now, so a self-referencing value ends.
    active: Vec<String>,
    /// Texts already scanned, with the shallowest depth each was reached at. A
    /// text is scanned again only from a shallower depth, which can see further
    /// before [`MAX_DEPTH`] cuts it off.
    seen: std::collections::HashMap<String, usize>,
}

impl Generated<'_> {
    /// Analyze `text` as a script and every string it can be built from.
    fn scan(&mut self, text: &str, depth: usize, hits: &mut Vec<Hit>) {
        if depth > MAX_DEPTH || self.seen.get(text).is_some_and(|&d| d <= depth) {
            return;
        }
        self.seen.insert(text.to_string(), depth);
        if !spend_generated_text_budget() {
            hits.push(Hit::new(GH_PR_MERGE, GH_PR_MERGE));
            return;
        }
        merge_only(text, depth, hits);
        let lexed = lex(text);
        for seg in &lexed.segments {
            let words: Vec<&Word> = seg
                .iter()
                .filter(|w| w.kind != WordKind::Redirect)
                .collect();
            for w in &words {
                self.scan_word(w, text, depth, hits);
            }
            // A producer such as `echo` prints its arguments as one line.
            for from in 1..words.len().min(JOINED_TAILS + 1) {
                let tail: Vec<&str> = words[from..].iter().map(|w| w.text.as_str()).collect();
                self.scan(&tail.join(" "), depth + 1, hits);
            }
        }
        for (_, body) in &lexed.heredocs {
            self.scan(body, depth + 1, hits);
        }
    }

    /// `parent` is the text the word was lexed from; a word that is the whole
    /// text (nothing to unquote) is not scanned again.
    fn scan_word(&mut self, w: &Word, parent: &str, depth: usize, hits: &mut Vec<Hit>) {
        if w.text != parent {
            self.scan(&w.text, depth + 1, hits);
        }
        for sub in &w.subs {
            self.scan(sub, depth + 1, hits);
        }
        for name in referenced_names(&w.text, self.vars) {
            if self.active.contains(&name) {
                continue;
            }
            let Some(values) = self.vars.get(&name) else {
                continue;
            };
            self.active.push(name);
            for value in values {
                self.scan(value, depth + 1, hits);
            }
            self.active.pop();
        }
    }
}

/// Index of the command word among a segment's plain words: past assignments,
/// reserved words and the wrappers that only prefix another command.
fn command_word_index(words: &[&Word]) -> Option<usize> {
    let mut i = 0;
    while i < words.len() {
        let t = words[i].text.as_str();
        if is_assignment(t) || KEYWORDS.contains(&t) {
            i += 1;
        } else if PLAIN_WRAPPERS.contains(&basename(t))
            || matches!(basename(t), "env" | "command" | "nice")
        {
            i += 1;
            while i < words.len() && words[i].text.starts_with('-') {
                i += 1;
            }
        } else {
            return Some(i);
        }
    }
    None
}

/// Whether the shell whose words are `args` (after the shell's own name) reads
/// its script from stdin: no `-c`, and either `-s`, `-`, `/dev/stdin` or no
/// script operand at all.
fn shell_reads_stdin(args: &[&Word]) -> bool {
    let mut j = 0;
    while j < args.len() {
        let w = args[j].text.as_str();
        if w == "-s" {
            return true;
        }
        if matches!(w, "-o" | "+o" | "-O" | "+O") {
            j += 2;
            continue;
        }
        if w.starts_with('-') && w != "-" {
            if !w.starts_with("--") && w[1..].contains('c') {
                return false;
            }
            j += 1;
            continue;
        }
        return w == "-" || w == "/dev/stdin";
    }
    true
}

/// The `-c` payload word of a shell whose words after its name are `args`.
fn shell_c_payload<'a>(args: &[&'a Word]) -> Option<&'a Word> {
    let mut j = 0;
    while j < args.len() {
        let w = args[j].text.as_str();
        if matches!(w, "-o" | "+o" | "-O" | "+O") {
            j += 2;
            continue;
        }
        if w.starts_with('-') && !w.starts_with("--") && w[1..].contains('c') {
            return args.get(j + 1).copied();
        }
        if !w.starts_with('-') && !w.starts_with('+') {
            return None;
        }
        j += 1;
    }
    None
}

/// Scan the text each segment of `lexed` hands to an interpreter.
fn consume_generated_text(lexed: &Lexed, depth: usize, hits: &mut Vec<Hit>) {
    let vars = collect_vars(lexed);
    let mut scan = Generated {
        vars: &vars,
        active: Vec::new(),
        seen: std::collections::HashMap::new(),
    };
    for (i, seg) in lexed.segments.iter().enumerate() {
        let words: Vec<&Word> = seg.iter().filter(|w| w.kind == WordKind::Plain).collect();
        let command = command_word_index(&words);
        // `eval <words>`: the arguments are joined and run.
        if let Some(p) = words.iter().position(|w| w.text == "eval") {
            for w in &words[p + 1..] {
                scan.scan_word(w, "", depth + 1, hits);
            }
        }
        if let Some(c) = command {
            let program = basename(&words[c].text);
            // `$X args…`: the variable's value is the command.
            if words[c].text.starts_with('$') {
                scan.scan_word(words[c], "", depth + 1, hits);
            }
            if SHELLS.contains(&program) {
                // `bash -c "$(…)"`.
                if let Some(payload) = shell_c_payload(&words[c + 1..]) {
                    scan.scan_word(payload, "", depth + 1, hits);
                }
                // `bash <<< "$(…)"` / a heredoc fed to it.
                for w in seg.iter().filter(|w| w.kind == WordKind::HereString) {
                    scan.scan_word(w, "", depth + 1, hits);
                }
                // `… | bash`: everything the pipeline's earlier stages print.
                if lexed.piped[i] && shell_reads_stdin(&words[c + 1..]) {
                    let start = (0..i).rev().find(|&k| !lexed.piped[k]).unwrap_or(0);
                    for stage in &lexed.segments[start..i] {
                        for w in stage.iter().filter(|w| w.kind != WordKind::Redirect) {
                            scan.scan_word(w, "", depth + 1, hits);
                        }
                    }
                    for (at, body) in &lexed.heredocs {
                        if (start..=i).contains(at) {
                            scan.scan(body, depth + 1, hits);
                        }
                    }
                }
            }
        }
    }
}

/// Index (into `seg`) of the search pattern when the segment is a plain
/// `grep`/`rg`-family call, so a quoted `'~/.ssh'` pattern is not mistaken for
/// a path. Input FILE operands (including `-f FILE`) are never returned.
fn search_pattern_word(seg: &[Word]) -> Option<usize> {
    const SEARCHERS: [&str; 7] = ["grep", "egrep", "fgrep", "zgrep", "rg", "ag", "ack"];
    const WITH_VALUE: [&str; 17] = [
        "-A",
        "-B",
        "-C",
        "-m",
        "-g",
        "-t",
        "-T",
        "-d",
        "-D",
        "--glob",
        "--type",
        "--include",
        "--exclude",
        "--exclude-dir",
        "--max-count",
        "--context",
        "--max-depth",
    ];
    let mut i = seg
        .iter()
        .position(|w| w.kind == WordKind::Plain && !is_assignment(&w.text))?;
    if !SEARCHERS.contains(&basename(&seg[i].text)) {
        return None;
    }
    i += 1;
    let mut first_operand = None;
    let mut pattern_by_option = None;
    while i < seg.len() {
        let w = &seg[i];
        if w.kind != WordKind::Plain {
            i += 1;
            continue;
        }
        let t = w.text.as_str();
        if t == "--" {
            first_operand = first_operand.or(seg[i + 1..]
                .iter()
                .position(|n| n.kind == WordKind::Plain)
                .map(|p| i + 1 + p));
            break;
        }
        if t == "-e" || t == "--regexp" {
            pattern_by_option = pattern_by_option.or(Some(i + 1));
            i += 2;
        } else if t.starts_with('-') && t != "-" {
            // Pattern given inside this very word (`--regexp=x`, `-ex`,
            // `-rnex`), via `-f`/`--file` (a FILE that stays a checked path),
            // or no pattern at all (`rg --files`): either way no operand is
            // the pattern, so every operand stays a checked path.
            let (short_e, short_f) = short_cluster_flags(t);
            if t.starts_with("--regexp=")
                || t == "-f"
                || t == "--file"
                || t.starts_with("--file=")
                || t == "--files"
                || short_f
                || short_e == Some(true)
            {
                pattern_by_option = pattern_by_option.or(Some(usize::MAX));
            } else if short_e == Some(false) {
                // Trailing `e` of a cluster (`-rne`): the next word is the pattern.
                pattern_by_option = pattern_by_option.or(Some(i + 1));
                i += 2;
                continue;
            }
            i += if WITH_VALUE.contains(&t) { 2 } else { 1 };
        } else {
            first_operand = first_operand.or(Some(i));
            break;
        }
    }
    // With `-e PATTERN` every operand is a file; otherwise the first operand
    // is the pattern.
    pattern_by_option.map_or(first_operand, |p| (p != usize::MAX).then_some(p))
}

/// For a short-option cluster (`-rne`, `-ex`, `-fFILE`): whether it carries
/// `-e` (`Some(true)` = pattern attached in the same word, `Some(false)` = `e`
/// is last so the pattern is the next word), and whether it carries `-f`.
/// Long options and non-clusters yield `(None, false)`.
fn short_cluster_flags(word: &str) -> (Option<bool>, bool) {
    let Some(rest) = word.strip_prefix('-').filter(|r| !r.starts_with('-')) else {
        return (None, false);
    };
    for (idx, c) in rest.char_indices() {
        match c {
            'e' => return (Some(idx + 1 < rest.len()), false),
            'f' => return (None, true),
            _ => {}
        }
    }
    (None, false)
}

/// Decode one backslash escape of `$'...'` (the text after the backslash).
/// Returns the decoded text and how many chars were consumed. `None` means
/// the escape decoded to NUL (bash truncates the string there); an invalid
/// scalar yields empty text; unknown escapes keep the escaped char.
fn decode_ansi_c_escape(rest: &[char]) -> (Option<String>, usize) {
    let radix_run = |radix: u32, max: usize, skip: usize| {
        let digits: String = rest[skip..]
            .iter()
            .take(max)
            .take_while(|ch| ch.is_digit(radix))
            .collect();
        let value = u32::from_str_radix(&digits, radix).ok();
        (value, digits.len())
    };
    let simple = |ch: char| -> Option<char> {
        Some(match ch {
            'a' => '\x07',
            'b' => '\x08',
            'e' | 'E' => '\x1b',
            'f' => '\x0c',
            'n' => '\n',
            'r' => '\r',
            't' => '\t',
            'v' => '\x0b',
            _ => return None,
        })
    };
    let first = rest[0];
    if let Some(ch) = simple(first) {
        return (Some(ch.to_string()), 1);
    }
    let (value, used) = match first {
        'x' => radix_run(16, 2, 1),
        'u' => radix_run(16, 4, 1),
        'U' => radix_run(16, 8, 1),
        '0'..='7' => radix_run(8, 3, 0),
        _ => return (Some(first.to_string()), 1),
    };
    let consumed = if first.is_digit(8) { used } else { 1 + used };
    if used == 0 {
        return (Some(first.to_string()), 1);
    }
    if value == Some(0) {
        return (None, consumed);
    }
    let decoded = value
        .and_then(char::from_u32)
        .map(String::from)
        .unwrap_or_default();
    (Some(decoded), consumed)
}

fn seg_runs_shell(seg: &[Word]) -> bool {
    seg.iter()
        .find(|w| w.kind == WordKind::Plain && !is_assignment(&w.text))
        .is_some_and(|w| SHELLS.contains(&basename(&w.text)))
}

fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Index of the first word after a run of `-`options, where each option in
/// `with_value` consumes the following word too. `--` ends the options.
///
/// A short-option cluster (`-rn 1`, `-Hu root`, `-n1`) is read flag by flag: the
/// first value-taking flag takes the rest of the cluster as its value when one
/// is attached, and otherwise consumes the following word.
fn skip_options(argv: &[String], with_value: &[&str]) -> usize {
    let mut j = 0;
    while j < argv.len() {
        let w = argv[j].as_str();
        if w == "--" {
            return j + 1;
        }
        if !w.starts_with('-') || w == "-" {
            break;
        }
        j += if with_value.contains(&w) || cluster_takes_next_word(w, with_value) {
            2
        } else {
            1
        };
    }
    j.min(argv.len())
}

/// True when `w` is a short-option cluster (`-rn`) whose first value-taking flag
/// is the last character, so its value is the next word. A value-taking flag
/// followed by more characters (`-n1`, `-Hroot`) has its value attached.
fn cluster_takes_next_word(w: &str, with_value: &[&str]) -> bool {
    if w.starts_with("--") {
        return false;
    }
    let mut flag = [0u8; 4];
    let body = &w[1..];
    for (idx, c) in body.char_indices() {
        let name = format!("-{}", c.encode_utf8(&mut flag));
        if with_value.contains(&name.as_str()) {
            return idx + c.len_utf8() == body.len();
        }
    }
    false
}

#[allow(clippy::too_many_lines)]
fn analyze_argv(argv: &[String], home: Option<&str>, depth: usize, hits: &mut Vec<Hit>) {
    if depth > MAX_DEPTH {
        return;
    }
    let mut i = 0;
    while i < argv.len() && (is_assignment(&argv[i]) || KEYWORDS.contains(&argv[i].as_str())) {
        let is_time = argv[i] == "time";
        i += 1;
        if is_time {
            // `time -p cmd` / `time -o file -f fmt cmd`: skip time's own options.
            i += skip_options(&argv[i..], &["-o", "-f", "--output", "--format"]);
        }
    }
    let Some(word) = argv.get(i) else { return };
    let prog = basename(word);
    let rest = &argv[i + 1..];
    let recurse = |from: usize, hits: &mut Vec<Hit>| {
        if from < rest.len() {
            analyze_argv(&rest[from..], home, depth + 1, hits);
        }
    };

    if PLAIN_WRAPPERS.contains(&prog) {
        let j = skip_options(
            rest,
            &[
                "-u",
                "-g",
                "-C",
                "-D",
                "-h",
                "-p",
                "-r",
                "-t",
                "-U",
                "-a",
                "--user",
                "--group",
                "--chdir",
                "--chroot",
                "--host",
                "--prompt",
                "--role",
                "--type",
                "--close-from",
                "--other-user",
                "--command-timeout",
                "--login-class",
                "--auth-type",
            ],
        );
        recurse(j, hits);
        return;
    }
    match prog {
        "command" => {
            // `command -v ssh` / `command -V ssh` only LOOK UP a name.
            if rest
                .first()
                .is_some_and(|o| o.starts_with('-') && (o.contains('v') || o.contains('V')))
            {
                return;
            }
            recurse(skip_options(rest, &[]), hits);
        }
        "env" => {
            let mut j = 0;
            while j < rest.len() {
                let w = rest[j].as_str();
                if w == "-S" || w == "--split-string" {
                    analyze_str(&rest[j + 1..].join(" "), home, depth + 1, hits);
                    return;
                }
                if let Some(s) = w
                    .strip_prefix("--split-string=")
                    .or_else(|| w.strip_prefix("-S").filter(|s| !s.is_empty()))
                {
                    let mut line = s.to_string();
                    for more in &rest[j + 1..] {
                        line.push(' ');
                        line.push_str(more);
                    }
                    analyze_str(&line, home, depth + 1, hits);
                    return;
                }
                if matches!(w, "-u" | "--unset" | "-C" | "--chdir") {
                    j += 2;
                } else if w == "--" {
                    j += 1;
                    break;
                } else if w.starts_with('-') || is_assignment(w) {
                    j += 1;
                } else {
                    break;
                }
            }
            recurse(j, hits);
        }
        "nice" | "ionice" | "stdbuf" | "chrt" | "taskset" => {
            // Each wrapper has its own option grammar: `-c` takes a value for
            // ionice but is the CPU-list *mode flag* for taskset, and `-i`/`-o`
            // are chrt policy flags but stdbuf buffer-mode values.
            let with_value: &[&str] = match prog {
                "nice" => &["-n", "--adjustment"],
                "ionice" => &[
                    "-c",
                    "-n",
                    "-p",
                    "-P",
                    "-u",
                    "--class",
                    "--classdata",
                    "--pid",
                    "--pgid",
                    "--uid",
                ],
                "stdbuf" => &["-i", "-o", "-e", "--input", "--output", "--error"],
                "chrt" => &[
                    "-T",
                    "-P",
                    "-D",
                    "--sched-runtime",
                    "--sched-deadline",
                    "--sched-period",
                ],
                // taskset: `-c`/`--cpu-list` only changes how the mask operand is
                // read; it takes no value of its own.
                _ => &[],
            };
            let j = skip_options(rest, with_value);
            // chrt takes a priority and taskset a mask / CPU-list operand before
            // the command; consume it exactly once.
            let j = if matches!(prog, "chrt" | "taskset") {
                j + 1
            } else {
                j
            };
            recurse(j, hits);
        }
        "timeout" => {
            // Options, then the DURATION operand, then the command.
            let j = skip_options(rest, &["-s", "-k", "--signal", "--kill-after"]);
            recurse(j + 1, hits);
        }
        "xargs" => {
            let j = skip_options(
                rest,
                &[
                    "-I",
                    "-n",
                    "-P",
                    "-L",
                    "-s",
                    "-d",
                    "-E",
                    "-a",
                    "--max-args",
                    "--max-procs",
                    "--delimiter",
                    "--arg-file",
                    "--max-chars",
                    "--process-slot-var",
                ],
            );
            recurse(j, hits);
        }
        "unbuffer" => {
            // `-p` (pipeline mode) and `-a` are plain flags here, not sudo's value-taking ones.
            let j = skip_options(rest, &[]);
            recurse(j, hits);
        }
        "watch" => {
            // `-d`/`--differences` takes an optional `=value` only, never the next word.
            let j = skip_options(rest, &["-n", "--interval"]);
            if j < rest.len() {
                analyze_str(&rest[j..].join(" "), home, depth + 1, hits);
            }
        }
        "flock" => {
            let j = skip_options(rest, &["-w", "--timeout", "-E", "--conflict-exit-code"]);
            // flock <lockfile> -c "<cmd>"  |  flock <lockfile> <cmd> …
            match rest.get(j + 1).map(String::as_str) {
                Some("-c" | "--command") => {
                    if let Some(s) = rest.get(j + 2) {
                        analyze_str(s, home, depth + 1, hits);
                    }
                }
                _ => recurse(j + 1, hits),
            }
        }
        "eval" => analyze_str(&rest.join(" "), home, depth + 1, hits),
        "find" => {
            let mut k = 0;
            while k < rest.len() {
                if matches!(rest[k].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
                    let end = rest[k + 1..]
                        .iter()
                        .position(|w| w == ";" || w == "+")
                        .map_or(rest.len(), |p| k + 1 + p);
                    analyze_argv(&rest[k + 1..end], home, depth + 1, hits);
                    k = end;
                }
                k += 1;
            }
        }
        "gh" => {
            if let Some(evidence) = gh_forge_secret(rest) {
                hits.push(Hit::new("forge-secrets", evidence));
            }
            if let Some((k, _)) = gh_operand(rest, &["pr"]) {
                if gh_operand(&rest[k + 1..], &["merge"]).is_some() {
                    hits.push(Hit::new(GH_PR_MERGE, GH_PR_MERGE));
                }
            }
        }
        // Multi-call binaries: the first operand is the applet.
        "busybox" | "toybox" => recurse(skip_options(rest, &[]), hits),
        // `su -c '<cmd>'`, `su - u -c …`, `script -qc '<cmd>' log`.
        "su" | "runuser" | "script" => {
            for (j, w) in rest.iter().enumerate() {
                let body = if let Some(v) = w.strip_prefix("--command=") {
                    Some(v)
                } else if w == "--command"
                    || w == "--session-command"
                    || (w.starts_with('-') && !w.starts_with("--") && w.ends_with('c'))
                {
                    rest.get(j + 1).map(String::as_str)
                } else {
                    None
                };
                if let Some(body) = body {
                    analyze_str(body, home, depth + 1, hits);
                }
            }
            // `runuser -u <user> -- <cmd> …` runs its operands directly.
            if prog == "runuser" {
                if let Some(p) = rest.iter().position(|w| w == "--") {
                    recurse(p + 1, hits);
                }
            }
        }
        // A tmux/screen operand is a shell command (`tmux new 'ssh h'`,
        // `screen ssh h`, `tmux send-keys 'ssh h' Enter`): analyze each one.
        // Over-denies only a session literally named after a capability.
        "tmux" | "screen" => {
            for w in rest {
                analyze_str(w, home, depth + 1, hits);
            }
        }
        // rsync runs ssh itself for `-e`/`--rsh` and for a `host:path` operand.
        "rsync" => {
            let rsh = rest.iter().any(|w| {
                w == "--rsh"
                    || w.starts_with("--rsh=")
                    || (w.starts_with('-') && !w.starts_with("--") && w.contains('e'))
            });
            let remote = rest.iter().any(|w| {
                !w.starts_with('-')
                    && (w.starts_with("rsync://")
                        || w.split_once(':')
                            .is_some_and(|(h, _)| !h.is_empty() && !h.contains('/')))
            });
            if rsh || remote {
                hits.push(Hit::new("remote-shell", "rsync"));
            }
        }
        "git" => git_hits(rest, home, depth, hits),
        p if SHELLS.contains(&p) => {
            // `bash -c '<cmd>'`, `sh -ec '<cmd>'`, `bash -o pipefail -c …`.
            let mut j = 0;
            while j < rest.len() {
                let w = rest[j].as_str();
                if matches!(w, "-o" | "+o" | "-O" | "+O") {
                    j += 2;
                    continue;
                }
                if w.starts_with('-') && !w.starts_with("--") && w[1..].contains('c') {
                    if let Some(s) = rest.get(j + 1) {
                        analyze_str(s, home, depth + 1, hits);
                    }
                    return;
                }
                if !w.starts_with('-') && !w.starts_with('+') {
                    break;
                }
                j += 1;
            }
        }
        p if REMOTE_SHELL_PROGRAMS.contains(&p) => hits.push(Hit::new("remote-shell", p)),
        p if CLOUD_CLI_PROGRAMS.contains(&p) => hits.push(Hit::new("cloud-cli", p)),
        _ => {}
    }
}

/// `git` words that run a command of the caller's choosing: a
/// `core.sshCommand` override (`git -c core.sshCommand=…`, `git config
/// core.sshCommand …`) is a remote shell outright; a `!`-alias body and an
/// `ext::` remote are command lines of their own. Plain `git fetch` over SSH
/// is forge transport and stays allowed.
fn git_hits(rest: &[String], home: Option<&str>, depth: usize, hits: &mut Vec<Hit>) {
    for w in rest {
        if w.to_ascii_lowercase().contains("core.sshcommand") {
            hits.push(Hit::new("remote-shell", "git core.sshCommand"));
        }
        let body = w
            .split_once("=!")
            .map(|(_, b)| b)
            .or_else(|| w.strip_prefix('!'))
            .or_else(|| w.strip_prefix("ext::"));
        if let Some(body) = body {
            analyze_str(body, home, depth + 1, hits);
        }
    }
}

/// The `forge-secrets` evidence for `gh <rest…>`, if it is one.
fn gh_forge_secret(rest: &[String]) -> Option<String> {
    const SENSITIVE: [&str; 4] = ["secret", "variable", "auth", "api"];
    let (k, sub) = gh_operand(rest, &SENSITIVE)?;
    match sub {
        "secret" | "variable" => Some(format!("gh {sub}")),
        "auth" => {
            const VERBS: [&str; 6] = ["token", "login", "refresh", "logout", "setup-git", "status"];
            let verb = gh_operand(&rest[k + 1..], &VERBS).map_or("", |(_, v)| v);
            let shows_token = rest
                .iter()
                .any(|w| w == "-t" || w == "--show-token" || w == "--show-token=true");
            match verb {
                "token" | "login" | "refresh" | "logout" | "setup-git" => {
                    Some(format!("gh auth {verb}"))
                }
                "status" if shows_token => Some("gh auth status --show-token".to_string()),
                _ => None,
            }
        }
        "api" => rest
            .iter()
            .find(|w| {
                let lower = w.to_ascii_lowercase();
                lower.contains("/secrets") || lower.contains("/variables")
            })
            .map(|w| format!("gh api {w}")),
        _ => None,
    }
}

/// The first `gh` operand among `wanted` and its index, past options and
/// their values. `-R`/`--repo`/`--hostname` always consume the next word. A
/// word right after any other bare option MAY be that option's value, so
/// when it is not in `wanted` the scan goes on instead of taking it as the
/// subcommand (`gh -R o/r secret list`, `gh --foo x api …`). The first
/// operand that cannot be a value and is not wanted ends the scan, so
/// `gh pr comment --body secret` stays a `pr` command.
fn gh_operand<'a>(words: &'a [String], wanted: &[&str]) -> Option<(usize, &'a str)> {
    let mut takes_value = false;
    let mut maybe_value = false;
    for (k, w) in words.iter().enumerate() {
        let w = w.as_str();
        if takes_value {
            takes_value = false;
            continue;
        }
        if w.starts_with('-') && w != "-" && w != "--" {
            takes_value = matches!(w, "-R" | "--repo" | "--hostname");
            maybe_value = !w.contains('=');
            continue;
        }
        if wanted.contains(&w) {
            return Some((k, w));
        }
        if !maybe_value {
            return None;
        }
        maybe_value = false;
    }
    None
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "command_match_tests.rs"]
mod tests;
