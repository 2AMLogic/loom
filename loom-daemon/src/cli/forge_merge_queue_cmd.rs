//! `loom-daemon forge merge-queue …` (#10255) — clap surface only. All
//! decisions live in [`crate::forge_merge_queue`], whose `MergeQueueCmd` is
//! itself the clap subcommand enum (#15626).

use crate::forge_merge_queue::MergeQueueCmd;
use clap::Subcommand;

/// `forge merge-queue`'s subcommand slot (#10256), built lazily.
///
/// A debug build gives each clap-derived `augment_subcommands` one stack frame
/// sized by every arg it declares, and the derive nests them: `Commands`
/// (~1.2 MB) -> `ForgeAction` (~0.7 MB) -> `MergeQueueCmd` (~0.2 MB). Adding
/// `Step` pushed that chain past the 2 MB test-thread stack (the
/// `cli::sweep_outcomes_cli` parse tests aborted with a stack overflow). Boxing
/// the field does not help: `Box<T>`'s `augment_subcommands` is `T`'s. Here
/// [`clap::Command::defer`] adds the merge-queue verbs only when clap builds the
/// `merge-queue` command itself (it is selected, or `--help` / `build()` walks
/// it), in a frame of its own after `Commands`' has returned.
pub(crate) struct DeferredMergeQueue(pub(crate) MergeQueueCmd);

impl clap::FromArgMatches for DeferredMergeQueue {
    fn from_arg_matches(m: &clap::ArgMatches) -> Result<Self, clap::Error> {
        MergeQueueCmd::from_arg_matches(m).map(Self)
    }

    fn from_arg_matches_mut(m: &mut clap::ArgMatches) -> Result<Self, clap::Error> {
        MergeQueueCmd::from_arg_matches_mut(m).map(Self)
    }

    fn update_from_arg_matches(&mut self, m: &clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches(m)
    }

    fn update_from_arg_matches_mut(&mut self, m: &mut clap::ArgMatches) -> Result<(), clap::Error> {
        self.0.update_from_arg_matches_mut(m)
    }
}

impl Subcommand for DeferredMergeQueue {
    fn augment_subcommands(cmd: clap::Command) -> clap::Command {
        cmd.defer(MergeQueueCmd::augment_subcommands)
    }

    fn augment_subcommands_for_update(cmd: clap::Command) -> clap::Command {
        cmd.defer(MergeQueueCmd::augment_subcommands_for_update)
    }

    fn has_subcommand(name: &str) -> bool {
        MergeQueueCmd::has_subcommand(name)
    }
}
