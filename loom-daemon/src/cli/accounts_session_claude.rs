//! `accounts session start|stop|status --provider claude` (issue #14371): the
//! per-seat Claude session container surface. Kept out of
//! `accounts_session.rs`, whose Codex lifecycle (profile directories, hooks,
//! private clones) a Claude account shares none of.
use std::path::PathBuf;

use anyhow::{bail, Result};

use crate::tokens_pool::account_registry::AccountProvider;
use crate::tokens_pool::claude_seat::{seat_root, ClaudeSeatLifecycle, SeatStatus};
use crate::tokens_pool::session_lifecycle::{ProcessContainerRunner, DEFAULT_SESSION_IMAGE};

/// `Ok(true)` when `--provider` names Claude, `Ok(false)` for Codex (the
/// default), and an error listing the vocabulary for anything else.
pub(crate) fn is_claude(provider: &str) -> Result<bool> {
    let provider: AccountProvider = provider.parse()?;
    Ok(provider == AccountProvider::Claude)
}

/// The operator-facing actions a Claude seat supports.
pub(crate) enum Op {
    Start {
        name: String,
        image: Option<String>,
        mount_workspace: Option<PathBuf>,
    },
    Stop {
        name: String,
        force: bool,
    },
    Status {
        name: String,
    },
}

pub(crate) fn run(op: Op, workspace: PathBuf, json: bool) -> Result<()> {
    let Some(root) = seat_root() else {
        bail!(
            "Claude seats are disabled: {} is set but empty",
            crate::tokens_pool::claude_seat::SEAT_ROOT_ENV
        );
    };
    // Invalid `accounts.session` config refuses outright: a seat created from
    // the stock image because its override failed to parse would be a
    // fail-open (#14019). The operator's explicit `--image` beats the
    // configured override, which beats the stock image.
    let session_config = crate::tokens_pool::session_image::read_session_image_config(&workspace)?;
    let (explicit_image, mount_workspace) = match &op {
        Op::Start {
            image,
            mount_workspace,
            ..
        } => (image.clone(), mount_workspace.clone()),
        _ => (None, None),
    };
    let image = explicit_image.or_else(|| session_config.image.clone());
    let lifecycle =
        ClaudeSeatLifecycle::new(workspace, root, ProcessContainerRunner, image.clone())
            .with_session_config(Some(session_config));
    let status = match op {
        Op::Start { name, .. } => {
            if let Err(error) = crate::tokens_pool::operator_interrupt::install() {
                eprintln!("note: Ctrl-C will not reach the `docker` command ({error:#})");
            }
            if lifecycle.status(&name)?.container_id.is_none() {
                crate::tokens_pool::docker_cli::ensure_image_for_operator(
                    image.as_deref().unwrap_or(DEFAULT_SESSION_IMAGE),
                )?;
            }
            lifecycle.start(&name, mount_workspace.as_deref())?
        }
        Op::Stop { name, force } => lifecycle.stop(&name, force)?,
        Op::Status { name } => lifecycle.status(&name)?,
    };
    print_status(&status, json)
}

/// `accounts list --provider claude`: one line per pool account, showing the
/// seat's runtime and image (#14371 AC8). Read-only; starts nothing.
pub(crate) fn list(workspace: PathBuf, json: bool) -> Result<()> {
    let Some(root) = seat_root() else {
        bail!(
            "Claude seats are disabled: {} is set but empty",
            crate::tokens_pool::claude_seat::SEAT_ROOT_ENV
        );
    };
    let session_config = crate::tokens_pool::session_image::read_session_image_config(&workspace)?;
    let lifecycle = ClaudeSeatLifecycle::new(workspace, root, ProcessContainerRunner, None)
        .with_session_config(Some(session_config));
    let seats = lifecycle.list()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&seats)?);
    } else if seats.is_empty() {
        println!("No Claude accounts in the token pool.");
    } else {
        for seat in &seats {
            println!("{}", list_line(seat));
        }
    }
    Ok(())
}

/// The text form of one `accounts list` row. A registered seat names its
/// container's runtime, containment shape and image; an unregistered account
/// says it has no seat, since it dispatches on the host or per-sweep.
fn list_line(seat: &SeatStatus) -> String {
    if !seat.seat_registered {
        return format!(
            "{}: no seat (runtime={}, host or per-sweep dispatch)",
            seat.name, seat.runtime
        );
    }
    format!(
        "{}: {} (runtime={}, containment={}, container={}, image={})",
        seat.name,
        if seat.running {
            "running"
        } else if seat.restarting {
            "restarting"
        } else {
            "stopped"
        },
        seat.runtime,
        seat.containment,
        seat.container_name,
        seat.image.as_deref().unwrap_or("-"),
    )
}

fn print_status(status: &SeatStatus, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(status)?);
        return Ok(());
    }
    println!(
        "{}: {} (runtime={}, containment={}, container={}, id={}, image={}, started_at={}, \
         claude_home={}, mount={}, seat_registered={}, workspace={})",
        status.name,
        if status.running {
            "running"
        } else if status.restarting {
            "restarting"
        } else {
            "stopped"
        },
        status.runtime,
        status.containment,
        status.container_name,
        status.container_id.as_deref().unwrap_or("-"),
        status.image.as_deref().unwrap_or("-"),
        status.started_at.as_deref().unwrap_or("-"),
        status.claude_home.display(),
        status.mount_path,
        status.seat_registered,
        status
            .workspace
            .as_ref()
            .map_or_else(|| "-".to_string(), |w| w.display().to_string()),
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seat(registered: bool, running: bool, image: Option<&str>) -> SeatStatus {
        SeatStatus {
            schema_version: 1,
            runtime: "claude",
            containment: "claude-session",
            name: "alice".into(),
            container_name: "loom-claude-session-alice".into(),
            running,
            restarting: false,
            container_id: running.then(|| "abc".to_string()),
            started_at: None,
            image: image.map(str::to_string),
            claude_home: PathBuf::from("/seats/alice/home"),
            mount_path: "/home/loom/.claude",
            seat_registered: registered,
            workspace: None,
        }
    }

    #[test]
    fn a_registered_seat_row_names_runtime_containment_and_image() {
        let line = list_line(&seat(true, true, Some("ghcr.io/x/loom-worker-session@sha256:1")));
        assert_eq!(
            line,
            "alice: running (runtime=claude, containment=claude-session, \
             container=loom-claude-session-alice, image=ghcr.io/x/loom-worker-session@sha256:1)"
        );
    }

    #[test]
    fn a_stopped_seat_shows_no_image_rather_than_a_stale_one() {
        assert!(list_line(&seat(true, false, None)).contains("stopped (runtime=claude"));
        assert!(list_line(&seat(true, false, None)).ends_with("image=-)"));
    }

    #[test]
    fn an_unregistered_account_is_listed_as_having_no_seat() {
        let line = list_line(&seat(false, false, None));
        assert!(line.starts_with("alice: no seat"), "{line}");
        assert!(!line.contains("containment="), "{line}");
    }
}
