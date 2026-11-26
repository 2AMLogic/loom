//! A Codex sandbox refusal holds the selected profile across every workspace
//! that uses it (#14455). Each profile carries its own deadline. Credential
//! health, selection cursors and model cooldowns stay in their workspace.

use std::collections::HashMap;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::account_registry::{
    account_inventory_quiet, AccountDescriptor, AccountId, AccountProvider,
};
use super::health::{AccountHealth, HealthReason};
use super::locking::MkdirLock;

pub const PROVENANCE: &str = "sandbox-unavailable:v1";
const FILE: &str = ".sandbox-seat-hold.json";
const LOCK: &str = ".sandbox-seat-hold.lock";

#[derive(Serialize, Deserialize)]
struct Hold {
    held_at: u64,
    until: u64,
}

fn read(profile: &Path) -> Result<Option<Hold>> {
    let path = profile.join(FILE);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .with_context(|| format!("malformed sandbox hold {}", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
    }
}

fn profile_for(workspace: &Path, id: &AccountId) -> Result<Option<PathBuf>> {
    if super::paths::codex_profile_root().is_none() {
        return Ok(None);
    }
    Ok(account_inventory_quiet(workspace, AccountProvider::Codex)?
        .into_iter()
        .find(|account| account.id == *id)
        .map(|account| account.credential_reference))
}

/// Hold the resolved seat even if this workspace has a longer credential
/// cooldown: another workspace sharing that profile may have none.
pub fn hold_seat(workspace: &Path, id: &AccountId, secs: u64, now: u64) -> Result<bool> {
    if secs == 0 {
        return Ok(false);
    }
    let profile =
        profile_for(workspace, id)?.with_context(|| format!("unknown Codex seat {}", id.name))?;
    let _lock = MkdirLock::acquire(&profile.join(LOCK)).map_err(anyhow::Error::msg)?;
    let until = now.saturating_add(secs);
    if read(&profile)?.is_some_and(|hold| hold.until >= until) {
        return Ok(false);
    }
    super::session_hold::write_json_atomic(
        &profile,
        FILE,
        &Hold {
            held_at: now,
            until,
        },
    )?;
    Ok(true)
}

/// A successful launch on this profile releases the hold for every workspace.
pub(super) fn clear_on_success(workspace: &Path, id: &AccountId) -> Result<()> {
    let Some(profile) = profile_for(workspace, id)? else {
        return Ok(());
    };
    let path = profile.join(FILE);
    if !path.try_exists()? {
        return Ok(());
    }
    let _lock = MkdirLock::acquire(&profile.join(LOCK)).map_err(anyhow::Error::msg)?;
    match fs::remove_file(path) {
        Err(error) if error.kind() != ErrorKind::NotFound => Err(error.into()),
        _ => Ok(()),
    }
}

/// Overlay the profile deadlines on a read-only view of workspace health.
/// Selection may persist its cursor, but must never copy a shared hold into
/// that workspace or a success elsewhere could not release it.
pub(super) fn apply_to_snapshot(
    health: &mut HashMap<AccountId, AccountHealth>,
    inventory: &[AccountDescriptor],
    now: u64,
) -> Result<()> {
    for account in inventory
        .iter()
        .filter(|a| a.enabled && a.id.provider == AccountProvider::Codex)
    {
        let Some(hold) = read(&account.credential_reference)?.filter(|hold| hold.until > now)
        else {
            continue;
        };
        let entry = health
            .entry(account.id.clone())
            .or_insert_with(|| AccountHealth {
                provider: account.id.provider,
                name: account.id.name.clone(),
                reason: HealthReason::Healthy,
                updated_at: hold.held_at,
                signal_provenance: PROVENANCE.to_owned(),
                cooldown_until: None,
                consecutive_transient_failures: 0,
                last_success: None,
                last_probe: None,
                class_cooldowns: HashMap::new(),
            });
        if entry.cooldown_until.is_none_or(|until| until < hold.until) {
            entry.cooldown_until = Some(hold.until);
            // Re-auth stays sticky. Preflight also needs the deadline for
            // a session-managed seat whose re-auth would otherwise be probed.
            if entry.reason != HealthReason::ReauthRequired {
                entry.reason = HealthReason::TransientFailure;
            }
            entry.signal_provenance = PROVENANCE.to_owned();
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::tokens_pool::health::{
        account_health, inventory_health_snapshot, record_terminal_for_model_at,
        TerminalClassification,
    };
    use crate::tokens_pool::profile_root_env::ProfileRootEnv;
    use tempfile::tempdir;

    #[test]
    fn hold_deadlines_age_out_and_a_shorter_window_cannot_replace_them() {
        let workspace = tempdir().unwrap();
        let profiles = tempdir().unwrap();
        let _root = ProfileRootEnv::set(profiles.path());
        fs::create_dir(profiles.path().join("one")).unwrap();
        let inventory = account_inventory_quiet(workspace.path(), AccountProvider::Codex).unwrap();
        let id = &inventory[0].id;
        assert!(!hold_seat(workspace.path(), id, 0, 1000).unwrap());
        assert!(!profiles.path().join("one").join(FILE).exists());
        assert!(hold_seat(workspace.path(), id, 600, 1000).unwrap());
        assert!(!hold_seat(workspace.path(), id, 10, 1001).unwrap());
        let before = inventory_health_snapshot(workspace.path(), &inventory, 1599).unwrap();
        assert_eq!(before[id].signal_provenance, PROVENANCE);
        assert!(!before[id].is_eligible_at(1599));
        assert!(inventory_health_snapshot(workspace.path(), &inventory, 1600)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_sandbox_hold_preserves_longer_cooldowns_and_sticky_reauth() {
        let workspace = tempdir().unwrap();
        let profiles = tempdir().unwrap();
        let _root = ProfileRootEnv::set(profiles.path());
        fs::create_dir(profiles.path().join("one")).unwrap();
        let inventory = account_inventory_quiet(workspace.path(), AccountProvider::Codex).unwrap();
        let id = &inventory[0].id;
        let now = 1_000;
        for category in [
            TerminalClassification::TokenExhausted,
            TerminalClassification::TokenExpired,
        ] {
            record_terminal_for_model_at(workspace.path(), id, category, None, "test", now)
                .unwrap();
            let original = account_health(workspace.path(), id).unwrap().unwrap();
            hold_seat(workspace.path(), id, 60, now).unwrap();
            let effective = inventory_health_snapshot(workspace.path(), &inventory, now).unwrap();
            assert_eq!(effective[id].reason, original.reason);
            assert_eq!(account_health(workspace.path(), id).unwrap().unwrap(), original);
            assert!(!effective[id].is_eligible_at(now));
            if let Some(until) = original.cooldown_until {
                assert_eq!(effective[id].cooldown_until, Some(until));
            }
        }
    }
}
