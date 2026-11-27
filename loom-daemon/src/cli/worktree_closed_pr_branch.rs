//! `loom-daemon worktree-closed-pr-branch` — the closed-unmerged arm of
//! `worktree.sh`'s branch-resolution contract (#9083).
//!
//! # Exit-code contract
//!
//! 0 = proceed (reuse `origin/<branch>`), 1 = refuse (a message was printed),
//! 2 = could not run (a message was printed to stderr). Nothing else, and
//! every inability to *decide* is 0 — see [`worktree_cli::closed_pr_branch`]'s
//! module docs for why that direction is fixed rather than chosen.
//!
//! 2 is the one failure that is not an answer about the branch: the tools arm
//! cannot resolve its mode or fleet route
//! ([`crate::worktree_cli::pr_list::PrList::Tools`] →
//! `tools_mode::configure_forge_reads`), a misconfiguration of this host
//! (#15650). As an `anyhow` error it left `main` as exit 1 — the code
//! `lib/worktree-forge-pr-check.sh` refuses on — so an unset `LOOM_MODE`
//! refused worktree creation with no refusal document on fd 3. The daemon arm
//! (`PrList::Legacy`) never resolves mode and never exits 2 this way.
//!
//! A daemon predating this subcommand answers clap's exit 2, a usage error
//! and so another shape of "could not run". `worktree.sh`'s dispatch
//! nevertheless probes `--help` first and skips the guard entirely when it is
//! absent — degrading to the pre-#9083 behaviour (reuse) with no stray clap
//! usage text, the same shape `worktree-branch-conflict` uses.
//!
//! # Why clap here
//!
//! One caller, a generated command line inside `worktree.sh`, no human types
//! it — the same argument as `worktree-stale-ref` / `worktree-upstream`.

use std::path::PathBuf;

use anyhow::Result;

use crate::worktree_cli::{closed_pr_branch, pr_list::PrList};

#[derive(clap::Args)]
pub struct WorktreeClosedPrBranchArgs {
    /// `$BRANCH_NAME` — the branch whose `origin/` tip is under question.
    #[arg(long)]
    branch: String,

    /// `$ISSUE_NUMBER`, quoted into the message and the JSON document.
    #[arg(long)]
    issue: String,

    /// `$BASE_DISPLAY` — how the base ref is spelled to a human (`main`).
    #[arg(long)]
    base_display: String,

    /// The main workspace root every `git` and forge call runs in.
    #[arg(long)]
    repo_root: PathBuf,

    /// `$JSON_OUTPUT`, verbatim. A value rather than a flag so the shell
    /// dispatch stays a single line — see [`closed_pr_branch::Options`].
    #[arg(long, default_value = "false")]
    json_output: String,
}

impl WorktreeClosedPrBranchArgs {
    /// Never returns.
    pub(crate) fn run(self, pr_list: PrList) -> Result<()> {
        let opts = closed_pr_branch::Options {
            branch: self.branch,
            issue: self.issue,
            base_display: self.base_display,
            repo: self.repo_root,
            json_output: self.json_output,
        };
        let code = match closed_pr_branch::try_run_with(&opts, &|branch| {
            closed_pr_branch::forge_probe_with(&opts.repo, branch, pr_list)
        }) {
            Ok(code) => code,
            Err(complaint) => {
                eprintln!("{complaint}");
                eprintln!(
                    "worktree-closed-pr-branch: the closed-PR check for origin/{} did not run (exit 2); worktree.sh treats that as \"could not check\" and proceeds, as it does on a forge outage.",
                    opts.branch
                );
                2
            }
        };
        std::process::exit(code);
    }
}
