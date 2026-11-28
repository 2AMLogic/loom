//! Project-level hook wiring for `.claude/settings.json` (#13793).
//!
//! The daemon resync used to write only payload files, so a hook script that
//! landed under `.loom/hooks/` stayed unwired in every fleet repo until an
//! operator ran `resync-installed.sh`. This module owns the same
//! `(event, matcher, script, timeout_s)` table as
//! `scripts/install/provision-hooks.sh` (a drift test compares the two) and
//! adds the missing entries additively.
//!
//! Rules, identical to `ensure_project_hook_wiring`:
//! * only a hook whose script exists under `.loom/hooks/` counts;
//! * "wired" means some command under the same event contains
//!   `.loom/hooks/<name>` (and is not the machine-level `defaults/hooks/`
//!   wrapper). A Repo Skills `.claude/skills/repo/hooks/…` path never matches;
//! * a missing entry is appended in the launcher form ([`project_command`]),
//!   the shape every Loom hook entry routes through
//!   (`defaults/hooks/hook-wiring.sh`, #7761). Until #15741 this module wrote
//!   the bare `"${CLAUDE_PROJECT_DIR}/.loom/hooks/<name>"` instead, which runs
//!   the script and nothing else — no broken-install signal, and no deny when
//!   a PreToolUse guard is missing. Nothing is removed or reordered;
//! * an entry an earlier Loom writer left in that bare form (quoted, or
//!   unquoted before #6544) is upgraded to the launcher form in place (#15741).
//!   Only those two exact commands are Loom's: a hand-written variant that
//!   merely references the same script is never touched;
//! * a timeout-bearing hook converges a Loom-authored entry to the table's
//!   `timeout_s` in place (#13959) instead of leaving pre-timeout wiring
//!   unbounded;
//! * a file that is not a JSON object, or whose `hooks` shape is unexpected,
//!   is left untouched and reported.

use serde_json::{json, Map, Value};

/// `(event, matcher, script, timeout_s)`; mirrors
/// `_PHOOK_TYPES/_MATCHERS/_NAMES/_TIMEOUTS`. `timeout_s` is the Claude Code
/// hook `timeout` (#13959): the seconds after which the client kills a hung
/// hook instead of letting it stall session start. `None` emits no field.
/// Only the SessionStart gh front carries one today: its worst measured step
/// is ~0.1 s (`gh-shim session-env`), the unresolved worst case (a binary
/// lookup that falls through to `cargo metadata`, a cold binary start under
/// load) is seconds, and the reported stall was 30–50 s — 10 s bounds the
/// pathology with two orders of magnitude of headroom over anything healthy.
/// The hook is fail-open and stdout-silent, so a killed run costs only the
/// PATH line, the same degradation as any other resolution failure.
pub(crate) const HOOKS: &[(&str, &str, &str, Option<u64>)] = &[
    ("PreToolUse", "Bash", "guard-destructive.sh", None),
    ("PreToolUse", "Bash", "guard-loom-workflow.sh", None),
    ("PreToolUse", "Edit|Write", "guard-worktree-paths.sh", None),
    ("PreToolUse", "mcp__loom__.*", "guard-mcp-tools.sh", None),
    ("UserPromptSubmit", "", "skill-router.sh", None),
    ("UserPromptSubmit", "", "methodology-inject.sh", None),
    ("Stop", "", "guard-background-subagents.sh", None),
    ("SessionStart", "", "gh-front-env.sh", Some(10)),
    ("PreToolUse", "*", "roll-pause.sh", None),
    ("PostToolUse", "*", "roll-pause.sh", None),
    ("PostToolUseFailure", "*", "roll-pause.sh", None),
    ("Stop", "", "guard-uncommitted-work.sh", None),
    ("SubagentStop", "", "guard-uncommitted-work.sh", None),
];

/// The result of [`wire`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Wired {
    /// Every present hook is already wired.
    Unchanged,
    /// The new file text.
    Changed(String),
    /// Left as is: why.
    Unusable(String),
}

/// PreToolUse hooks that are not guards: a missing copy must allow. Only
/// `roll-pause.sh` (#10830), which parks a tool call for a roll and is wired
/// match-all — failing closed there would block every tool call in the
/// checkout. Mirrors `_PHOOK_FAILOPEN` in `scripts/install/provision-hooks.sh`.
const FAIL_OPEN: &[&str] = &["roll-pause.sh"];

/// The `bash -c '...'` body of a project hook command ([`project_command`]).
/// The caller single-quotes it, so it carries no single quote of its own — that
/// is what lets it survive JSON round-tripping. Every `$` is expanded by the
/// hook's own shell at hook time, never here. `@EVENT@`, `@NAME@` and `@DENY@`
/// are filled in by [`project_command`].
///
/// The ladder, in order (`defaults/hooks/hook-wiring.sh`, whose fallback this
/// carries inline for a host whose `.loom/hooks/` lags the launcher):
/// 1. `$W` is the worktree-aware main checkout, else `CLAUDE_PROJECT_DIR`,
///    else the cwd. `git rev-parse`'s output is emptiness-tested BEFORE the
///    `cd`: an empty substitution would make `cd /..` succeed at `/`.
/// 2. The launcher runs the hook when it is readable and owns everything from
///    there.
/// 3. Otherwise the hook runs inline: directly when executable, via `bash`
///    when merely readable, so a lost `+x` bit costs no coverage.
/// 4. A workspace with no `.loom/hooks/` is not a Loom workspace: allow.
/// 5. Otherwise the install is broken: say so on stderr, and [`DENY`] for a
///    PreToolUse guard.
const BODY: &str = r##"R=$(git rev-parse --git-common-dir 2>/dev/null); R=${R:+$(cd "$R/.." 2>/dev/null && pwd)}; W=${R:-${CLAUDE_PROJECT_DIR:-$PWD}}; L=$W/.loom/hooks/hook-wiring.sh; [ -r "$L" ] && LOOM_PROJECT_ROOT=$W exec bash "$L" @EVENT@ @NAME@; G=$W/.loom/hooks/@NAME@; [ -x "$G" ] && exec "$G"; [ -r "$G" ] && exec bash "$G"; [ -d "$W/.loom/hooks" ] || exit 0; echo "[loom] BROKEN GUARD INSTALL: @NAME@ is not installed in this Loom workspace ($W/.loom/hooks/@NAME@); re-run scripts/install-loom.sh." >&2; @DENY@exit 0"##;

/// Rung 5 for a PreToolUse guard: a deny, expressed the way every Loom guard
/// expresses one — exit 0 plus a `hookSpecificOutput.permissionDecision`
/// document, never a non-zero exit that could wedge the client in a retry
/// loop. Emitted with `printf`, not `jq`: a host missing `jq` must still get
/// the decision. `LOOM_GUARD_WIRING_FAILOPEN=1` downgrades it to the
/// warn-and-allow every other event gets.
const DENY: &str = r##"[ "${LOOM_GUARD_WIRING_FAILOPEN:-0}" = "1" ] && exit 0; printf %s "{\"hookSpecificOutput\":{\"hookEventName\":\"@EVENT@\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"Loom hook @NAME@ is not installed in this workspace (no .loom/hooks/hook-wiring.sh and no .loom/hooks/@NAME@), but .loom/hooks/ exists, so this workspace does carry per-repo hook copies and expects this one. A missing guard is a broken install, not an opt-out, and this session runs without permission prompts. Repair: re-run scripts/install-loom.sh (or .loom/scripts/check-guards-installed.sh), then restart Claude Code. Set LOOM_GUARD_WIRING_FAILOPEN=1 in the environment to downgrade this deny to a warning.\"}}"; "##;

/// The command a project-level `.claude/settings.json` entry runs to execute
/// `name` on `event`.
///
/// Byte-identical to `_phook_project_cmd` in
/// `scripts/install/provision-hooks.sh`, which writes the same entries from the
/// installer; a test generates every table row through both and compares.
fn project_command(event: &str, name: &str) -> String {
    // The escape hatch is exported inside the hook's own shell, so only that
    // process sees it — hook-wiring.sh's one committed exception (#10830).
    let pre = if event == "PreToolUse" && FAIL_OPEN.contains(&name) {
        "export LOOM_GUARD_WIRING_FAILOPEN=1; "
    } else {
        ""
    };
    let deny = if event == "PreToolUse" {
        DENY.replace("@EVENT@", event).replace("@NAME@", name)
    } else {
        String::new()
    };
    let body = BODY
        .replace("@DENY@", &deny)
        .replace("@EVENT@", event)
        .replace("@NAME@", name);
    format!("bash -c '{pre}{body}'")
}

/// The bare `${CLAUDE_PROJECT_DIR}` commands pre-#15741 Loom writers emitted
/// for `name`: quoted since #6544, unquoted before. Besides [`project_command`]'s
/// output, the only commands that count as Loom-authored.
fn bare_forms(name: &str) -> [String; 2] {
    [
        format!("\"${{CLAUDE_PROJECT_DIR}}/.loom/hooks/{name}\""),
        format!("${{CLAUDE_PROJECT_DIR}}/.loom/hooks/{name}"),
    ]
}

fn is_wired(entries: &[Value], name: &str) -> bool {
    let marker = format!(".loom/hooks/{name}");
    entries
        .iter()
        .filter_map(|e| e.get("hooks").and_then(Value::as_array))
        .flatten()
        .filter_map(|h| h.get("command").and_then(Value::as_str))
        .any(|c| c.contains(&marker) && !c.contains("defaults/hooks/"))
}

/// Bring the Loom-authored entries for `name` on `event` up to date
/// (#13959, #15741): a bare-form command is rewritten to the launcher form,
/// and a launcher-form entry of a timeout-bearing hook gets the table's
/// `timeout`. Returns whether anything changed. Nothing else is touched: only
/// [`project_command`]'s output and the two [`bare_forms`] are Loom's, so a
/// hand-written variant that merely references the same script is left alone.
fn converge(entries: &mut [Value], event: &str, name: &str, timeout: Option<u64>) -> bool {
    let ours = project_command(event, name);
    let bare = bare_forms(name);
    let mut changed = false;
    for hook in entries
        .iter_mut()
        .filter_map(|e| e.get_mut("hooks").and_then(Value::as_array_mut))
        .flatten()
    {
        let command = hook.get("command").and_then(Value::as_str);
        let is_bare = bare.iter().any(|b| Some(b.as_str()) == command);
        if command != Some(ours.as_str()) && !is_bare {
            continue;
        }
        let Some(obj) = hook.as_object_mut() else {
            continue;
        };
        if is_bare {
            obj.insert("command".to_string(), Value::String(ours.clone()));
            changed = true;
        }
        if let Some(t) = timeout {
            if obj.get("timeout").and_then(Value::as_u64) != Some(t) {
                obj.insert("timeout".to_string(), json!(t));
                changed = true;
            }
        }
    }
    changed
}

/// Add the missing entries for every hook `present` reports to `text`.
pub(crate) fn wire(text: &str, present: impl Fn(&str) -> bool) -> Wired {
    let mut root: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => return Wired::Unusable(format!("not valid JSON: {e}")),
    };
    let Some(obj) = root.as_object_mut() else {
        return Wired::Unusable("top level is not a JSON object".to_string());
    };
    let mut changed = false;
    for (event, matcher, name, timeout) in HOOKS {
        if !present(name) {
            continue;
        }
        let hooks = obj
            .entry("hooks")
            .or_insert_with(|| Value::Object(Map::new()));
        let Some(hooks) = hooks.as_object_mut() else {
            return Wired::Unusable("`hooks` is not an object".to_string());
        };
        let entries = hooks
            .entry((*event).to_string())
            .or_insert_with(|| Value::Array(Vec::new()));
        let Some(entries) = entries.as_array_mut() else {
            return Wired::Unusable(format!("`hooks.{event}` is not an array"));
        };
        if is_wired(entries, name) {
            changed |= converge(entries, event, name, *timeout);
            continue;
        }
        let command = project_command(event, name);
        let new_hook = match timeout {
            Some(t) => json!({"type": "command", "command": command, "timeout": t}),
            None => json!({"type": "command", "command": command}),
        };
        let slot = entries.iter_mut().find(|e| {
            e.get("matcher").and_then(Value::as_str).unwrap_or("") == *matcher
                && e.get("hooks").is_none_or(Value::is_array)
        });
        match slot {
            Some(entry) => {
                let list = entry
                    .as_object_mut()
                    .map(|o| o.entry("hooks").or_insert_with(|| Value::Array(Vec::new())))
                    .and_then(Value::as_array_mut);
                match list {
                    Some(list) => list.push(new_hook),
                    None => return Wired::Unusable(format!("`hooks.{event}` entry is malformed")),
                }
            }
            None => entries.push(json!({"matcher": matcher, "hooks": [new_hook]})),
        }
        changed = true;
    }
    if !changed {
        return Wired::Unchanged;
    }
    match serde_json::to_string_pretty(&root) {
        Ok(mut s) => {
            s.push('\n');
            Wired::Changed(s)
        }
        Err(e) => Wired::Unusable(format!("cannot serialize: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use tempfile::TempDir;

    const SCRIPT: &str = include_str!("../../../scripts/install/provision-hooks.sh");

    /// `scripts/install/provision-hooks.sh` on disk, quoted for `source`.
    fn provision_hooks_sh() -> String {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/install/provision-hooks.sh")
            .to_string_lossy()
            .replace('\'', "'\\''")
    }

    /// Every table row's project command, generated by the installer's own
    /// `_phook_project_cmd` in bash.
    fn shell_commands() -> Vec<String> {
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "source '{}'; for i in \"${{!_PHOOK_NAMES[@]}}\"; do \
                 _phook_project_cmd \"${{_PHOOK_TYPES[i]}}\" \"${{_PHOOK_NAMES[i]}}\"; printf '\\n'; done",
                provision_hooks_sh()
            ))
            .output()
            .expect("bash is required for the provision-hooks.sh drift tests");
        assert!(
            out.status.success(),
            "_phook_project_cmd failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// Run the installer's `ensure_project_hook_wiring` on `dir`, a stand-in
    /// consumer repo, and return the project settings it leaves behind.
    fn shell_wire(dir: &Path) -> String {
        let out = Command::new("bash")
            .arg("-c")
            .arg(format!(
                "source '{}'; ensure_project_hook_wiring \"$1\" >/dev/null 2>&1",
                provision_hooks_sh()
            ))
            .arg("hook_wiring_test")
            .arg(dir)
            .output()
            .expect("bash is required for the provision-hooks.sh drift tests");
        assert!(
            out.status.success(),
            "ensure_project_hook_wiring failed (is jq installed?): {}",
            String::from_utf8_lossy(&out.stderr)
        );
        fs::read_to_string(dir.join(".claude/settings.json")).unwrap()
    }

    /// A project settings file as the pre-#15741 writers left it: every table
    /// hook wired as the bare quoted command, one entry per matcher, with a
    /// stale bound on the timeout-bearing hook.
    fn bare_install() -> String {
        let mut hooks = Map::new();
        for (event, matcher, name, timeout) in HOOKS {
            let mut hook = json!({"type": "command", "command": bare_forms(name)[0]});
            if timeout.is_some() {
                hook["timeout"] = json!(3);
            }
            let slots = hooks
                .entry((*event).to_string())
                .or_insert_with(|| json!([]));
            let slots = slots.as_array_mut().unwrap();
            match slots.iter_mut().find(|s| s["matcher"] == *matcher) {
                Some(slot) => slot["hooks"].as_array_mut().unwrap().push(hook),
                None => slots.push(json!({"matcher": matcher, "hooks": [hook]})),
            }
        }
        json!({"permissions": {"allow": ["Bash(gh:*)"]}, "hooks": hooks}).to_string()
    }

    /// `text` wires exactly one Loom entry per table row — the launcher command
    /// for its own event — carrying the row's timeout, and nothing else moved.
    fn assert_one_launcher_per_row(text: &str) {
        let v: Value = serde_json::from_str(text).unwrap();
        assert_eq!(v["permissions"]["allow"][0], "Bash(gh:*)", "unrelated settings survive");
        for (event, _, name, timeout) in HOOKS {
            let command = project_command(event, name);
            let hooks: Vec<&Value> = v["hooks"][event]
                .as_array()
                .unwrap()
                .iter()
                .flat_map(|e| e["hooks"].as_array().unwrap())
                .collect();
            let mine: Vec<&str> = hooks
                .iter()
                .filter_map(|h| h["command"].as_str())
                .filter(|c| c.contains(&format!(".loom/hooks/{name}")))
                .collect();
            assert_eq!(
                mine,
                vec![command.as_str()],
                "exactly one launcher entry for {event}/{name}"
            );
            assert_eq!(
                hooks.iter().find(|h| h["command"] == command).unwrap()["timeout"],
                json!(timeout),
                "timeout of {event}/{name}"
            );
        }
    }

    /// The JSON document a PreToolUse command's inline fallback prints, once
    /// bash has unescaped it; `None` for a command that prints no decision.
    fn deny_document(command: &str) -> Option<Value> {
        let rest = command.split_once("printf %s \"")?.1;
        let (document, _) = rest.split_once("\"; exit 0")?;
        serde_json::from_str(&document.replace("\\\"", "\"")).ok()
    }

    fn all(_: &str) -> bool {
        true
    }

    fn words(block: &str) -> Vec<String> {
        block
            .split_whitespace()
            .map(|w| w.trim_matches('"').to_string())
            .collect()
    }

    /// Text between `NAME=(` and the closing `)`.
    fn array(name: &str) -> String {
        let start = SCRIPT.find(&format!("{name}=(")).expect(name) + name.len() + 2;
        SCRIPT[start..start + SCRIPT[start..].find(')').unwrap()].to_string()
    }

    /// Words of an array body, quote-aware: a mix of bare words (`Bash`) and
    /// quoted words, including quoted EMPTY strings (`""`).
    fn quoted_words(block: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let (mut cur, mut quoted, mut open) = (String::new(), false, false);
        for c in block.chars() {
            match c {
                '"' => {
                    quoted = !quoted;
                    open = true;
                }
                c if c.is_whitespace() && !quoted => {
                    if open {
                        out.push(std::mem::take(&mut cur));
                        open = false;
                    }
                }
                c => {
                    cur.push(c);
                    open = true;
                }
            }
        }
        if open {
            out.push(cur);
        }
        out
    }

    #[test]
    fn table_matches_provision_hooks_sh() {
        let types = words(&array("_PHOOK_TYPES"));
        let names = words(&array("_PHOOK_NAMES"));
        // Matchers and timeouts mix bare and quoted words, and may be empty.
        let matchers = quoted_words(&array("_PHOOK_MATCHERS"));
        let timeouts = quoted_words(&array("_PHOOK_TIMEOUTS"));
        let rust: Vec<_> = HOOKS.iter().collect();
        assert_eq!(types.len(), rust.len());
        assert_eq!(names.len(), rust.len());
        assert_eq!(matchers.len(), rust.len());
        assert_eq!(timeouts.len(), rust.len());
        for (i, (e, m, n, t)) in HOOKS.iter().enumerate() {
            assert_eq!((types[i].as_str(), matchers[i].as_str(), names[i].as_str()), (*e, *m, *n));
            assert_eq!(timeouts[i].parse::<u64>().ok(), *t, "timeout of {} at index {i}", names[i]);
        }
    }

    /// #13959: the SessionStart gh front is wired with the table's `timeout`;
    /// hooks whose table entry has none get no field at all.
    #[test]
    fn sessionstart_entry_carries_timeout_and_others_do_not() {
        let Wired::Changed(out) = wire("{}", all) else {
            panic!("expected change")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let session: Vec<_> = v["hooks"]["SessionStart"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .collect();
        assert_eq!(session.len(), 1);
        assert_eq!(session[0]["command"], project_command("SessionStart", "gh-front-env.sh"));
        assert_eq!(session[0]["timeout"], 10);
        let bash: Vec<_> = v["hooks"]["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|e| e["hooks"].as_array().unwrap())
            .collect();
        assert!(bash.iter().all(|h| h.get("timeout").is_none()));
    }

    /// #13959, #15741: a bare Loom entry converges to the launcher form AND the
    /// table's `timeout` in place (no duplicate), a hand-written entry
    /// referencing the same script is left alone, and an already-converged
    /// file is byte-stable.
    #[test]
    fn wired_entry_converges_to_timeout_without_touching_hand_written_entries() {
        let before = r#"{"hooks":{"SessionStart":[
            {"matcher":"","hooks":[
              {"type":"command","command":"\"${CLAUDE_PROJECT_DIR}/.loom/hooks/gh-front-env.sh\""},
              {"type":"command","command":"bash .loom/hooks/gh-front-env.sh"}]}]}}"#;
        let Wired::Changed(out) = wire(before, |n| n == "gh-front-env.sh") else {
            panic!("expected change")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let list = v["hooks"]["SessionStart"][0]["hooks"].as_array().unwrap();
        assert_eq!(list.len(), 2, "no duplicate appended");
        assert_eq!(
            list[0]["command"],
            project_command("SessionStart", "gh-front-env.sh"),
            "the bare Loom entry was upgraded to the launcher form"
        );
        assert_eq!(list[0]["timeout"], 10, "Loom-authored entry gained the timeout");
        assert_eq!(list[1]["command"], "bash .loom/hooks/gh-front-env.sh");
        assert!(list[1].get("timeout").is_none(), "hand-written entry untouched");
        // A second pass on the converged file changes nothing (byte-stable).
        assert_eq!(wire(&out, |n| n == "gh-front-env.sh"), Wired::Unchanged);
    }

    #[test]
    fn wires_everything_into_repo_skills_only_settings_and_keeps_order() {
        let before = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
            {"type":"command","command":".claude/skills/repo/hooks/guard-destructive.sh"}]}]}}"#;
        let Wired::Changed(out) = wire(before, all) else {
            panic!("expected change")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let bash = &v["hooks"]["PreToolUse"][0]["hooks"];
        assert_eq!(bash[0]["command"], ".claude/skills/repo/hooks/guard-destructive.sh");
        assert_eq!(bash[1]["command"], project_command("PreToolUse", "guard-destructive.sh"));
        let count: usize = v["hooks"]
            .as_object()
            .unwrap()
            .values()
            .flat_map(|e| e.as_array().unwrap())
            .map(|e| e["hooks"].as_array().unwrap().len())
            .sum();
        assert_eq!(count, 1 + HOOKS.len());
    }

    #[test]
    fn second_pass_is_a_no_op() {
        let Wired::Changed(out) = wire("{}", all) else {
            panic!()
        };
        assert_eq!(wire(&out, all), Wired::Unchanged);
    }

    #[test]
    fn absent_scripts_are_not_wired() {
        assert_eq!(wire("{}", |_| false), Wired::Unchanged);
        let Wired::Changed(out) = wire("{}", |n| n == "gh-front-env.sh") else {
            panic!()
        };
        assert!(out.contains("gh-front-env.sh") && !out.contains("skill-router.sh"));
    }

    #[test]
    fn unusable_files_are_left_alone() {
        for text in [
            "not json",
            "[]",
            r#"{"hooks":[]}"#,
            r#"{"hooks":{"Stop":{}}}"#,
            "",
        ] {
            assert!(matches!(wire(text, all), Wired::Unusable(_)), "{text}");
        }
    }

    /// #15741: both writers emit the same command for every table row — the
    /// installer's `_phook_project_cmd` and [`project_command`] — so a resync
    /// and an install converge instead of duplicating.
    #[test]
    fn project_commands_match_provision_hooks_sh() {
        let theirs = shell_commands();
        assert_eq!(theirs.len(), HOOKS.len());
        for ((event, _, name, _), theirs) in HOOKS.iter().zip(theirs) {
            assert_eq!(project_command(event, name), theirs, "command for {event}/{name}");
        }
    }

    /// #15741: every command is a launcher command for its OWN event and hook.
    #[test]
    fn command_is_a_launcher_for_its_own_event_and_hook() {
        for (event, _, name, _) in HOOKS {
            let (event, name) = (*event, *name);
            let command = project_command(event, name);
            let body = command
                .strip_prefix("bash -c '")
                .and_then(|b| b.strip_suffix('\''))
                .unwrap_or_else(|| panic!("not a single-quoted bash -c command: {command}"));
            // What the dedup and the shell both rely on.
            assert!(!body.contains('\''), "a quote inside would end the bash -c string early");
            assert!(
                !command.contains("defaults/hooks/"),
                "the dedup reads that as the machine-level wrapper"
            );
            assert!(
                !command.contains("${CLAUDE_PROJECT_DIR}/"),
                "the project path is never embedded as text (#6544)"
            );
            assert!(body.ends_with("exit 0"), "never exits non-zero");
            // ...and the ladder: root, launcher, inline fallback, gate, report.
            assert!(body.contains(concat!(
                "R=$(git rev-parse --git-common-dir 2>/dev/null); ",
                "R=${R:+$(cd \"$R/..\" 2>/dev/null && pwd)}; ",
                "W=${R:-${CLAUDE_PROJECT_DIR:-$PWD}}; "
            )));
            assert!(body.contains(&format!(
                "L=$W/.loom/hooks/hook-wiring.sh; [ -r \"$L\" ] && \
                 LOOM_PROJECT_ROOT=$W exec bash \"$L\" {event} {name}; "
            )));
            assert!(body.contains(&format!(
                "G=$W/.loom/hooks/{name}; [ -x \"$G\" ] && exec \"$G\"; \
                 [ -r \"$G\" ] && exec bash \"$G\"; "
            )));
            assert!(body.contains(
                "[ -d \"$W/.loom/hooks\" ] || exit 0; \
                 echo \"[loom] BROKEN GUARD INSTALL: "
            ));
            // Every PreToolUse command carries the deny document (the fail-open
            // hook's too — its exported escape hatch exits before the printf);
            // no other event has a deny channel and so carries none.
            let fail_open = FAIL_OPEN.contains(&name);
            assert_eq!(
                body.starts_with("export LOOM_GUARD_WIRING_FAILOPEN=1; "),
                event == "PreToolUse" && fail_open,
                "fail-open prefix of {event}/{name}"
            );
            if event == "PreToolUse" {
                let doc = deny_document(&command)
                    .unwrap_or_else(|| panic!("no parsable deny document in {command}"));
                assert_eq!(doc["hookSpecificOutput"]["hookEventName"], event);
                assert_eq!(doc["hookSpecificOutput"]["permissionDecision"], "deny");
                assert!(doc["hookSpecificOutput"]["permissionDecisionReason"]
                    .as_str()
                    .unwrap()
                    .contains(name));
            } else {
                assert!(deny_document(&command).is_none(), "{event}/{name} must carry no decision");
            }
        }
    }

    /// #15741: both bare forms an earlier Loom writer emitted are upgraded in
    /// place, each to its own event's launcher command, with nothing duplicated
    /// and nothing reordered; a second pass changes nothing.
    #[test]
    fn bare_loom_commands_upgrade_in_place() {
        let before = r#"{"hooks":{
            "PreToolUse":[{"matcher":"Bash","hooks":[
              {"type":"command","command":"\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\""},
              {"type":"command","command":"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-loom-workflow.sh"}]}],
            "Stop":[{"matcher":"","hooks":[
              {"type":"command","command":"\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-uncommitted-work.sh\""}]}]}}"#;
        let present = |n: &str| {
            matches!(
                n,
                "guard-destructive.sh" | "guard-loom-workflow.sh" | "guard-uncommitted-work.sh"
            )
        };
        let Wired::Changed(out) = wire(before, present) else {
            panic!("expected change")
        };
        let v: Value = serde_json::from_str(&out).unwrap();
        let bash = v["hooks"]["PreToolUse"][0]["hooks"].as_array().unwrap();
        assert_eq!(bash.len(), 2, "no duplicate appended");
        assert_eq!(
            bash[0]["command"],
            project_command("PreToolUse", "guard-destructive.sh"),
            "the quoted bare form is upgraded"
        );
        assert_eq!(
            bash[1]["command"],
            project_command("PreToolUse", "guard-loom-workflow.sh"),
            "the unquoted bare form is upgraded"
        );
        let stop = v["hooks"]["Stop"][0]["hooks"].as_array().unwrap();
        assert_eq!(stop.len(), 1, "upgraded in place, not duplicated");
        assert_eq!(
            stop[0]["command"],
            project_command("Stop", "guard-uncommitted-work.sh"),
            "the upgrade carries the entry's own event"
        );
        assert_eq!(
            v["hooks"].as_object().unwrap().keys().collect::<Vec<_>>(),
            ["PreToolUse", "Stop", "SubagentStop"]
        );
        assert_eq!(wire(&out, present), Wired::Unchanged, "a second pass is a no-op");
    }

    /// #15741: anything that is not one of Loom's own commands is the
    /// operator's. It still counts as wiring the hook (nothing is appended),
    /// but it is never rewritten.
    #[test]
    fn hand_written_variants_are_neither_upgraded_nor_duplicated() {
        let mut edited_launcher = project_command("PreToolUse", "guard-destructive.sh");
        edited_launcher.insert(8, ' ');
        for theirs in [
            "bash \"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\"",
            "${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh --verbose",
            "\"${CLAUDE_PROJECT_DIR}/.loom/hooks/guard-destructive.sh\" --verbose",
            ".loom/hooks/guard-destructive.sh",
            edited_launcher.as_str(),
        ] {
            let before = json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[
                {"type":"command","command":theirs,"timeout":7}]}]}});
            let before = serde_json::to_string(&before).unwrap();
            assert_eq!(
                wire(&before, |n| n == "guard-destructive.sh"),
                Wired::Unchanged,
                "{theirs}"
            );
        }
    }

    /// #13959: a launcher-form entry of the timeout-bearing hook that lost (or
    /// never had) its bound gets it back, in place.
    #[test]
    fn launcher_entry_converges_to_the_table_timeout() {
        let command = project_command("SessionStart", "gh-front-env.sh");
        for stale in [None, Some(3)] {
            let mut hook = json!({"type": "command", "command": command, "status": "mine"});
            if let Some(t) = stale {
                hook["timeout"] = json!(t);
            }
            let before = json!({"hooks":{"SessionStart":[{"matcher":"","hooks":[hook]}]}});
            let before = serde_json::to_string(&before).unwrap();
            let Wired::Changed(out) = wire(&before, |n| n == "gh-front-env.sh") else {
                panic!("expected change for {before}")
            };
            let v: Value = serde_json::from_str(&out).unwrap();
            let hook = &v["hooks"]["SessionStart"][0]["hooks"][0];
            assert_eq!(hook["command"], command, "command untouched");
            assert_eq!(hook["timeout"], 10);
            assert_eq!(hook["status"], "mine", "the operator's other fields survive");
        }
    }

    /// #15741: the two writers are interchangeable. Over a bare-form install,
    /// whichever goes first, the other's pass on the result changes nothing —
    /// and either order leaves one launcher entry per (event, hook), with the
    /// table's timeout.
    #[test]
    fn the_two_writers_converge_on_the_same_entries() {
        let before = bare_install();
        let rust_then_shell = TempDir::new().unwrap();
        let shell_then_rust = TempDir::new().unwrap();
        for dir in [rust_then_shell.path(), shell_then_rust.path()] {
            let hooks = dir.join(".loom/hooks");
            fs::create_dir_all(&hooks).unwrap();
            for (_, _, name, _) in HOOKS {
                fs::write(hooks.join(name), "#!/bin/sh\n").unwrap();
            }
            fs::create_dir_all(dir.join(".claude")).unwrap();
            fs::write(dir.join(".claude/settings.json"), &before).unwrap();
        }

        // The daemon resync, then the installer: the installer's pass must not
        // add or change anything.
        let Wired::Changed(after_rust) = wire(&before, all) else {
            panic!("expected change")
        };
        fs::write(rust_then_shell.path().join(".claude/settings.json"), &after_rust).unwrap();
        let after_shell = shell_wire(rust_then_shell.path());
        assert_eq!(
            serde_json::from_str::<Value>(&after_shell).unwrap(),
            serde_json::from_str::<Value>(&after_rust).unwrap(),
            "the installer's pass over the daemon's output changed it"
        );
        assert_one_launcher_per_row(&after_rust);

        // The installer, then the daemon resync: the resync's pass is a no-op.
        let after_shell = shell_wire(shell_then_rust.path());
        assert_eq!(
            wire(&after_shell, all),
            Wired::Unchanged,
            "the daemon's pass over the installer's output changed it"
        );
        assert_one_launcher_per_row(&after_shell);
    }
}
