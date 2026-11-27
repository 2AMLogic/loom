//! Replace an idle seat under its account lease. The stopped original keeps
//! its filesystem and immutable ID until the new seat passes every check.
use super::super::session_lifecycle::{image_reference, mark_session_managed};
use super::{docker, lifecycle::reports_running, Config};
use anyhow::{ensure, Context, Result};
use serde_json::Value;

/// The roll reached the stop boundary. A failure here consumes the image pair.
#[derive(Debug)]
pub(crate) struct RollAttempted;

impl std::fmt::Display for RollAttempted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("private session replacement failed")
    }
}

pub(super) fn replace(config: &Config, original: &Value, image: &str) -> Result<()> {
    let id = original["Id"]
        .as_str()
        .context("original container ID unavailable")?;
    let image_id = original["Image"]
        .as_str()
        .context("original image ID unavailable")?;
    let backup = format!("{}-previous-{id}", config.container);

    let result = (|| {
        docker::command(&["stop", "--time", "15", id])?;
        let stopped = docker::inspect_id(id)?.context("original container disappeared")?;
        ensure!(stopped["State"]["Running"] == false, "container stop not confirmed");
        docker::command(&["rename", id, &backup])?;
        docker::provision_controls(config, image)?;
        docker::create_replacing(config, image, Some(id))?;
        let candidate = docker::inspect(&config.container)?.context("replacement disappeared")?;
        ensure!(
            candidate["Config"]["Labels"][docker::REPLACES_LABEL] == id
                && candidate["Config"]["Image"].as_str().map(image_reference) == Some(image),
            "unexpected replacement identity or image"
        );
        verify_running(config, &candidate, Some(id))?;
        mark_session_managed(&config.profile, &config.container)?;
        // Commit point. An ambiguous rm result can still mean it committed.
        if let Err(error) = docker::command(&["rm", id]) {
            if docker::inspect_id(id)?.is_some() {
                return Err(error);
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback) = restore(config, id, image_id) {
            return Err(error
                .context(format!("rollback failed for original container {id}: {rollback:#}"))
                .context(RollAttempted));
        }
        return Err(error.context(RollAttempted));
    }
    Ok(())
}

/// Finish or undo a roll whose process died (#14112). Call under the account
/// lease, before any validation of the seat: an interrupted roll leaves the
/// stopped original under `<name>-previous-<id>`, which the ownership check
/// rightly refuses as a peer. Identified by the immutable 64-hex ID in the
/// backup name; the original must still be this account's stopped seat. Then:
/// - the session name is free, or holds a replacement that does not verify:
///   restore the original (the same rollback a live roll runs);
/// - the name holds a running replacement labelled as replacing exactly this
///   original and it verifies: commit it by removing the original;
/// - the name belongs to anything else: change nothing and refuse.
pub(super) fn recover_interrupted(config: &Config, held: &dyn Fn() -> Result<()>) -> Result<()> {
    let prefix = format!("{}-previous-", config.container);
    let names = docker::command(&["container", "ls", "-a", "--format", "{{.Names}}"])?;
    let mut backups = names
        .lines()
        .filter_map(|name| name.strip_prefix(&prefix))
        .filter(|id| id.len() == 64 && id.chars().all(|c| c.is_ascii_hexdigit()));
    let Some(id) = backups.next() else {
        return Ok(());
    };
    ensure!(
        backups.next().is_none(),
        "more than one interrupted-roll backup exists; preserve them for operator recovery"
    );
    let original = docker::inspect_id(id)?.context("interrupted roll's original disappeared")?;
    ensure!(
        original["Name"] == format!("/{prefix}{id}"),
        "interrupted roll backup identity changed"
    );
    ensure!(
        original["State"]["Running"] == false,
        "interrupted roll's original is running under its backup name"
    );
    docker::validate_settings(config, &original)?;
    let image_id = original["Image"]
        .as_str()
        .context("original image ID unavailable")?;
    held()?;
    match docker::inspect(&config.container)? {
        Some(named) if named["Config"]["Labels"][docker::REPLACES_LABEL] == id => {
            let verified = reports_running(Some(&named))
                && verify_running(config, &named, Some(id)).is_ok()
                && mark_session_managed(&config.profile, &config.container).is_ok();
            if verified {
                // Commit point, as in `replace`: an ambiguous rm can still commit.
                if let Err(error) = docker::command(&["rm", id]) {
                    if docker::inspect_id(id)?.is_some() {
                        return Err(error);
                    }
                }
                log::warn!(
                    "private session {}: finished an interrupted image roll; removed original {id}",
                    config.account
                );
                return Ok(());
            }
        }
        Some(_) => anyhow::bail!(
            "session name belongs to a container that is not this roll's replacement; \
             leaving the interrupted roll's original {id} untouched"
        ),
        None => {}
    }
    restore(config, id, image_id)?;
    log::warn!(
        "private session {}: restored original {id} after an interrupted image roll",
        config.account
    );
    Ok(())
}

fn restore(config: &Config, id: &str, image_id: &str) -> Result<()> {
    // Never touch a name's new occupant unless it was created to replace this
    // exact original. A failed docker run can leave a named, stopped container.
    let original = docker::inspect_id(id)?.context("original container unavailable")?;
    if let Some(named) = docker::inspect(&config.container)? {
        if named["Id"] != id {
            ensure!(
                named["Config"]["Labels"][docker::REPLACES_LABEL] == id,
                "session name now belongs to another container"
            );
            let replacement = named["Id"].as_str().context("replacement ID unavailable")?;
            docker::command(&["rm", "-f", replacement])?;
        }
    }
    if original["Name"] != format!("/{}", config.container) {
        docker::command(&["rename", id, &config.container])?;
    }
    docker::validate(config, &original)?;
    if original["State"]["Running"] == false {
        // A stock tag (especially :latest) may no longer name the original
        // image. Restore its profile controls using the immutable image ID.
        docker::provision_controls(config, image_id)?;
        docker::command(&["start", id])?;
    }
    let restored = docker::inspect(&config.container)?.context("restored container disappeared")?;
    ensure!(restored["Id"] == id, "restored container identity changed");
    verify_running(config, &restored, None)
}

fn verify_running(config: &Config, state: &Value, retained: Option<&str>) -> Result<()> {
    docker::validate_replacing(config, state, retained)?;
    ensure!(reports_running(Some(state)), "private session is not running");
    let id = state["Id"].as_str().context("container ID unavailable")?;
    docker::setup(config)?;
    docker::control(config, id)?;
    let current = docker::inspect(&config.container)?.context("verified session disappeared")?;
    ensure!(
        current["Id"] == id && reports_running(Some(&current)),
        "session changed during verification"
    );
    Ok(())
}
