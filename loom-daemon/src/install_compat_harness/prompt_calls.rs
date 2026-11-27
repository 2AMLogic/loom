//! The `loom-daemon` calls in the shipped Markdown prompts (#15644).
//!
//! `REQUIRES_DAEMON` is the oldest daemon the installed files work with, and
//! direction B proves it against a release binary. Its `(file, subcommand)`
//! pairs come from the shell detector, which reads `*.sh`; the role and command
//! prompts under `defaults/.claude/commands/loom/` (which `defaults/roles/*.md`
//! symlink to) call `loom-daemon <verb>` directly, and neither the detector
//! nor the hard-floor walk saw them. A prompt could adopt a verb newer than
//! the floor with nothing forcing the floor to move: #15639 found it behind
//! `guide work-plan-receipt` and `forge workflow-secrets-check`.
//!
//! This module reads the prompts and hands direction B their `(prompt,
//! command path)` pairs, probed through the same cached [`super::Probe`] and
//! reported as the same gaps as the shell files'.
//!
//! # Extraction
//!
//! A call is the daemon in command position: the literal `loom-daemon`, a
//! path ending in it, or `${LOOM_DAEMON_BIN:-loom-daemon}`. It is looked for
//! in a shell fence, in a fence with no language, and in an inline code span;
//! never in prose, and never in a fence in another language. Inside those a
//! small shell lexer decides what is command position: `$( )`, backticks and
//! pipes start commands, `if`/`exec`/assignments are stepped over, redirects
//! and `<placeholders>` are operands, and comments and quoted prose are data
//! (`echo "run loom-daemon x"` is not a call). `{a,b}` is a brace list, so
//! `host {disable,enable,status}` is three calls. A code span may wrap onto
//! the next line of its block.
//!
//! # Resolution
//!
//! The words after the executable are walked down the NEW daemon's command
//! tree, and only words that resolve are probed: `guide work-plan-receipt
//! read` is three levels, `dispatch <N>` is one, and `forge issue create`
//! stops at `forge issue`, whose tail belongs to `gh`. The tree is the new
//! daemon's, not the floor's, so an old binary that knows only a parent
//! cannot make the missing child vanish from discovery. A first word the new
//! daemon does not know (`loom-daemon is running` in a prose-shaped fence)
//! resolves to nothing and is not probed.
//!
//! Limits, accepted: a variable a prompt assigns the daemon to under another
//! name is not seen; a heredoc body is read as further commands (a daemon
//! call in one is a true dependency anyway); the reference docs under
//! `defaults/docs/`, the role symlinks and the generated skill copies are not
//! scanned (#15644 leaves them out).

use super::Gaps;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Where the prompts live under a `defaults/` tree.
pub const PROMPT_DIR: &str = ".claude/commands/loom";

/// Fence languages whose body is shell.
const SHELL_FENCES: [&str; 6] = ["bash", "sh", "zsh", "ksh", "dash", "shell"];

/// Fence languages whose body is Markdown: a prompt quoting a prompt.
const MARKDOWN_FENCES: [&str; 2] = ["markdown", "md"];

/// How deep Markdown inside Markdown is followed. A nested fence needs a
/// longer fence than the one around it, so this only bounds a pathological
/// file.
const MAX_MARKDOWN_DEPTH: usize = 8;

/// Words that can precede a command word, as in the shell detector.
const LEADING: [&str; 15] = [
    "!", "{", "}", "if", "then", "else", "elif", "do", "while", "until", "exec", "command", "time",
    "env", "sudo",
];

/// The NEW daemon's command tree, which a prompt's call words are resolved
/// against before the floor binary is probed.
#[derive(Debug, Default, Clone)]
pub struct CommandTree {
    root: Node,
}

#[derive(Debug, Default, Clone)]
struct Node {
    /// The subcommand's own name, when it was reached through an alias.
    name: String,
    /// Subcommands, by name and by alias.
    children: BTreeMap<String, Node>,
    /// The command takes a trailing catch-all argument (`forge issue <args…>`
    /// forwards to `gh`): every word after it is that argument's data, not a
    /// deeper command.
    passthrough: bool,
}

/// One invocation of the daemon: for each word after the executable, the
/// literals it can be (`{disable,enable}` is two), or none when the word is
/// not a literal. Empty for the bare executable.
pub(super) type Call = Vec<Vec<String>>;

impl CommandTree {
    /// The `loom-daemon` command tree, read from the `Cli` the library gives
    /// the daemon binary. `loom-tools install-compat check` links the same
    /// one, so both binaries resolve a prompt's call the same way.
    pub fn loom_daemon() -> Self {
        let mut cli = <crate::cli::Cli as clap::CommandFactory>::command();
        cli.build();
        Self::from_clap(&cli)
    }

    /// The tree under `cmd`: every subcommand, by name and by alias.
    #[must_use]
    pub fn from_clap(cmd: &clap::Command) -> Self {
        Self {
            root: Node::of(cmd),
        }
    }

    /// Add `path` (space-separated words). `passthrough` marks its last word
    /// the end of the command. For fixtures; the real tree comes from
    /// [`Self::loom_daemon`].
    #[cfg(test)]
    pub(super) fn insert(&mut self, path: &str, passthrough: bool) {
        let mut node = &mut self.root;
        for word in path.split_whitespace() {
            node = node
                .children
                .entry(word.to_string())
                .or_insert_with(|| Node {
                    name: word.to_string(),
                    ..Node::default()
                });
        }
        node.passthrough = passthrough;
    }

    /// The command paths `call` names here, one per alternative it contains,
    /// each spelled with its subcommands' own names rather than aliases. A
    /// path ends where its words stop being this daemon's subcommands, so
    /// flags, values and a passthrough's tail are not part of it. Empty when
    /// the first word is no command of this daemon.
    pub(super) fn resolve(&self, call: &Call) -> Vec<String> {
        let mut done: Vec<String> = Vec::new();
        let mut frontier: Vec<(&Node, String)> = vec![(&self.root, String::new())];
        for alternatives in call {
            if alternatives.is_empty() {
                break;
            }
            let mut next = Vec::new();
            for (node, path) in &frontier {
                for alternative in alternatives {
                    match node.children.get(alternative) {
                        Some(child) => {
                            let extended = if path.is_empty() {
                                child.name.clone()
                            } else {
                                format!("{} {}", path, child.name)
                            };
                            if child.passthrough {
                                done.push(extended);
                            } else {
                                next.push((child, extended));
                            }
                        }
                        None => done.push(path.clone()),
                    }
                }
            }
            frontier = next;
        }
        done.extend(frontier.into_iter().map(|(_, path)| path));
        done.retain(|path| !path.is_empty());
        done.sort();
        done.dedup();
        done
    }
}

impl Node {
    fn of(cmd: &clap::Command) -> Self {
        let passthrough = cmd
            .get_positionals()
            .any(clap::Arg::is_trailing_var_arg_set);
        let mut children = BTreeMap::new();
        for sub in cmd.get_subcommands() {
            let node = Self::of(sub);
            for name in std::iter::once(sub.get_name()).chain(sub.get_all_aliases()) {
                children.insert(name.to_string(), node.clone());
            }
        }
        Self {
            name: cmd.get_name().to_string(),
            children,
            passthrough,
        }
    }
}

/// What the prompts give direction B.
#[derive(Debug, Default)]
pub(super) struct PromptCalls {
    /// `(prompt, command path)`, one per prompt per path it calls, so a gap
    /// names every prompt that hits it.
    pub pairs: BTreeSet<(PathBuf, String)>,
    /// The distinct command paths among them: what is probed.
    paths: BTreeSet<String>,
    /// Prompt files read.
    files: usize,
}

impl PromptCalls {
    /// The note for what was scanned and what `gaps` found.
    pub(super) fn summary(&self, label: &str, gaps: &Gaps) -> String {
        gaps.with_probes(format!(
            "{label}: {} prompt files, {} command paths, {} subcommand gaps",
            self.files,
            self.paths.len(),
            gaps.missing.len()
        ))
    }
}

/// The calls in `prompts` (from [`super::shipped_prompts`]) that `commands`
/// resolves.
pub(super) fn scan(prompts: &[PathBuf], commands: &CommandTree) -> PromptCalls {
    let mut out = PromptCalls::default();
    for file in prompts {
        let Ok(text) = std::fs::read_to_string(file) else {
            continue;
        };
        out.files += 1;
        for call in markdown(&text) {
            for path in commands.resolve(&call) {
                out.pairs.insert((file.clone(), path.clone()));
                out.paths.insert(path);
            }
        }
    }
    out
}

/// Every daemon invocation in one Markdown document.
pub(super) fn markdown(text: &str) -> Vec<Call> {
    read_markdown(text, 0)
}

/// The same, `depth` levels of Markdown nested in a fence.
fn read_markdown(text: &str, depth: usize) -> Vec<Call> {
    fn read_block(block: &mut Vec<&str>, calls: &mut Vec<Call>) {
        for span in code_spans(&block.join("\n")) {
            calls.extend(invocations(&lex(&span)));
        }
        block.clear();
    }

    let mut calls = Vec::new();
    let mut block: Vec<&str> = Vec::new();
    let mut fence: Option<Fence> = None;
    for line in text.lines() {
        if let Some(mut open) = fence.take() {
            if closes_fence(line, open.marker, open.len) {
                calls.extend(read_fence(&open, depth));
            } else {
                open.body.push_str(line);
                open.body.push('\n');
                fence = Some(open);
            }
            continue;
        }
        if let Some((marker, len, info)) = opens_fence(line) {
            read_block(&mut block, &mut calls);
            fence = Some(Fence {
                marker,
                len,
                info: info.to_string(),
                body: String::new(),
            });
            continue;
        }
        let heading = is_heading(line);
        if heading || starts_block(line) {
            read_block(&mut block, &mut calls);
        }
        block.push(line);
        if heading {
            read_block(&mut block, &mut calls);
        }
    }
    read_block(&mut block, &mut calls);
    if let Some(open) = fence {
        calls.extend(read_fence(&open, depth));
    }
    calls
}

struct Fence {
    marker: char,
    len: usize,
    info: String,
    body: String,
}

/// The calls in a fence. Shell when it says so or says nothing; Markdown when
/// it says that; a fence with no language gets both readings; a fence in
/// another language is not read.
fn read_fence(fence: &Fence, depth: usize) -> Vec<Call> {
    let lang = fence.info.split_whitespace().next().unwrap_or_default();
    let mut calls = Vec::new();
    if lang.is_empty() || SHELL_FENCES.contains(&lang) {
        calls.extend(invocations(&lex(&fence.body)));
    }
    if depth < MAX_MARKDOWN_DEPTH && (lang.is_empty() || MARKDOWN_FENCES.contains(&lang)) {
        calls.extend(read_markdown(&fence.body, depth + 1));
    }
    calls
}

/// A line without its indentation, up to the three spaces CommonMark allows
/// in front of a fence.
fn dedent(line: &str) -> &str {
    line.strip_prefix("   ")
        .or_else(|| line.strip_prefix("  "))
        .or_else(|| line.strip_prefix(' '))
        .unwrap_or(line)
}

/// The fence `line` opens, as its marker, the marker's run length and its
/// info string.
fn opens_fence(line: &str) -> Option<(char, usize, &str)> {
    let rest = dedent(line);
    let marker = rest.chars().next()?;
    if !matches!(marker, '`' | '~') {
        return None;
    }
    let len = rest.chars().take_while(|&c| c == marker).count();
    if len < 3 || (marker == '`' && rest[len..].contains('`')) {
        return None;
    }
    Some((marker, len, rest[len..].trim()))
}

/// Whether `line` closes a fence of `marker` that opened `len` or more long.
fn closes_fence(line: &str, marker: char, len: usize) -> bool {
    let rest = dedent(line);
    let run = rest.chars().take_while(|&c| c == marker).count();
    run >= len && rest[run..].trim_matches(['\t', ' ']).is_empty()
}

/// Whether `line` is an ATX heading, a block of its own whatever follows.
fn is_heading(line: &str) -> bool {
    let t = line.trim_start();
    let after = t.trim_start_matches('#');
    after.len() < t.len() && after.starts_with(' ')
}

/// Whether `line` starts a block of its own: a blank line, a list item or a
/// table row. A code span cannot cross from one block into the next.
fn starts_block(line: &str) -> bool {
    let t = line.trim();
    t.is_empty()
        || t.starts_with('|')
        || ["- ", "* ", "+ "].iter().any(|b| t.starts_with(b))
        || t.split_once(['.', ')']).is_some_and(|(number, rest)| {
            !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
                && rest.starts_with(' ')
        })
}

/// The code spans of one Markdown block. A span opens with a backtick run and
/// closes at the next run of the same length.
fn code_spans(block: &str) -> Vec<String> {
    let c: Vec<char> = block.chars().collect();
    let run_at = |at: usize| c[at..].iter().take_while(|&&ch| ch == '`').count();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < c.len() {
        if c[i] == '\\' {
            i += 2;
        } else if c[i] != '`' {
            i += 1;
        } else {
            let open = run_at(i);
            let mut j = i + open;
            let mut close = None;
            while j < c.len() {
                if c[j] != '`' {
                    j += 1;
                    continue;
                }
                let run = run_at(j);
                if run == open {
                    close = Some(j);
                    break;
                }
                j += run;
            }
            match close {
                Some(j) => {
                    spans.push(code_span(&c[i + open..j]));
                    i = j + open;
                }
                None => i += open,
            }
        }
    }
    spans
}

/// A code span's content on one line, the way CommonMark renders it: line
/// endings become spaces, and the one leading and trailing space that would
/// otherwise be stripped are stripped.
fn code_span(c: &[char]) -> String {
    let text: String = c
        .iter()
        .map(|&ch| if ch == '\n' { ' ' } else { ch })
        .collect();
    if text.len() > 1 && text.starts_with(' ') && text.ends_with(' ') && !text.trim().is_empty() {
        text[1..text.len() - 1].to_string()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// Shell
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Word {
    /// The word with its quotes removed.
    text: String,
    /// No `$…` or `` `…` `` inside: the daemon would see `text` as it is.
    literal: bool,
    /// A redirect target or `<placeholder>`: an operand, never a command word.
    operand: bool,
}

/// Split shell-ish text into its simple commands, in source order, including
/// those inside `$( )`, backticks and `<( )`.
///
/// Each line is contained: an unclosed quote ends at its newline instead of
/// swallowing the rest of the block, and a newline ends a command unless a
/// backslash continues it.
// The trailing `end_command!()` resets state a final time that nothing reads.
#[allow(unused_assignments)]
fn lex(text: &str) -> Vec<Vec<Word>> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut command: Vec<Word> = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut literal = true;
    let mut operand = false;
    let mut redirect = false;
    let mut i = 0;

    macro_rules! start_word {
        () => {{
            in_word = true;
            operand = redirect;
            redirect = false;
        }};
    }
    macro_rules! end_word {
        () => {
            if in_word {
                command.push(Word {
                    text: std::mem::take(&mut cur),
                    literal,
                    operand,
                });
                in_word = false;
                literal = true;
                operand = false;
            }
        };
    }
    macro_rules! end_command {
        () => {
            end_word!();
            redirect = false;
            if !command.is_empty() {
                out.push(std::mem::take(&mut command));
            }
        };
    }
    // The body of a `$( )` or `<( )` is a command of its own.
    macro_rules! substituted {
        ($start:expr) => {{
            start_word!();
            let (body, end) = take_balanced(&c, $start);
            out.extend(lex(&body));
            literal = false;
            i = end;
        }};
    }
    macro_rules! backticked {
        ($start:expr) => {{
            start_word!();
            let (body, end) = take_backtick(&c, $start);
            out.extend(lex(&body));
            literal = false;
            i = end;
        }};
    }
    macro_rules! braced {
        () => {{
            start_word!();
            let end = take_brace(&c, i + 2);
            cur.extend(&c[i..end]);
            literal = false;
            i = end;
        }};
    }
    // `<`, `>>`, `2>&1`: the next word is the redirect's target.
    macro_rules! redirect {
        () => {{
            end_word!();
            while matches!(c.get(i), Some('<' | '>')) {
                i += 1;
            }
            if c.get(i) == Some(&'&') {
                i += 1;
            }
            redirect = true;
        }};
    }

    while i < c.len() {
        match c[i] {
            '\\' if c.get(i + 1) == Some(&'\n') => i += 2,
            '\\' => {
                if let Some(&escaped) = c.get(i + 1) {
                    cur.push(escaped);
                    start_word!();
                    i += 1;
                }
                i += 1;
            }
            '\'' => {
                start_word!();
                i = quoted(&c, i + 1, &mut cur);
            }
            '"' => {
                start_word!();
                i += 1;
                while i < c.len() && !matches!(c[i], '"' | '\n') {
                    match c[i] {
                        '\\' if c.get(i + 1) == Some(&'\n') => i += 2,
                        '\\' if matches!(c.get(i + 1), Some('$' | '`' | '"' | '\\')) => {
                            cur.push(c[i + 1]);
                            i += 2;
                        }
                        '$' if c.get(i + 1) == Some(&'(') => substituted!(i + 2),
                        '$' if c.get(i + 1) == Some(&'{') => braced!(),
                        '`' => backticked!(i + 1),
                        other => {
                            cur.push(other);
                            i += 1;
                        }
                    }
                }
                if i < c.len() && c[i] == '"' {
                    i += 1;
                }
            }
            '$' if c.get(i + 1) == Some(&'(') => substituted!(i + 2),
            '$' if c.get(i + 1) == Some(&'{') => braced!(),
            '`' => backticked!(i + 1),
            '$' => {
                start_word!();
                cur.push('$');
                literal = false;
                i += 1;
            }
            '#' if !in_word => {
                while i < c.len() && c[i] != '\n' {
                    i += 1;
                }
            }
            '<' | '>' if c.get(i + 1) == Some(&'(') => substituted!(i + 2),
            '<' => {
                end_word!();
                match placeholder_len(&c, i) {
                    Some(len) => {
                        command.push(Word {
                            text: c[i..i + len].iter().collect(),
                            literal: true,
                            operand: true,
                        });
                        i += len;
                    }
                    None => redirect!(),
                }
            }
            '>' => redirect!(),
            ' ' | '\t' | '\r' => {
                end_word!();
                i += 1;
            }
            '\n' | ';' | '&' | '|' | '(' | ')' => {
                end_command!();
                i += 1;
            }
            other => {
                start_word!();
                cur.push(other);
                i += 1;
            }
        }
    }
    end_command!();
    out
}

/// Copy the `'`-quoted run starting at `start` into `out` and return the
/// index just past its closing quote. An unclosed run ends at its newline,
/// which is left to the caller.
fn quoted(c: &[char], start: usize, out: &mut String) -> usize {
    let mut i = start;
    while i < c.len() && !matches!(c[i], '\'' | '\n') {
        out.push(c[i]);
        i += 1;
    }
    if i < c.len() && c[i] == '\'' {
        i += 1;
    }
    i
}

/// The body of a `(`-opened region whose body starts at `start`, and the
/// index just past its `)`. Quotes inside are skipped, so a `)` in a string
/// does not close it; an unclosed region runs to the end.
fn take_balanced(c: &[char], start: usize) -> (String, usize) {
    let mut depth = 1usize;
    let mut i = start;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '\'' => i = quoted(c, i + 1, &mut String::new()),
            '"' => i = double_quoted(c, i + 1, &mut String::new()),
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return (c[start..i].iter().collect(), i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    (c[start..].iter().collect(), c.len())
}

/// The body of a backtick substitution whose body starts at `start`, with
/// `\` escapes decoded, and the index just past its closing backtick.
fn take_backtick(c: &[char], start: usize) -> (String, usize) {
    let mut body = String::new();
    let mut i = start;
    while i < c.len() && c[i] != '`' {
        if c[i] == '\\' && i + 1 < c.len() {
            body.push(c[i + 1]);
            i += 2;
        } else {
            body.push(c[i]);
            i += 1;
        }
    }
    (body, (i + 1).min(c.len()))
}

/// The index just past the `}` closing a `${` whose body starts at `start`.
fn take_brace(c: &[char], start: usize) -> usize {
    let mut depth = 1usize;
    let mut i = start;
    while i < c.len() {
        match c[i] {
            '\\' => i += 1,
            '\'' => i = quoted(c, i + 1, &mut String::new()),
            '"' => i = double_quoted(c, i + 1, &mut String::new()),
            '$' if c.get(i + 1) == Some(&'(') => {
                i = take_balanced(c, i + 2).1;
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

/// Like [`quoted`] for `"`, with the escapes a double-quoted string decodes.
fn double_quoted(c: &[char], start: usize, out: &mut String) -> usize {
    let mut i = start;
    while i < c.len() && !matches!(c[i], '"' | '\n') {
        match c[i] {
            '\\' if matches!(c.get(i + 1), Some('$' | '`' | '"' | '\\')) => {
                out.push(c[i + 1]);
                i += 2;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    if i < c.len() && c[i] == '"' {
        i += 1;
    }
    i
}

/// The length of the `<placeholder>` starting at `start`, the notation
/// prompts use for a value (`<number>`, `<sweep-id>`); `None` when the `<` is
/// a redirect.
fn placeholder_len(c: &[char], start: usize) -> Option<usize> {
    let name = start + 1;
    let mut end = name;
    while end < c.len() && (c[end].is_ascii_alphanumeric() || "_.,:/%@+-".contains(c[end])) {
        end += 1;
    }
    (end > name && c.get(end) == Some(&'>')).then_some(end + 1 - start)
}

/// The daemon invocations among `commands`: for each, the words after the
/// executable.
fn invocations(commands: &[Vec<Word>]) -> Vec<Call> {
    commands
        .iter()
        .filter_map(|command| {
            let mut words = command.iter();
            let executable = words
                .by_ref()
                .find(|w| !(LEADING.contains(&w.text.as_str()) || is_assignment(&w.text)))?;
            (!executable.operand && is_daemon(&executable.text)).then(|| {
                words
                    .take_while(|w| w.literal && !w.operand)
                    .map(|w| brace_list(&w.text))
                    .collect()
            })
        })
        .collect()
}

/// Whether `word` is a `NAME=value` assignment in front of a command.
fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty()
            && !name.starts_with(|c: char| c.is_ascii_digit())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

/// Whether `word` names the daemon binary: the literal, a path ending in it,
/// or the resolver variables the shell detector knows, with or without a
/// `:-` default.
fn is_daemon(word: &str) -> bool {
    if word == "loom-daemon" || word.ends_with("/loom-daemon") {
        return true;
    }
    let Some(expansion) = word.strip_prefix('$') else {
        return false;
    };
    let braced = expansion.starts_with('{');
    let name = expansion.trim_start_matches('{');
    let name = name.split([':', '}']).next().unwrap_or_default();
    if !matches!(name, "LOOM_DAEMON_BIN" | "LOOM_DAEMON_SELF_BIN") {
        return false;
    }
    let tail = &expansion[name.len() + usize::from(braced)..];
    match tail {
        "" => !braced,
        "}" => braced,
        _ => braced && tail.starts_with(":-") && tail.ends_with('}'),
    }
}

/// The words a brace list in `word` expands to: `{disable,enable}` is two.
/// One word when there is none; none when `word` cannot be a command word
/// (`{abc}` stays literal in bash, `{1..3}` is a sequence).
fn brace_list(word: &str) -> Vec<String> {
    let Some(open) = word.find('{') else {
        return vec![word.to_string()];
    };
    let mut depth = 0usize;
    let mut alternatives: Vec<&str> = Vec::new();
    let mut from = open + 1;
    let mut close = None;
    for (i, b) in word.bytes().enumerate().skip(open + 1) {
        match b {
            b'{' => depth += 1,
            b'}' if depth == 0 => {
                close = Some(i);
                alternatives.push(&word[from..i]);
                break;
            }
            b'}' => depth -= 1,
            b',' if depth == 0 => {
                alternatives.push(&word[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    let Some(close) = close else {
        return vec![word.to_string()];
    };
    if alternatives.len() < 2 || alternatives.iter().any(|a| a.contains("..")) {
        return Vec::new();
    }
    let (prefix, suffix) = (&word[..open], &word[close + 1..]);
    alternatives
        .iter()
        .flat_map(|a| brace_list(&format!("{prefix}{a}{suffix}")))
        .collect()
}
