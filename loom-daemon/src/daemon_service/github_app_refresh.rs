//! GitHub App installation-token refresh ticks, extracted from `run_daemon`
//! (Issue #13641, slice 2 of the `run_daemon` decomposition; same shape as
//! `credential_startup` / `observer_tasks`).
//!
//! The caller keeps the `if let` guards (so "spawned whenever the App script is
//! configured" stays visible in `run_daemon`) and invokes the primary tick,
//! then the per-owner tick, after `fleet_sync::start` and before
//! `spawn_reader_refresh`. Both must be called from within the tokio runtime.
//!
//! #15327: the primary tick is bounded by [`PRIMARY_TICK_DEADLINE`], beats
//! the `github_app_refresh` task-liveness entry, and feeds the primary-token
//! expiry watch ([`credential_preflight::primary_token`]). Before that, a hang
//! in any await of the tick body stopped the loop with no log line, the writer
//! token expired an hour later, and every claim answered `401`.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use loom_daemon::credential_preflight;
use loom_daemon::credential_preflight::primary_token;
use loom_daemon::observability::ops::liveness::{self, Fault};
use loom_daemon::task_liveness;

use super::gh_token_needs_update;

/// Upper bound on one primary tick's body (mint, #11957 retry, #14223 drift
/// pass). The mint alone is bounded at `2 x 90 s + 2 s` by default and the
/// per-owner pass mints once per owner, so this is several multiples of that
/// and still a quarter of the ~1 h token life: a tick that overruns it is
/// abandoned, recorded as a primary-credential failure, and the next one runs.
pub(super) const PRIMARY_TICK_DEADLINE: Duration = Duration::from_secs(900);

/// Every this many ticks (~1 h) the loop logs one `info!` line, so a living
/// loop is visible in `daemon.log` even when every tick is a quiet cache hit.
const HEARTBEAT_EVERY_TICKS: u32 = 12;

/// What the primary tick needs that never changes.
struct PrimaryCtx {
    minter: Arc<dyn credential_preflight::GithubAppMinter + Send + Sync>,
    owner_repo: String,
    config_dir: PathBuf,
    /// `(workspace, script)` for the #11957 / #14223 owner passes.
    pass_workspace: PathBuf,
    pass_script: PathBuf,
}

/// What the primary tick carries from one run to the next.
struct PrimaryState {
    /// The token this task last published (never the process env — #4456).
    current_token: Option<String>,
    config_dir_active: bool,
    cross_owner_retry: credential_preflight::cross_owner::CrossOwnerRetry,
    drift_watch: credential_preflight::owner_drift::DriftWatch,
    ticks: u32,
    /// #14936: one diagnostic per continuous failure of this source.
    diagnostics: credential_preflight::RefreshDiagnostics,
}

/// Primary (workspace-root owner) refresh tick.
// The startup state this tick owns has grown with its responsibilities
// (mint refresh, #11957 retry, #14223 drift watch); each arg is a distinct
// piece of that state, matching `fleet_sync.rs`'s allow convention.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_primary_refresh(
    script_path: PathBuf,
    owner_repo: String,
    sweep_workspace: PathBuf,
    config_dir: PathBuf,
    minted_token: Option<String>,
    github_app_gh_config_dir_active: bool,
    cross_owner_retry: credential_preflight::cross_owner::CrossOwnerRetry,
    startup_roots: Vec<(PathBuf, String)>,
) {
    // #4430: ticks every `GITHUB_APP_REFRESH_INTERVAL` (~5min) to keep the
    // minted installation token fresh across its ~1h lifetime for a
    // long-running daemon. Ticks that don't rotate the token
    // never *write* anything: the `NotConfigured` arm is a true no-op, and a
    // cache-hit tick (the shell helper's own cache still has plenty of life —
    // ~10 of every ~11 ticks) compares against `current_token` (a plain local
    // captured by this task, never the process env — #4456's original
    // intent, now trivially true since there is no shared `GH_TOKEN` env var
    // left to read) and skips the write because the minted value is
    // unchanged. Only a genuine rotation (value differs) rewrites
    // `hosts.yml`; a mint failure is logged and the loop keeps whatever
    // credential `GH_CONFIG_DIR` already has on disk (fail-open, matching the
    // startup preflight's posture).
    let ctx = Arc::new(PrimaryCtx {
        minter: Arc::new(credential_preflight::RealGithubAppMinter {
            script_path: script_path.clone(),
            cwd: sweep_workspace.clone(),
        }),
        owner_repo,
        config_dir,
        pass_workspace: sweep_workspace,
        pass_script: script_path,
    });
    let state = Arc::new(tokio::sync::Mutex::new(PrimaryState {
        current_token: minted_token,
        config_dir_active: github_app_gh_config_dir_active,
        cross_owner_retry,
        // #14223: the owner-drift watch — the startup capture is the baseline
        // every tick compares the checkouts' current remotes against.
        drift_watch: credential_preflight::owner_drift::DriftWatch::new(startup_roots),
        ticks: 0,
        diagnostics: credential_preflight::RefreshDiagnostics::default(),
    }));
    let interval = credential_preflight::GITHUB_APP_REFRESH_INTERVAL;
    // #15327: the loop is a long-running task like the self-update loop: it
    // beats once per tick, and reads dead (loom-ui `daemon.task-dead`, the
    // `task_alive` gauge) once it stops, whatever the cause. The window
    // allows for one tick running its full deadline.
    task_liveness::register(
        task_liveness::GITHUB_APP_REFRESH,
        interval,
        task_liveness::default_stale_after(interval).saturating_add(PRIMARY_TICK_DEADLINE),
    );
    // #15327: gauges and the expired-token alert, on their own ticker so a
    // wedged tick cannot silence them.
    primary_token::spawn_monitor();
    tokio::spawn(drive_ticks(interval, PRIMARY_TICK_DEADLINE, None, move || {
        let (ctx, state) = (ctx.clone(), state.clone());
        async move {
            let mut state = state.lock().await;
            let primary_minted = refresh_mint(&ctx, &mut state).await;
            owner_passes(&ctx, &mut state, primary_minted).await;
        }
    }));
}

/// The loop: sleep `interval`, run `tick` under `deadline`, beat. `max_ticks`
/// bounds it for tests; production passes `None`.
async fn drive_ticks<F, Fut>(
    interval: Duration,
    deadline: Duration,
    max_ticks: Option<u32>,
    mut tick: F,
) where
    F: FnMut() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut ran = 0u32;
    loop {
        tokio::time::sleep(interval).await;
        if tokio::time::timeout(deadline, tick()).await.is_err() {
            on_tick_deadline(deadline);
        }
        // Beats whether the tick finished or was abandoned: the *loop* is
        // alive; the abandoned tick is reported as a credential failure.
        task_liveness::beat(task_liveness::GITHUB_APP_REFRESH, interval);
        ran += 1;
        if max_ticks.is_some_and(|m| ran >= m) {
            return;
        }
    }
}

/// A tick overran [`PRIMARY_TICK_DEADLINE`]: its futures are dropped (the
/// blocking threads under them finish or hang on their own), the failure is
/// recorded against the primary source, and the loop goes on.
fn on_tick_deadline(deadline: Duration) {
    credential_preflight::record_forge_credential_failure(
        credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
        &format!(
            "github-app refresh tick exceeded its {}s deadline and was abandoned",
            deadline.as_secs()
        ),
    );
    liveness::fault(task_liveness::GITHUB_APP_REFRESH, Fault::Overrun);
    log::warn!(
        "credential_preflight: github-app refresh tick exceeded its {}s deadline and was \
         abandoned; the primary token may not have rotated — main-health gate verdicts are \
         held for up to {}s while this persists — #15327",
        deadline.as_secs(),
        credential_preflight::resolve_forge_credential_stale_grace().as_secs()
    );
}

/// Mint, and publish when the token rotated. Returns whether the mint
/// succeeded (a precondition of the #11957 retry).
async fn refresh_mint(ctx: &Arc<PrimaryCtx>, state: &mut PrimaryState) -> bool {
    let (minter, repo) = (ctx.minter.clone(), ctx.owner_repo.clone());
    let outcome = tokio::task::spawn_blocking(move || minter.mint(&repo)).await;
    // #15327: a mint that is already expired is a failure, not a cache hit.
    let outcome = outcome.map(|o| primary_token::reject_expired_mint(o, Utc::now()));
    let primary_minted =
        matches!(outcome, Ok(credential_preflight::GithubAppOutcome::Minted { .. }));
    state.ticks = state.ticks.wrapping_add(1);
    match outcome {
        Ok(
            ref minted @ credential_preflight::GithubAppOutcome::Minted {
                ref token,
                ref installation_id,
                ref app_id,
                ref expires_at,
            },
        ) => {
            // #5630: a successful mint (fresh or cache hit) ends this
            // source's credential-failure streak, so the main-health
            // gate goes back to trusting its forge answers on the very
            // next tick. A publish failure below re-opens it — an
            // unpublished token is just as un-refreshed on disk.
            credential_preflight::record_forge_credential_success(
                credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
            );
            // #4456/#4458: only rewrite when the value actually
            // rotated, and do it as a pure file write — no
            // `std::env::set_var` anywhere in this arm once
            // `config_dir_active` is already true (the common
            // case: activation happened at startup).
            if gh_token_needs_update(state.current_token.as_deref(), token) {
                // Rare edge case: the app becomes configured
                // *after* startup (e.g. the shell helper's own
                // config appears mid-run) without the daemon
                // restarting, so activation didn't happen there.
                // This is a second, still-bounded (at most once
                // per process) `std::env::set_var` window — the
                // recurring, steady-state race #4458 targets
                // is the one eliminated; this cold-start
                // transition is unchanged in kind from the
                // pre-#4458 code's own equivalent one-time
                // `set_var` in this same arm.
                if !state.config_dir_active {
                    std::env::remove_var("GH_TOKEN");
                    std::env::remove_var("GITHUB_TOKEN");
                    std::env::set_var("GH_CONFIG_DIR", &ctx.config_dir);
                    state.config_dir_active = true;
                }
                match credential_preflight::publish_outcome(
                    &ctx.config_dir,
                    minted,
                    &ctx.owner_repo,
                ) {
                    Ok(()) => {
                        state.current_token = Some(token.clone());
                        primary_token::record_published(&ctx.owner_repo, expires_at); // #15327
                        state
                            .diagnostics
                            .success(credential_preflight::CREDENTIAL_SOURCE_PRIMARY);
                        log::info!(
                            "credential_preflight: github-app refresh tick rotated the \
                             primary writer token (app {app_id} installation {installation_id}, \
                             expires {expires_at}) — #4430/#15327"
                        );
                    }
                    Err(e) => {
                        // #5630: the mint succeeded but the token never
                        // reached disk, so `GH_CONFIG_DIR` still holds the
                        // aging one — same staleness as a failed mint.
                        credential_preflight::record_forge_credential_failure(
                            credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                            &format!("could not publish the minted token: {e}"),
                        );
                        state.diagnostics.failure(
                            &ctx.pass_workspace,
                            credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                            &ctx.config_dir,
                            &format!("could not publish the minted token: {e}"),
                        );
                        log::warn!(
                            "credential_preflight: github-app refresh tick failed to \
                             publish token to {} ({e}); GH_CONFIG_DIR left unchanged \
                             (app {app_id} installation {installation_id}) — #4458",
                            ctx.config_dir.display()
                        );
                    }
                }
            } else {
                state
                    .diagnostics
                    .success(credential_preflight::CREDENTIAL_SOURCE_PRIMARY);
                log::debug!(
                    "credential_preflight: github-app refresh tick cache hit, \
                     GH_TOKEN unchanged (app {app_id} installation {installation_id}) \
                     — #4456"
                );
            }
            // #15327: one quiet line an hour, so an alive loop is visible.
            if state.ticks.is_multiple_of(HEARTBEAT_EVERY_TICKS) {
                match primary_token::status() {
                    Some(s) => log::info!(
                        "credential_preflight: github-app refresh loop alive after {} tick(s); \
                         primary token last rotated {}s ago, {} — #15327",
                        state.ticks,
                        s.age_secs,
                        s.remaining_secs
                            .map_or_else(|| "expiry unknown".to_string(), |r| format!("{r}s left"))
                    ),
                    None => log::info!(
                        "credential_preflight: github-app refresh loop alive after {} tick(s); \
                         no primary token published by this process yet — #15327",
                        state.ticks
                    ),
                }
            }
        }
        Ok(credential_preflight::GithubAppOutcome::NotConfigured) => {
            // Cheap no-op tick -- nothing to refresh.
        }
        Ok(credential_preflight::GithubAppOutcome::Error(reason)) => {
            // #5630: the credential on disk is now aging with no
            // successful refresh behind it. Record the streak so the
            // main-health gate holds its previous verdicts instead of
            // reading the resulting forge failures as "every repo's
            // main just went red".
            credential_preflight::record_forge_credential_failure(
                credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                &reason,
            );
            state.diagnostics.failure(
                &ctx.pass_workspace,
                credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                &ctx.config_dir,
                &reason,
            );
            log::warn!(
                "credential_preflight: github-app refresh tick failed ({reason}); \
                 GH_TOKEN left unchanged — main-health gate verdicts are held for up \
                 to {}s while this persists — #4430/#5630",
                credential_preflight::resolve_forge_credential_stale_grace().as_secs()
            );
        }
        Err(e) => {
            // A join error means the mint never completed either, so
            // the credential is just as un-refreshed as an Error arm.
            credential_preflight::record_forge_credential_failure(
                credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                &format!("github-app refresh task join error: {e}"),
            );
            state.diagnostics.failure(
                &ctx.pass_workspace,
                credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
                &ctx.config_dir,
                &format!("github-app refresh task join error: {e}"),
            );
            log::warn!("credential_preflight: github-app refresh task join error: {e} — #4430");
        }
    }
    primary_minted
}

/// The owner passes that follow a mint: #11957 retry and #14223 drift watch.
async fn owner_passes(ctx: &Arc<PrimaryCtx>, state: &mut PrimaryState, primary_minted: bool) {
    // #11957: run the per-owner pass startup skipped or could not
    // finish, once the App mints again. After the primary publish
    // above, so the cached refusals it expires are filed under
    // the process credential as it now stands.
    if primary_minted && state.cross_owner_retry.due() {
        let pass = credential_preflight::cross_owner::establish_for_workspace_off_thread(
            ctx.pass_workspace.clone(),
            ctx.owner_repo.clone(),
            ctx.pass_script.clone(),
        )
        .await;
        state.cross_owner_retry.record(&pass);
    }
    // #14223: owner-drift watch — a checkout re-pointed at another
    // owner after startup keeps the credential it was resolved for
    // and sheds reads silently (the #14170 incident). Bounded: one
    // `git remote get-url` per managed root; the canonical-name
    // probe only when no owner drifted, cached per repo. Repairs
    // what it can (one re-establish pass, plus dropping a root
    // re-pointed back at this workspace's own owner) and warns ONCE
    // per repo+owner pair when it cannot.
    //
    // The watch is cloned in and assigned back, so an abandoned tick
    // (#15327) keeps its previous baseline instead of losing it.
    state.drift_watch = credential_preflight::owner_drift::check_off_thread(
        ctx.pass_workspace.clone(),
        ctx.owner_repo.clone(),
        ctx.pass_script.clone(),
        state.drift_watch.clone(),
    )
    .await;
}

/// Per-owner credential refresh tick.
pub(super) fn spawn_owner_refresh(script_path: PathBuf, sweep_workspace: PathBuf) {
    // #5401: keep each per-owner credential fresh across its ~1h lifetime,
    // ticking every `GITHUB_APP_REFRESH_INTERVAL` (~5min) — the same cadence
    // as the workspace-root owner's tick above. Re-mint the
    // owner's representative repo and republish into its `GH_CONFIG_DIR` — a
    // pure file rewrite via #4458's delivery path; the root->dir registration
    // is path-stable and never needs re-registering. A fail-open loop
    // (matching the startup posture): a mint/publish miss logs and keeps
    // whatever token that owner's dir already holds.
    //
    // #6171: spawned whenever the App mechanism itself is configured
    // (`github_app_script.is_some()`), NOT only when a per-owner plan
    // happened to exist at startup — the source list
    // (`credential_preflight::owner_refresh_sources()`) is re-read from the
    // shared registry on every tick, so an owner discovered later via
    // `force_refresh_owner_credential`'s 404-recovery path (including a
    // fleet that started single-owner and only later grew a cross-owner
    // workspace) is picked up automatically, without a second task to spawn.
    // An empty registry makes a tick a cheap no-op loop, same cost as the
    // pre-#6171 early return.
    //
    // #14936: each source's continuous failure gets one diagnostic, by the
    // same latch the primary tick keeps.
    let workspace = sweep_workspace;
    let mut diagnostics = credential_preflight::RefreshDiagnostics::default();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(credential_preflight::GITHUB_APP_REFRESH_INTERVAL).await;
            let owners = credential_preflight::owner_refresh_sources();
            for (owner_repo, config_dir) in &owners {
                let script_path = script_path.clone();
                let cwd = workspace.clone();
                let owner_repo_for_mint = owner_repo.clone();
                let outcome = tokio::task::spawn_blocking(move || {
                    let minter = credential_preflight::RealGithubAppMinter { script_path, cwd };
                    credential_preflight::GithubAppMinter::mint(&minter, &owner_repo_for_mint)
                })
                .await;
                // #5630: each owner's credential is its own refresh
                // source. A saturated host times these out alongside the
                // primary tick, and their repos' forge calls go just as
                // wrong — so they feed the same staleness tracker, keyed
                // separately so a healthy owner's success cannot clear a
                // failing owner's streak.
                let source = credential_preflight::credential_source_for_owner(owner_repo);
                match outcome {
                    Ok(ref minted @ credential_preflight::GithubAppOutcome::Minted { .. }) => {
                        if let Err(e) =
                            credential_preflight::publish_outcome(config_dir, minted, owner_repo)
                        {
                            credential_preflight::record_forge_credential_failure(
                                &source,
                                &format!("could not publish the minted token: {e}"),
                            );
                            diagnostics.failure(
                                &workspace,
                                &source,
                                config_dir,
                                &format!("could not publish the minted token: {e}"),
                            );
                            log::warn!(
                                "credential_preflight: per-owner github-app refresh could \
                                     not publish the token for {owner_repo} to {} ({e}); its \
                                     credential left unchanged — #5401",
                                config_dir.display()
                            );
                        } else {
                            credential_preflight::record_forge_credential_success(&source);
                            diagnostics.success(&source);
                        }
                    }
                    Ok(credential_preflight::GithubAppOutcome::NotConfigured) => {
                        // Nothing to refresh for this owner. A source is only
                        // registered once its owner's writer was minted, so
                        // an App that no longer mints is an inactive
                        // publisher: the credential on disk ages with no
                        // renewal and nothing else says so (#14936). Diagnosed
                        // once per streak; the health-gate tracker is left
                        // alone, as before.
                        diagnostics.failure(
                            &workspace,
                            &source,
                            config_dir,
                            "the GitHub App is not configured, so this owner's writer is not \
                             being renewed",
                        );
                    }
                    Ok(credential_preflight::GithubAppOutcome::Error(reason)) => {
                        credential_preflight::record_forge_credential_failure(&source, &reason);
                        diagnostics.failure(&workspace, &source, config_dir, &reason);
                        log::warn!(
                            "credential_preflight: per-owner github-app refresh for \
                                 {owner_repo} failed ({reason}); its credential left \
                                 unchanged — main-health gate verdicts are held for up to \
                                 {}s while this persists — #5401/#5630",
                            credential_preflight::resolve_forge_credential_stale_grace().as_secs()
                        );
                    }
                    Err(e) => {
                        credential_preflight::record_forge_credential_failure(
                            &source,
                            &format!("per-owner refresh task join error: {e}"),
                        );
                        diagnostics.failure(
                            &workspace,
                            &source,
                            config_dir,
                            &format!("per-owner refresh task join error: {e}"),
                        );
                        log::warn!(
                            "credential_preflight: per-owner github-app refresh task join \
                                 error for {owner_repo}: {e} — #5401"
                        );
                    }
                }
            }
        }
    });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use credential_preflight::GithubAppOutcome;
    use serial_test::serial;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct FixedMinter(GithubAppOutcome);
    impl credential_preflight::GithubAppMinter for FixedMinter {
        fn mint(&self, _owner_repo: &str) -> GithubAppOutcome {
            self.0.clone()
        }
    }

    fn minted(token: &str, expires_at: &str) -> GithubAppOutcome {
        GithubAppOutcome::Minted {
            token: token.into(),
            installation_id: "9".into(),
            app_id: "4".into(),
            expires_at: expires_at.into(),
        }
    }

    fn ctx(dir: &std::path::Path, outcome: GithubAppOutcome) -> Arc<PrimaryCtx> {
        Arc::new(PrimaryCtx {
            minter: Arc::new(FixedMinter(outcome)),
            owner_repo: "2AMLogic/loom".into(),
            config_dir: dir.join("gh-config"),
            pass_workspace: dir.to_path_buf(),
            pass_script: dir.join("none.sh"),
        })
    }

    fn state() -> PrimaryState {
        PrimaryState {
            current_token: None,
            config_dir_active: true, // never touch the process env in a test
            cross_owner_retry: credential_preflight::cross_owner::CrossOwnerRetry::after_startup(
                None,
            ),
            drift_watch: credential_preflight::owner_drift::DriftWatch::default(),
            ticks: 0,
            diagnostics: credential_preflight::RefreshDiagnostics::default(),
        }
    }

    fn clear_primary_failure() {
        credential_preflight::record_forge_credential_success(
            credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
        );
    }

    #[tokio::test]
    #[serial]
    async fn a_tick_past_its_deadline_is_recorded_as_a_failure_and_the_next_tick_runs() {
        clear_primary_failure();
        let calls = Arc::new(AtomicU32::new(0));
        let seen = calls.clone();
        drive_ticks(Duration::from_millis(5), Duration::from_millis(40), Some(2), move || {
            let n = seen.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    // the wedge: an await that never returns
                    std::future::pending::<()>().await;
                }
            }
        })
        .await;
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the loop ran the tick after the wedged one");
        assert!(credential_preflight::forge_credential_stale());
        let summary = credential_preflight::forge_credential_stale_summary().unwrap();
        assert!(summary.contains("deadline"), "{summary}");
        assert!(
            summary.starts_with(credential_preflight::CREDENTIAL_SOURCE_PRIMARY),
            "{summary}"
        );
        clear_primary_failure();
    }

    #[tokio::test]
    #[serial]
    async fn an_already_expired_mint_is_a_failure_and_is_never_published() {
        clear_primary_failure();
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx(tmp.path(), minted("ghs_dead", "2020-01-01T00:00:00Z"));
        let mut st = state();
        assert!(!refresh_mint(&ctx, &mut st).await, "an expired mint is not a mint");
        assert!(credential_preflight::forge_credential_stale());
        let summary = credential_preflight::forge_credential_stale_summary().unwrap();
        assert!(summary.contains("already expired"), "{summary}");
        assert!(!ctx.config_dir.join("hosts.yml").exists());
        assert_eq!(st.current_token, None);
        clear_primary_failure();
    }

    #[tokio::test]
    #[serial]
    async fn a_live_mint_publishes_records_the_rotation_and_clears_the_failure() {
        credential_preflight::record_forge_credential_failure(
            credential_preflight::CREDENTIAL_SOURCE_PRIMARY,
            "earlier failure",
        );
        let tmp = tempfile::tempdir().unwrap();
        let ctx = ctx(tmp.path(), minted("ghs_live", "2099-01-01T00:00:00Z"));
        let mut st = state();
        assert!(refresh_mint(&ctx, &mut st).await);
        assert!(!credential_preflight::forge_credential_stale());
        let hosts = std::fs::read_to_string(ctx.config_dir.join("hosts.yml")).unwrap();
        assert!(hosts.contains("ghs_live"));
        assert_eq!(st.current_token.as_deref(), Some("ghs_live"));
        let status = primary_token::status().unwrap();
        assert_eq!(status.owner, "2AMLogic");
        assert!(status.remaining_secs.unwrap() > 0);
        // a cache hit (same token) rewrites nothing
        std::fs::remove_file(ctx.config_dir.join("hosts.yml")).unwrap();
        assert!(refresh_mint(&ctx, &mut st).await);
        assert!(!ctx.config_dir.join("hosts.yml").exists());
    }
}
