//! `loom-daemon merge-pr required-checks` (#10688): the synchronous merge
//! path's required-context conclusion gate. Logic and contract:
//! [`crate::merge_pr::checks_failure::required`].
//!
//! | outcome | stdout | exit |
//! |---|---|---|
//! | every required context green | `LOOM-REQUIRED-CHECKS-CLEAN` | 0 |
//! | red only on `--allow-red-required` contexts | `LOOM-REQUIRED-CHECKS-BYPASSED …` (audit comment posted) | 0 |
//! | an unallowed required context concluded red | the refusal naming it | 1 |
//! | the forge could not be read | `LOOM-REQUIRED-CHECKS-UNREADABLE …` | 2 |
//! | a required context missing or pending | `LOOM-REQUIRED-CHECKS-HOLD …` | 3 (caller holds, exit 5) |
//!
//! `--dry-run` and `--from-stdin` never post the audit comment. `--from-stdin`
//! reads `{"required": [...], "check_runs": [{name, status, conclusion,
//! started_at, completed_at}], "statuses": <commits/{sha}/status payload>}`
//! (`completed_at` optional; consulted via `check_runs_rollup::is_settled`
//! when `status` is not `"completed"`, #15371) and assesses offline (the
//! fixture seam); a missing `statuses` key reads as none. Status-disagreement
//! diagnostics are written to stderr so stdout stays machine-readable.

use crate::merge_pr::checks_failure::required::{
    assess, bypass_comment, hold_reason, list, live_inputs, parse_check_runs, parse_statuses,
    refusal, status_disagreements, Inputs, Status, Verdict, BYPASSED, CLEAN, HOLD, UNREADABLE,
};
use crate::merge_pr::stale_checks::CheckRun;
use anyhow::Result;
use serde_json::Value;
use std::io::{Read, Write};

#[derive(clap::Args)]
pub(crate) struct RequiredChecksArgs {
    /// The PR number.
    #[arg(long, value_name = "N")]
    pr: String,

    /// The repository as owner/repo.
    #[arg(long, value_name = "OWNER/REPO", default_value = "")]
    repo: String,

    /// The PR head SHA being merged.
    #[arg(long, value_name = "SHA")]
    head_sha: String,

    /// The branch the merge lands on (whose required contexts apply).
    #[arg(long, value_name = "REF", default_value = "main")]
    base_ref: String,

    /// Override a red conclusion of this required context (repeatable).
    #[arg(long, value_name = "CONTEXT")]
    allow_red_required: Vec<String>,

    /// Report only; post no audit comment.
    #[arg(long)]
    dry_run: bool,

    /// Assess a JSON document from stdin instead of the forge.
    #[arg(long)]
    from_stdin: bool,
}

fn unreadable(why: &str) -> ! {
    println!("{UNREADABLE} {why}");
    std::process::exit(2);
}

fn parse_stdin_doc(doc: &Value) -> Result<Inputs, String> {
    let required: Vec<String> = doc
        .get("required")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let runs = parse_check_runs(doc.get("check_runs").unwrap_or(&Value::Array(vec![])))?;
    let statuses = match doc.get("statuses") {
        Some(s) => parse_statuses(s)?,
        None => Vec::new(),
    };
    Ok((required, runs, statuses))
}

impl RequiredChecksArgs {
    pub(crate) fn run(self) -> Result<()> {
        let (required, runs, statuses) = if self.from_stdin {
            let mut raw = String::new();
            std::io::stdin().read_to_string(&mut raw)?;
            let doc: Value = serde_json::from_str(&raw)?;
            parse_stdin_doc(&doc).unwrap_or_else(|e| unreadable(&e))
        } else {
            live_inputs(&self.repo, &self.base_ref, &self.head_sha)
                .unwrap_or_else(|e| unreadable(&e))
        };
        let post = !self.dry_run && !self.from_stdin;
        let code = self.finish(
            &required,
            &runs,
            &statuses,
            post,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        );
        if code != 0 {
            let _ = std::io::stdout().flush();
            let _ = std::io::stderr().flush();
            std::process::exit(code);
        }
        Ok(())
    }

    fn finish(
        &self,
        required: &[String],
        runs: &[CheckRun],
        statuses: &[Status],
        post: bool,
        stdout: &mut impl Write,
        stderr: &mut impl Write,
    ) -> i32 {
        for note in status_disagreements(&self.pr, &self.head_sha, required, runs) {
            let _ = writeln!(stderr, "{note}");
        }
        match assess(required, runs, statuses, &self.allow_red_required) {
            Verdict::Clean => {
                let _ = writeln!(stdout, "{CLEAN}");
                0
            }
            Verdict::Bypassed(bypassed) => {
                let _ = writeln!(
                    stdout,
                    "{BYPASSED} PR #{}: red required context(s) {} overridden by --allow-red-required (#10688).",
                    self.pr,
                    list(&bypassed)
                );
                if post {
                    let body = bypass_comment(&self.pr, &self.head_sha, &bypassed);
                    if let Err(e) = crate::forge_comment::post_comment(
                        "gh", None, &self.repo, &self.pr, true, &body,
                    ) {
                        let _ = writeln!(
                            stderr,
                            "Warning: could not post the --allow-red-required audit comment on PR #{}: {e}",
                            self.pr
                        );
                    }
                }
                0
            }
            Verdict::Refuse { red, .. } => {
                let _ = writeln!(stdout, "{}", refusal(&self.pr, &self.head_sha, &red));
                1
            }
            Verdict::Hold { waiting, .. } => {
                let _ = writeln!(stdout, "{HOLD} {}", hold_reason(&self.head_sha, &waiting));
                3
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(pr: &str, sha: &str, allow: &[&str], json_raw: &str) -> (i32, String, String) {
        let doc: Value = serde_json::from_str(json_raw).unwrap();
        let (required, runs, statuses) = parse_stdin_doc(&doc).unwrap();
        let args = RequiredChecksArgs {
            pr: pr.into(),
            repo: String::new(),
            head_sha: sha.into(),
            base_ref: "main".into(),
            allow_red_required: allow.iter().map(|s| (*s).into()).collect(),
            dry_run: false,
            from_stdin: true,
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = args.finish(&required, &runs, &statuses, false, &mut out, &mut err);
        (code, String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    #[test]
    fn incident_repro_emits_clean_on_stdout_and_disagreement_on_stderr() {
        let raw = r#"{"required":["lint"],"check_runs":[{"name":"lint","status":"in_progress","conclusion":"success","started_at":"2026-11-24T17:28:46Z","completed_at":"2026-11-24T17:28:53Z"}]}"#;
        let (code, stdout, stderr) = eval("430", "1e847cee", &[], raw);
        assert_eq!(code, 0);
        assert_eq!(stdout, "LOOM-REQUIRED-CHECKS-CLEAN\n");
        assert!(stderr.contains("PR #430"), "{stderr}");
        assert!(stderr.contains("1e847cee"), "{stderr}");
        assert!(stderr.contains("`lint`"), "{stderr}");
        assert!(stderr.contains("in_progress"), "{stderr}");
        assert!(stderr.contains("success"), "{stderr}");
    }

    #[test]
    fn settled_in_progress_failure_refuses_exit_1_with_stderr_diagnostic() {
        let raw = r#"{"required":["lint"],"check_runs":[{"name":"lint","status":"in_progress","conclusion":"failure","started_at":"2026-11-24T17:28:46Z","completed_at":"2026-11-24T17:28:53Z"}]}"#;
        let (code, stdout, stderr) = eval("430", "1e847cee", &[], raw);
        assert_eq!(code, 1);
        assert!(stdout.starts_with("Merge blocked: PR #430's head 1e847cee"), "{stdout}");
        assert!(stdout.contains("`lint` (failure)"), "{stdout}");
        assert!(stderr.contains("in_progress"), "{stderr}");
        assert!(stderr.contains("failure"), "{stderr}");
    }

    #[test]
    fn partial_and_rerunning_in_progress_rows_hold_exit_3() {
        for raw in [
            r#"{"required":["lint"],"check_runs":[{"name":"lint","status":"in_progress","conclusion":null,"started_at":"2026-11-24T17:28:46Z","completed_at":"2026-11-24T17:28:53Z"}]}"#,
            r#"{"required":["lint"],"check_runs":[{"name":"lint","status":"in_progress","conclusion":"success","started_at":"2026-11-24T17:28:46Z","completed_at":null}]}"#,
            r#"{"required":["lint"],"check_runs":[{"name":"lint","status":"in_progress","conclusion":"success","started_at":"2026-11-24T17:28:46Z"}]}"#,
            r#"{"required":["lint"],"check_runs":[{"name":"lint","status":"in_progress","conclusion":"success","started_at":"2026-11-24T17:28:46Z","completed_at":"2026-11-24T17:28:53Z"},{"name":"lint","status":"in_progress","conclusion":null,"started_at":"2026-11-24T17:35:00Z","completed_at":null}]}"#,
        ] {
            let (code, stdout, _stderr) = eval("430", "1e847cee", &[], raw);
            assert_eq!(code, 3, "must hold for {raw}: stdout={stdout}");
            assert!(stdout.starts_with("LOOM-REQUIRED-CHECKS-HOLD "), "{stdout}");
            assert!(stdout.contains("`lint` (in_progress)"), "{stdout}");
        }
    }
}
