//! Unit tests for [`super`] — the provider-scoped account-health surface.
//!
//! Extracted to a sibling file (#8058 Phase 2) when the class-scoped
//! `MODEL_CREDITS_EXHAUSTED` work grew `health.rs`; it is a pure move — every
//! test below is unchanged by the extraction itself.

use super::*;
use crate::tokens_pool::account_registry::{CredentialKind, InventoryProvenance};

fn descriptor(provider: AccountProvider, name: &str) -> AccountDescriptor {
    AccountDescriptor {
        id: AccountId {
            provider,
            name: name.into(),
        },
        credential_kind: CredentialKind::CodexHome,
        credential_reference: PathBuf::from(name),
        enabled: true,
        provenance: InventoryProvenance::Shared,
        email: None,
    }
}

#[test]
fn exhausted_fails_over_and_expires_at_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    assert_eq!(
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
            .unwrap()
            .id
            .name,
        "b"
    );
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS
    )
    .is_ok());
}

/// #8058 Phase 2 — the divergence this phase exists to create: a credit
/// exhaustion that names its model records a **class-scoped** hold, while
/// `TOKEN_EXHAUSTED` still records the account-wide one. Replaces
/// `model_credits_exhausted_is_recorded_identically_to_token_exhausted`,
/// which pinned the pre-#8058 equivalence.
#[test]
fn model_credits_exhausted_diverges_from_token_exhausted_when_the_class_is_known() {
    assert_eq!(
        "MODEL_CREDITS_EXHAUSTED"
            .parse::<TerminalClassification>()
            .unwrap(),
        TerminalClassification::ModelCreditsExhausted
    );

    // TOKEN_EXHAUSTED: account-wide, verbatim as before.
    let tmp = tempfile::tempdir().unwrap();
    let account = descriptor(AccountProvider::Codex, "a");
    record_terminal_for_model_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &account.id).unwrap().unwrap();
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.cooldown_until, Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS));
    assert!(
        entry.class_cooldowns.is_empty(),
        "a plan/quota exhaustion is an account-level fact, never class-scoped"
    );

    // MODEL_CREDITS_EXHAUSTED: class-scoped, no account-wide cooldown.
    let tmp = tempfile::tempdir().unwrap();
    record_terminal_for_model_at(
        tmp.path(),
        &account.id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &account.id).unwrap().unwrap();
    assert_eq!(entry.reason, HealthReason::ModelCreditsExhausted);
    assert_eq!(entry.cooldown_until, None);
    assert_eq!(
        entry.class_cooldowns.get("gpt-5-codex").copied(),
        Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS)
    );
}

/// The starvation fix itself: one class hitting its ceiling must not cost
/// the account every other class — while the account-wide question keeps
/// its pre-#8058 answer, exactly as Phase 1's `is_bad` does.
#[test]
fn a_class_scoped_credit_hold_starves_only_its_own_class() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();

    // The class that ran out is blocked...
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        101
    )
    .is_err());
    // ...every other class is not...
    assert_eq!(
        select_healthy_for_model_at(
            tmp.path(),
            AccountProvider::Codex,
            &accounts,
            Some("gpt-5-mini"),
            101
        )
        .unwrap()
        .id
        .name,
        "a"
    );
    // ...and the account-wide question is unchanged: still blocked, and
    // still reported as a cooldown rather than a bare "unavailable".
    let error = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains(&format!(
            "model_class=gpt-5-codex cooldown_until={}",
            100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS
        )),
        "{error}"
    );
    // Capacity must agree with selection, not report a phantom free seat.
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!((capacity.healthy, capacity.cooldown), (0, 1));

    // The hold ages out on its own schedule, and takes the reason with it.
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS
    )
    .is_ok());
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.reason, HealthReason::Healthy);
    assert!(entry.class_cooldowns.is_empty());
}

/// The fail-safe half of the design constraint: a class-scoped mark may
/// only ever be NARROWER than an account-wide one. Every route that yields
/// no usable class must reproduce the pre-#8058 account-wide hold.
#[test]
fn a_credit_exhaustion_without_a_usable_class_stays_account_wide() {
    for model in [
        None,
        Some(""),
        Some("   "),
        Some("gpt 5 codex"),
        Some("\"x\""),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let accounts = vec![descriptor(AccountProvider::Codex, "a")];
        record_terminal_for_model_at(
            tmp.path(),
            &accounts[0].id,
            TerminalClassification::ModelCreditsExhausted,
            model,
            "adapter_v1",
            100,
        )
        .unwrap();
        let entry = account_health(tmp.path(), &accounts[0].id)
            .unwrap()
            .unwrap();
        assert_eq!(entry.reason, HealthReason::PlanExhausted, "model={model:?}");
        assert_eq!(
            entry.cooldown_until,
            Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS),
            "model={model:?}"
        );
        assert!(entry.class_cooldowns.is_empty(), "model={model:?}");
        // Account-wide means account-wide: naming a class cannot escape it.
        assert!(
            select_healthy_for_model_at(
                tmp.path(),
                AccountProvider::Codex,
                &accounts,
                Some("gpt-5-mini"),
                101
            )
            .is_err(),
            "model={model:?}"
        );
    }
}

/// Classes are independent of each other, and a class-scoped success
/// releases only the class it names.
#[test]
fn class_holds_are_independent_and_success_clears_only_its_own_class() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    for (model, at) in [("gpt-5-codex", 100), ("gpt-5-mini", 200)] {
        record_terminal_for_model_at(
            tmp.path(),
            &accounts[0].id,
            TerminalClassification::ModelCreditsExhausted,
            Some(model),
            "adapter_v1",
            at,
        )
        .unwrap();
    }
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.class_cooldowns.len(), 2);
    // Each ages from its own timestamp.
    assert_eq!(
        entry.class_cooldowns.get("gpt-5-codex").copied(),
        Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS)
    );
    assert_eq!(
        entry.class_cooldowns.get("gpt-5-mini").copied(),
        Some(200 + DEFAULT_EXHAUSTED_COOLDOWN_SECS)
    );

    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::Success,
        Some("gpt-5-codex"),
        "adapter_v1",
        300,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.class_cooldowns.keys().collect::<Vec<_>>(), ["gpt-5-mini"]);
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        301
    )
    .is_ok());
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-mini"),
        301
    )
    .is_err());

    // A class-less success is the account-wide statement it always was.
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::Success,
        "adapter_v1",
        302,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert!(entry.class_cooldowns.is_empty());
    assert_eq!(entry.reason, HealthReason::Healthy);
}

/// A narrower fact must never overwrite a wider live hold: an account
/// already held account-wide keeps that hold (and its reason) when a
/// class-scoped credit signal lands on top of it.
#[test]
fn a_class_hold_never_narrows_a_live_account_wide_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-mini"),
        "adapter_v1",
        101,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.cooldown_until, Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS));
    assert!(select_healthy_for_model_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        Some("gpt-5-codex"),
        102
    )
    .is_err());
}

/// Claude-family models collapse to the class Phase 1 already uses, so the
/// two phases can never disagree about a shared vocabulary; everything
/// else normalizes to itself.
#[test]
fn model_classes_share_phase_ones_vocabulary_where_it_applies() {
    assert_eq!(model_class_of("claude-opus-5").as_deref(), Some("opus"));
    assert_eq!(model_class_of("opus").as_deref(), Some("opus"));
    assert_eq!(model_class_of("OPUS").as_deref(), Some("opus"));
    assert_eq!(model_class_of("gpt-5-codex").as_deref(), Some("gpt-5-codex"));
    assert_eq!(model_class_of(" GPT-5-Codex ").as_deref(), Some("gpt-5-codex"));
    assert_eq!(model_class_of(""), None);
    assert_eq!(model_class_of("gpt 5 codex"), None);
}

/// #8380: a `model@…` suffix — Loom's own `model@effort` rung grammar, or a
/// pinned `model@date` ID — names an effort or a snapshot of the SAME model,
/// never a second credit pool, so it must collapse onto the bare model's class.
/// Before this, `normalize_class` rejected `@` outright and every suffixed
/// model degraded to the account-wide path, costing a pinning fleet the whole
/// of Phase 2.
#[test]
fn a_suffixed_model_collapses_onto_its_bare_class() {
    // The issue's own example, and the effort-grammar sibling.
    assert_eq!(model_class_of("gpt-5-codex@2026-01-01"), model_class_of("gpt-5-codex"));
    assert_eq!(model_class_of("gpt-5-codex@high"), model_class_of("gpt-5-codex"));
    assert_eq!(model_class_of("gpt-5-codex@2026-01-01").as_deref(), Some("gpt-5-codex"));
    // Trim/lowercase still apply to the base half, and only the FIRST `@`
    // splits — same rule as `model_tiers::base_of` / `model_family`.
    assert_eq!(model_class_of(" GPT-5-Codex@High ").as_deref(), Some("gpt-5-codex"));
    assert_eq!(model_class_of("gpt-5-codex@a@b").as_deref(), Some("gpt-5-codex"));
    // A Claude-family suffixed ID keeps collapsing through Phase 1's
    // vocabulary, not through the fallback.
    assert_eq!(model_class_of("claude-opus-5@xhigh").as_deref(), Some("opus"));

    // Fail-safe edges: an empty base is still no class (never the effort), and
    // a value that is unrecognized for some OTHER reason stays unrecognized.
    assert_eq!(model_class_of("@high"), None);
    assert_eq!(model_class_of("  @2026-01-01"), None);
    assert_eq!(model_class_of("gpt-5\"codex@high"), None);
}

/// The production consequence of the collapse above: a class hold written from
/// a pinned/suffixed ID blocks the bare alias (and vice versa), because both
/// resolve to one `class_cooldowns` key. The sibling class is untouched.
#[test]
fn a_hold_written_from_a_pinned_id_blocks_the_bare_alias() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex@2026-01-01"),
        "adapter_v2",
        100,
    )
    .unwrap();

    // The hold is keyed by the BARE class, not the pinned string.
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(
        entry.class_cooldowns.keys().collect::<Vec<_>>(),
        vec!["gpt-5-codex"],
        "{:?}",
        entry.class_cooldowns
    );

    // …so selection blocks the bare alias, the pin it was written from, and a
    // differently-suffixed form of the same model alike.
    for held in ["gpt-5-codex", "gpt-5-codex@2026-01-01", "gpt-5-codex@high"] {
        assert!(
            select_healthy_for_model_at(
                tmp.path(),
                AccountProvider::Codex,
                &accounts,
                Some(held),
                101
            )
            .is_err(),
            "{held} must be blocked by the collapsed hold"
        );
    }
    // A genuinely different class is still selectable — the narrowing did not
    // widen into an account-wide outage.
    assert_eq!(
        select_healthy_for_model_at(
            tmp.path(),
            AccountProvider::Codex,
            &accounts,
            Some("gpt-5-mini@high"),
            101
        )
        .unwrap()
        .id
        .name,
        "a"
    );
}

/// A state file written before #8058 Phase 2 (no `class_cooldowns` key)
/// must still read back at the unchanged `SCHEMA_VERSION`, as account-wide
/// as the day it was written — the field is additive and optional, exactly
/// like `last_probe` (#6927) before it.
#[test]
fn pre_phase_two_state_reads_back_unchanged_and_stays_account_wide() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".loom")).unwrap();
    fs::write(
        state_path(tmp.path()),
        r#"{"version":1,"accounts":[{"provider":"codex","name":"a","reason":"plan_exhausted",
           "updated_at":100,"signal_provenance":"adapter_v1","cooldown_until":500,
           "consecutive_transient_failures":0}],"cursors":{}}"#,
    )
    .unwrap();
    let entry = read_state(tmp.path()).unwrap().accounts.remove(0);
    assert!(entry.class_cooldowns.is_empty());
    assert!(!entry.is_eligible_for_class_at(101, Some("gpt-5-codex")));
    assert!(entry.is_eligible_for_class_at(500, Some("gpt-5-codex")));

    // An account with no class hold serializes byte-identically to before:
    // the new key is skipped entirely when empty.
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "a".into(),
    };
    record_terminal_at(tmp.path(), &id, TerminalClassification::TokenExhausted, "adapter_v1", 1)
        .unwrap();
    let written = fs::read_to_string(state_path(tmp.path())).unwrap();
    assert!(!written.contains("class_cooldowns"), "{written}");
    assert!(written.contains("\"version\": 1"), "{written}");
}

#[test]
fn expired_survives_time_and_success_until_explicit_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let account = descriptor(AccountProvider::Codex, "a");
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        1,
    )
    .unwrap();
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::Success,
        "adapter_v1",
        u64::MAX - 1,
    )
    .unwrap();
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        std::slice::from_ref(&account),
        u64::MAX
    )
    .is_err());
    clear_reauth(tmp.path(), &account.id, "verified_reauth").unwrap();
    assert!(select_healthy_at(tmp.path(), AccountProvider::Codex, &[account], u64::MAX).is_ok());
}

#[test]
fn expired_survives_all_later_runtime_feedback_until_explicit_clear() {
    // #8058 Phase 2: the class-scoped signal is swallowed by the sticky
    // reauth hold exactly like every other terminal category — a broken
    // credential is broken for every class, so a narrower mark must not be
    // able to displace the wider permanent one.
    for (classification, model) in [
        (TerminalClassification::TokenExhausted, None),
        (TerminalClassification::Recoverable, None),
        (TerminalClassification::SessionLimit, None),
        (TerminalClassification::ModelCreditsExhausted, Some("gpt-5-codex")),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let account = descriptor(AccountProvider::Codex, "a");
        record_terminal_at(
            tmp.path(),
            &account.id,
            TerminalClassification::TokenExpired,
            "adapter_v1",
            1,
        )
        .unwrap();
        record_terminal_for_model_at(
            tmp.path(),
            &account.id,
            classification,
            model,
            "adapter_v1",
            2,
        )
        .unwrap();

        let health = account_health(tmp.path(), &account.id).unwrap().unwrap();
        assert_eq!(health.reason, HealthReason::ReauthRequired);
        assert_eq!(health.cooldown_until, None);
        assert!(health.class_cooldowns.is_empty());
        assert!(select_healthy_at(
            tmp.path(),
            AccountProvider::Codex,
            std::slice::from_ref(&account),
            u64::MAX
        )
        .is_err());
        // ...including for a caller that names a different class.
        assert!(select_healthy_for_model_at(
            tmp.path(),
            AccountProvider::Codex,
            std::slice::from_ref(&account),
            Some("gpt-5-mini"),
            u64::MAX
        )
        .is_err());
    }
}

#[test]
fn expired_transient_backoff_restores_fair_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::Recoverable,
        "adapter_v1",
        100,
    )
    .unwrap();

    assert_eq!(
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
            .unwrap()
            .id
            .name,
        "b"
    );

    let deadline = 100 + DEFAULT_RECOVERABLE_BACKOFF_SECS;
    let first = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, deadline).unwrap();
    let second =
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, deadline).unwrap();
    assert_ne!(first.id, second.id);
    assert!([first.id.name, second.id.name].contains(&"a".to_string()));

    let recovered = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(recovered.reason, HealthReason::Healthy);
    assert_eq!(recovered.cooldown_until, None);
    assert_eq!(recovered.consecutive_transient_failures, 0);
}

#[test]
fn neutral_failures_do_not_poison_and_same_names_are_provider_scoped() {
    let tmp = tempfile::tempdir().unwrap();
    let codex = descriptor(AccountProvider::Codex, "same");
    let claude = descriptor(AccountProvider::Claude, "same");
    record_terminal_at(
        tmp.path(),
        &codex.id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        1,
    )
    .unwrap();
    for category in [
        TerminalClassification::Timeout,
        TerminalClassification::Fatal,
        TerminalClassification::CwdDeleted,
        TerminalClassification::ModelRefusal,
    ] {
        record_terminal_at(tmp.path(), &claude.id, category, "adapter_v1", 2).unwrap();
    }
    assert!(account_health(tmp.path(), &claude.id).unwrap().is_none());
    assert_eq!(
        account_health(tmp.path(), &codex.id)
            .unwrap()
            .unwrap()
            .reason,
        HealthReason::ReauthRequired
    );
}

/// Issue #6927: a proactive probe that finds an account logged out must
/// exclude it from selection through the SAME `ReauthRequired` mechanism
/// a reactive `TOKEN_EXPIRED` uses — before any dispatch is attempted.
#[test]
fn a_not_logged_in_probe_excludes_the_account_and_is_reported_as_such() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    assert_eq!(
        record_probe_at(
            tmp.path(),
            &accounts[0].id,
            ProbeOutcome::NotLoggedIn,
            "session_probe",
            100,
        )
        .unwrap(),
        ProbeEffect::MarkedReauthRequired
    );
    let entry = account_health(tmp.path(), &accounts[0].id)
        .unwrap()
        .unwrap();
    assert_eq!(entry.reason, HealthReason::ReauthRequired);
    assert_eq!(entry.last_probe, Some(100));
    assert_eq!(entry.cooldown_until, None);
    assert_eq!(
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
            .unwrap()
            .id
            .name,
        "b"
    );
    // Re-probing an already-held account is idempotent, not a re-mark.
    assert_eq!(
        record_probe_at(
            tmp.path(),
            &accounts[0].id,
            ProbeOutcome::NotLoggedIn,
            "session_probe",
            200,
        )
        .unwrap(),
        ProbeEffect::Unchanged
    );
}

/// A healthy probe IS the "independently verified reauth" `clear_reauth`
/// demands — it observed the live credential, which ordinary runtime
/// feedback cannot.
#[test]
fn a_logged_in_probe_releases_a_reauth_hold_but_touches_nothing_else() {
    let tmp = tempfile::tempdir().unwrap();
    let account = descriptor(AccountProvider::Codex, "a");
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        1,
    )
    .unwrap();
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        std::slice::from_ref(&account),
        2
    )
    .is_err());

    assert_eq!(
        record_probe_at(tmp.path(), &account.id, ProbeOutcome::LoggedIn, "session_probe", 3,)
            .unwrap(),
        ProbeEffect::ClearedReauthHold
    );
    assert!(select_healthy_at(
        tmp.path(),
        AccountProvider::Codex,
        std::slice::from_ref(&account),
        4
    )
    .is_ok());

    // A healthy auth probe says nothing about quota, so it must not
    // rescue an exhausted account from its cooldown.
    record_terminal_at(
        tmp.path(),
        &account.id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        5,
    )
    .unwrap();
    assert_eq!(
        record_probe_at(tmp.path(), &account.id, ProbeOutcome::LoggedIn, "session_probe", 6,)
            .unwrap(),
        ProbeEffect::Unchanged
    );
    let entry = account_health(tmp.path(), &account.id).unwrap().unwrap();
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.cooldown_until, Some(5 + DEFAULT_EXHAUSTED_COOLDOWN_SECS));
    assert_eq!(entry.last_probe, Some(6));
    // Nor does it fabricate a dispatch success.
    assert_eq!(entry.last_success, None);
}

#[test]
fn probe_records_survive_a_read_write_round_trip_and_reject_empty_identity() {
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "a".into(),
    };
    record_probe_at(tmp.path(), &id, ProbeOutcome::NotLoggedIn, "session_probe", 7).unwrap();
    // Written state must still parse under `deny_unknown_fields` (the new
    // `last_probe` field is part of the schema, not a stowaway).
    assert_eq!(read_state(tmp.path()).unwrap().accounts[0].last_probe, Some(7));
    assert!(record_probe_at(tmp.path(), &id, ProbeOutcome::LoggedIn, "", 8).is_err());
}

#[test]
fn round_robin_is_persistent_and_capacity_is_honest() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    let first = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
    let second = select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
    assert_ne!(first.id, second.id);
    let capacity = provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 1).unwrap();
    assert_eq!((capacity.raw, capacity.enabled, capacity.healthy), (2, 2, 2));
}

#[test]
fn state_never_contains_credentials_or_raw_output() {
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "safe-name".into(),
    };
    record_terminal_at(tmp.path(), &id, TerminalClassification::Recoverable, "adapter_v1", 1)
        .unwrap();
    let state = fs::read_to_string(state_path(tmp.path())).unwrap();
    assert!(!state.contains("auth.json"));
    assert!(!state.contains("recognizable-secret"));
}

#[test]
fn malformed_and_unknown_schema_fail_closed() {
    let tmp = tempfile::tempdir().unwrap();
    fs::create_dir(tmp.path().join(".loom")).unwrap();
    fs::write(state_path(tmp.path()), r#"{"version":99,"accounts":[]}"#).unwrap();
    assert!(read_state(tmp.path()).is_err());
    fs::write(state_path(tmp.path()), "{broken").unwrap();
    assert!(read_state(tmp.path()).is_err());
}

// ============================================================================
// Per-model-class capacity reporting (#8058 Phase 3)
// ============================================================================

/// AC2's degradation clause on this surface: a provider with no class-scoped
/// state reports an empty map, so every consumer falls back to `healthy` and
/// sees exactly the pre-#8058 shape. It must also stay off disk entirely.
#[test]
fn capacity_reports_no_class_state_when_there_is_none() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!((capacity.healthy, capacity.cooldown), (1, 1));
    assert!(capacity.healthy_by_class.is_empty());
    let json = serde_json::to_value(&capacity).unwrap();
    assert!(
        json.get("healthy_by_class").is_none(),
        "an empty map must not appear on the wire: {json}"
    );
}

/// The observability gap this phase closes: one class held on most accounts
/// reads as a nearly-dead provider account-wide, while the per-class count
/// shows the capacity that is actually still there.
#[test]
fn capacity_reports_healthy_counts_per_class() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
        descriptor(AccountProvider::Codex, "c"),
    ];
    for account in &accounts[..2] {
        record_terminal_for_model_at(
            tmp.path(),
            &account.id,
            TerminalClassification::ModelCreditsExhausted,
            Some("gpt-5-codex"),
            "adapter_v1",
            100,
        )
        .unwrap();
    }
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    // Account-wide: a live class hold still blocks the class-less question.
    assert_eq!((capacity.healthy, capacity.cooldown), (1, 2));
    // Per class: the hold really is on two accounts, and nothing else is.
    assert_eq!(capacity.healthy_by_class, BTreeMap::from([("gpt-5-codex".to_string(), 1)]));
    assert_eq!(
        serde_json::to_value(&capacity).unwrap()["healthy_by_class"],
        serde_json::json!({"gpt-5-codex": 1})
    );
}

/// Two classes held on disjoint accounts are counted independently — neither
/// class inherits the other's hold.
#[test]
fn capacity_counts_each_class_independently() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
        descriptor(AccountProvider::Codex, "c"),
    ];
    for (account, model) in accounts.iter().zip(["gpt-5-codex", "gpt-5-mini"]) {
        record_terminal_for_model_at(
            tmp.path(),
            &account.id,
            TerminalClassification::ModelCreditsExhausted,
            Some(model),
            "adapter_v1",
            100,
        )
        .unwrap();
    }
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!(capacity.healthy, 1);
    assert_eq!(
        capacity.healthy_by_class,
        BTreeMap::from([
            ("gpt-5-codex".to_string(), 2),
            ("gpt-5-mini".to_string(), 2)
        ])
    );
}

/// Narrower, never wider: no class count may fall below the account-wide
/// `healthy`, because every account-wide hold is checked first and
/// identically. An account-wide exhaustion and a sticky reauth are invisible
/// to the class filter and stay counted out for every class.
#[test]
fn no_class_count_is_ever_below_the_account_wide_healthy_count() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
        descriptor(AccountProvider::Codex, "c"),
        descriptor(AccountProvider::Codex, "d"),
    ];
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::TokenExpired,
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_at(
        tmp.path(),
        &accounts[1].id,
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[2].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!(capacity.healthy, 1);
    for (class, count) in &capacity.healthy_by_class {
        assert!(
            *count >= capacity.healthy,
            "class {class} reported {count}, below the account-wide {}",
            capacity.healthy
        );
    }
    // Only `d` is free of every hold; `c` adds itself back for no class but
    // its own, so `gpt-5-codex` stays at 1 while nothing else is reported.
    assert_eq!(capacity.healthy_by_class, BTreeMap::from([("gpt-5-codex".to_string(), 1)]));
}

/// An EXPIRED class hold names no live class, so it is not reported at all —
/// a class whose count would simply equal `healthy` tells an operator
/// nothing, and reporting it would make the map look permanently populated.
#[test]
fn an_expired_class_hold_is_not_reported() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![descriptor(AccountProvider::Codex, "a")];
    record_terminal_for_model_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    let capacity = provider_capacity_at(
        tmp.path(),
        AccountProvider::Codex,
        &accounts,
        100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS,
    )
    .unwrap();
    assert_eq!(capacity.healthy, 1);
    assert!(capacity.healthy_by_class.is_empty());
}

// ---------------------------------------------------------------------------
// #8539: the provider-reported reset horizon
// ---------------------------------------------------------------------------

/// The headline case: a refusal that named its own reset replaces the blind
/// `now + LOOM_CODEX_EXHAUSTED_COOLDOWN_SECS` guess on both exhaustion arms.
#[test]
fn a_supplied_horizon_becomes_the_exhaustion_deadline() {
    for classification in [
        TerminalClassification::TokenExhausted,
        TerminalClassification::ModelCreditsExhausted,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let id = descriptor(AccountProvider::Codex, "a").id;
        let horizon = 100 + 30 * 60 * 60;
        record_terminal_for_class_with_reset_at(
            tmp.path(),
            &id,
            classification,
            None,
            Some(horizon),
            "spawn-codex:v1",
            100,
        )
        .unwrap();
        let entry = account_health(tmp.path(), &id).unwrap().unwrap();
        assert_eq!(
            entry.cooldown_until,
            Some(horizon),
            "{classification:?} must honour the provider's own horizon"
        );
        // …and that horizon really does outlive the default cooldown, so this
        // assertion cannot pass by coincidence.
        assert!(horizon > 100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS);
    }
}

/// `None` reproduces the pre-#8539 behaviour byte for byte: the horizon is an
/// enrichment, never a prerequisite.
#[test]
fn no_horizon_falls_back_to_the_configured_cooldown() {
    let tmp = tempfile::tempdir().unwrap();
    let id = descriptor(AccountProvider::Codex, "a").id;
    record_terminal_for_class_with_reset_at(
        tmp.path(),
        &id,
        TerminalClassification::TokenExhausted,
        None,
        None,
        "spawn-codex:v1",
        100,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &id).unwrap().unwrap();
    assert_eq!(entry.cooldown_until, Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS));
}

/// The horizon is untrusted input — it comes out of a log an agent also wrote
/// into. An implausible one falls back rather than arming a hold nothing will
/// notice, and one already in the past never shortens the hold to zero.
#[test]
fn an_implausible_or_past_horizon_falls_back_instead_of_being_trusted() {
    for (label, reset_at) in [
        ("already past", Some(99_u64)),
        ("exactly now", Some(100)),
        (
            "beyond the plausible ceiling",
            Some(100 + MAX_EXHAUSTION_RESET_HORIZON_SECS + 1),
        ),
        ("a misparsed far-future year", Some(u64::MAX)),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let id = descriptor(AccountProvider::Codex, "a").id;
        record_terminal_for_class_with_reset_at(
            tmp.path(),
            &id,
            TerminalClassification::TokenExhausted,
            None,
            reset_at,
            "spawn-codex:v1",
            100,
        )
        .unwrap();
        let entry = account_health(tmp.path(), &id).unwrap().unwrap();
        assert_eq!(
            entry.cooldown_until,
            Some(100 + DEFAULT_EXHAUSTED_COOLDOWN_SECS),
            "{label}: must fall back to the configured cooldown"
        );
    }
    // The boundary itself is accepted — the ceiling is inclusive.
    let tmp = tempfile::tempdir().unwrap();
    let id = descriptor(AccountProvider::Codex, "a").id;
    let ceiling = 100 + MAX_EXHAUSTION_RESET_HORIZON_SECS;
    record_terminal_for_class_with_reset_at(
        tmp.path(),
        &id,
        TerminalClassification::TokenExhausted,
        None,
        Some(ceiling),
        "spawn-codex:v1",
        100,
    )
    .unwrap();
    assert_eq!(
        account_health(tmp.path(), &id)
            .unwrap()
            .unwrap()
            .cooldown_until,
        Some(ceiling)
    );
}

/// Only the exhaustion arms consult it. Every other deadline here is Loom's
/// own retry policy, not a prediction of a provider-side event, so a horizon
/// read out of a refusal has no authority over it.
#[test]
fn a_horizon_never_reshapes_a_non_exhaustion_hold() {
    for classification in [
        TerminalClassification::Recoverable,
        TerminalClassification::SessionLimit,
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let id = descriptor(AccountProvider::Codex, "a").id;
        let with_horizon = 100 + 30 * 60 * 60;
        record_terminal_for_class_with_reset_at(
            tmp.path(),
            &id,
            classification,
            None,
            Some(with_horizon),
            "spawn-codex:v1",
            100,
        )
        .unwrap();
        let held = account_health(tmp.path(), &id)
            .unwrap()
            .unwrap()
            .cooldown_until;

        let baseline_dir = tempfile::tempdir().unwrap();
        record_terminal_for_class_at(
            baseline_dir.path(),
            &id,
            classification,
            None,
            "spawn-codex:v1",
            100,
        )
        .unwrap();
        let baseline = account_health(baseline_dir.path(), &id)
            .unwrap()
            .unwrap()
            .cooldown_until;
        assert_eq!(held, baseline, "{classification:?} must ignore the horizon");
        assert_ne!(held, Some(with_horizon));
    }
}

/// #8058 Phase 2's class scoping still holds when a horizon is supplied: the
/// hold the horizon shapes is the **class** hold, and the account stays
/// selectable for every other class.
#[test]
fn a_horizon_shapes_the_class_hold_a_named_model_records() {
    let tmp = tempfile::tempdir().unwrap();
    let id = descriptor(AccountProvider::Codex, "a").id;
    let horizon = 100 + 30 * 60 * 60;
    record_terminal_for_class_with_reset_at(
        tmp.path(),
        &id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        Some(horizon),
        "spawn-codex:v1",
        100,
    )
    .unwrap();
    let entry = account_health(tmp.path(), &id).unwrap().unwrap();
    assert_eq!(entry.cooldown_until, None, "the account itself is not held");
    assert_eq!(entry.class_cooldowns.get("gpt-5-codex"), Some(&horizon));
}

// ============================================================================
// Weekly-headroom preference among equally healthy Codex seats (#14585)
// ============================================================================

/// One enabled Codex seat whose profile directory carries a single rollout
/// line. `weekly` / `short` are `used_percent` values; `None` omits that window.
/// Both windows reset in the future relative to the file's own mtime.
fn seat_with_usage(
    root: &Path,
    name: &str,
    short: Option<f64>,
    weekly: Option<f64>,
) -> AccountDescriptor {
    let mut account = descriptor(AccountProvider::Codex, name);
    let profile = root.join("profiles").join(name);
    let sessions = profile.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let window = |used: f64, minutes: u64, resets: u64| {
        serde_json::json!({
            "used_percent": used,
            "window_minutes": minutes,
            "resets_in_seconds": resets,
        })
    };
    let line = serde_json::json!({
        "rate_limits": {
            "primary": short.map(|used| window(used, 300, 3_600)),
            "secondary": weekly.map(|used| window(used, 10_080, 400_000)),
        }
    });
    fs::write(sessions.join("rollout-fixture.jsonl"), format!("{line}\n")).unwrap();
    account.credential_reference = profile;
    account
}

fn pick(root: &Path, accounts: &[AccountDescriptor]) -> String {
    select_healthy_at(root, AccountProvider::Codex, accounts, now_epoch_secs())
        .unwrap()
        .id
        .name
}

#[test]
fn lowest_weekly_utilization_wins_at_every_cursor_position() {
    let tmp = tempfile::tempdir().unwrap();
    // The 5h readings point the opposite way, so a window mix-up flips the result.
    let accounts = vec![
        seat_with_usage(tmp.path(), "a", Some(1.0), Some(49.0)),
        seat_with_usage(tmp.path(), "b", Some(90.0), Some(5.0)),
    ];
    for _ in 0..4 {
        assert_eq!(pick(tmp.path(), &accounts), "b");
    }
    // Inverting only the weekly readings inverts the choice.
    let flipped = vec![
        seat_with_usage(tmp.path(), "a", Some(1.0), Some(5.0)),
        seat_with_usage(tmp.path(), "b", Some(90.0), Some(49.0)),
    ];
    for _ in 0..4 {
        assert_eq!(pick(tmp.path(), &flipped), "a");
    }
}

#[test]
fn seats_tied_for_lowest_weekly_utilization_still_rotate() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        seat_with_usage(tmp.path(), "a", None, Some(5.0)),
        seat_with_usage(tmp.path(), "b", None, Some(49.0)),
        seat_with_usage(tmp.path(), "c", None, Some(5.0)),
    ];
    let picks: HashSet<String> = (0..4).map(|_| pick(tmp.path(), &accounts)).collect();
    assert_eq!(picks, HashSet::from(["a".to_string(), "c".to_string()]));
}

#[test]
fn a_measured_zero_is_a_reading_not_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        seat_with_usage(tmp.path(), "a", None, Some(7.0)),
        seat_with_usage(tmp.path(), "b", None, Some(0.0)),
    ];
    for _ in 0..3 {
        assert_eq!(pick(tmp.path(), &accounts), "b");
    }
}

#[test]
fn missing_partial_or_five_hour_only_evidence_keeps_round_robin() {
    let tmp = tempfile::tempdir().unwrap();
    let rotates = |accounts: &[AccountDescriptor]| {
        let picks: HashSet<String> = (0..4).map(|_| pick(tmp.path(), accounts)).collect();
        picks.len() == accounts.len()
    };
    // Nothing measured at all (no profile on disk).
    assert!(rotates(&[
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ]));
    // Partial: one measured seat must not be favoured over an unknown one.
    assert!(rotates(&[
        seat_with_usage(tmp.path(), "c", None, Some(49.0)),
        descriptor(AccountProvider::Codex, "d"),
    ]));
    assert!(rotates(&[
        seat_with_usage(tmp.path(), "e", None, Some(5.0)),
        descriptor(AccountProvider::Codex, "f"),
    ]));
    // 5h-only readings say nothing about the weekly window.
    assert!(rotates(&[
        seat_with_usage(tmp.path(), "g", Some(80.0), None),
        seat_with_usage(tmp.path(), "h", Some(2.0), None),
    ]));
}

#[test]
fn a_rolled_over_weekly_window_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        seat_with_usage(tmp.path(), "a", None, Some(49.0)),
        seat_with_usage(tmp.path(), "b", None, Some(5.0)),
    ];
    // Both windows reset 400_000s after the snapshot; ask well past that.
    let later = now_epoch_secs() + 1_000_000;
    let picks: HashSet<String> = (0..4)
        .map(|_| {
            select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, later)
                .unwrap()
                .id
                .name
        })
        .collect();
    assert_eq!(picks.len(), 2, "expired readings must not rank: {picks:?}");
}

#[test]
fn weekly_preference_does_not_borrow_shared_rollouts() {
    let tmp = tempfile::tempdir().unwrap();
    let a = seat_with_usage(tmp.path(), "a", None, None);
    let b = seat_with_usage(tmp.path(), "b", None, Some(5.0));
    let a_sessions = a.credential_reference.join("sessions");
    fs::remove_dir_all(&a_sessions).unwrap();
    let accounts = [a, b];
    for shared_usage in [1.0, 80.0] {
        let default = seat_with_usage(tmp.path(), "default", None, Some(shared_usage));
        let shared = default.credential_reference.join("sessions");
        std::os::unix::fs::symlink(&shared, &a_sessions).unwrap();
        let picks: HashSet<_> = (0..4).map(|_| pick(tmp.path(), &accounts)).collect();
        assert_eq!(picks, HashSet::from(["a".to_owned(), "b".to_owned()]));
        fs::remove_file(&a_sessions).unwrap();

        fs::create_dir(&a_sessions).unwrap();
        std::os::unix::fs::symlink(
            shared.join("rollout-fixture.jsonl"),
            a_sessions.join("rollout-linked.jsonl"),
        )
        .unwrap();
        let picks: HashSet<_> = (0..4).map(|_| pick(tmp.path(), &accounts)).collect();
        assert_eq!(picks, HashSet::from(["a".to_owned(), "b".to_owned()]));
        fs::remove_dir_all(&a_sessions).unwrap();
    }
}

#[test]
fn weekly_preference_ages_out_a_malformed_reset_using_its_window() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = [
        seat_with_usage(tmp.path(), "a", None, None),
        seat_with_usage(tmp.path(), "b", None, Some(5.0)),
    ];
    let now = now_epoch_secs();
    let week = 7 * 24 * 60 * 60;
    let rollout = accounts[0]
        .credential_reference
        .join("sessions/rollout-fixture.jsonl");
    for (age, expected) in [
        (week - 1, HashSet::from(["a".to_owned()])),
        (week, HashSet::from(["a".to_owned(), "b".to_owned()])),
        (week + 86_400, HashSet::from(["a".to_owned(), "b".to_owned()])),
    ] {
        let observed_at = chrono::DateTime::from_timestamp((now - age) as i64, 0).unwrap();
        let line = serde_json::json!({
            "timestamp": observed_at.to_rfc3339(),
            "rate_limits": {
                "secondary": {
                    "used_percent": 1.0,
                    "window_minutes": 10_080,
                    "resets_at": "malformed"
                },
            },
        });
        fs::write(&rollout, format!("{line}\n")).unwrap();
        let picks: HashSet<_> = (0..4)
            .map(|_| {
                select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, now)
                    .unwrap()
                    .id
                    .name
            })
            .collect();
        assert_eq!(picks, expected, "snapshot age {age}");
    }
}

#[test]
fn weekly_preference_stays_inside_the_fewest_transient_failures_group() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        seat_with_usage(tmp.path(), "a", None, Some(1.0)),
        seat_with_usage(tmp.path(), "b", None, Some(60.0)),
    ];
    // `a` has the most headroom but a recent transient failure: health first.
    record_terminal_at(
        tmp.path(),
        &accounts[0].id,
        TerminalClassification::Recoverable,
        "adapter_v1",
        now_epoch_secs(),
    )
    .unwrap();
    assert_eq!(pick(tmp.path(), &accounts), "b");
}

#[test]
fn weekly_preference_never_resurrects_an_ineligible_seat() {
    let tmp = tempfile::tempdir().unwrap();
    let mut disabled = seat_with_usage(tmp.path(), "a", None, Some(0.0));
    disabled.enabled = false;
    let accounts = vec![
        disabled,
        seat_with_usage(tmp.path(), "b", None, Some(80.0)),
        seat_with_usage(tmp.path(), "c", None, Some(90.0)),
    ];
    for _ in 0..3 {
        assert_eq!(pick(tmp.path(), &accounts), "b");
    }
}

// -----------------------------------------------------------------
// #15091: the fleet heartbeat's Codex holds — publish read + merge
// -----------------------------------------------------------------

fn remote_hold(name: &str, reason: &str, until: u64) -> CodexAccountHold {
    CodexAccountHold {
        name: name.to_string(),
        reason: reason.to_string(),
        cooldown_until: until,
        learned_at: None,
    }
}

/// A local plan-exhaustion hold with a provider-named reset horizon, the
/// exact artifact a worker's failed dispatch leaves behind (#8443/#8539).
fn local_exhaustion(root: &Path, name: &str, until: u64) {
    record_terminal_for_class_with_reset_at(
        root,
        &AccountId {
            provider: AccountProvider::Codex,
            name: name.to_string(),
        },
        TerminalClassification::TokenExhausted,
        None,
        Some(until),
        "adapter_v1",
        100,
    )
    .unwrap();
}

fn held_entry(root: &Path, name: &str) -> Option<AccountHealth> {
    account_health(
        root,
        &AccountId {
            provider: AccountProvider::Codex,
            name: name.to_string(),
        },
    )
    .unwrap()
}

/// A remote hold lands on a fresh host, blocks selection and capacity until
/// its deadline, and is labelled with the fleet provenance (#15091 AC1).
#[test]
fn a_remote_hold_blocks_selection_until_its_deadline() {
    let tmp = tempfile::tempdir().unwrap();
    let accounts = vec![
        descriptor(AccountProvider::Codex, "a"),
        descriptor(AccountProvider::Codex, "b"),
    ];
    assert_eq!(
        merge_codex_holds(tmp.path(), &[remote_hold("a", "plan_exhausted", 1000)], 100),
        1,
        "the hold applies to an account with no local record"
    );
    let entry = held_entry(tmp.path(), "a").unwrap();
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.cooldown_until, Some(1000));
    assert_eq!(entry.signal_provenance, FLEET_HOLD_PROVENANCE);

    // The same eligibility question `codex_pool_state` and the selector ask.
    assert_eq!(
        select_healthy_at(tmp.path(), AccountProvider::Codex, &accounts, 101)
            .unwrap()
            .id
            .name,
        "b",
        "the held account is not spawnable before the deadline"
    );
    let capacity =
        provider_capacity_at(tmp.path(), AccountProvider::Codex, &accounts, 101).unwrap();
    assert_eq!(capacity.cooldown, 1);
    let recovered = held_entry(tmp.path(), "a").unwrap();
    assert!(recovered.is_eligible_at(1000), "the hold ends at its deadline");
}

/// The merge never narrows: a remote deadline beats the local one only when
/// strictly later; equal and earlier remotes change nothing (#15091 AC2).
#[test]
fn a_local_release_outranks_an_older_remote_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "a".to_string(),
    };
    local_exhaustion(tmp.path(), "a", 5000);
    // A measured release on newer evidence (observed 150 > updated 100).
    let effect = record_availability_at(
        tmp.path(),
        &id,
        AvailabilityOutcome::Available { observed_at: 150 },
        "probe",
        160,
    )
    .unwrap();
    assert_eq!(effect, AvailabilityEffect::ClearedExhaustionHold);

    // A hold learned *before* the release is not re-imposed.
    let mut stale = remote_hold("a", "plan_exhausted", 9000);
    stale.learned_at = Some(120);
    assert_eq!(merge_codex_holds(tmp.path(), &[stale], 200), 0);
    assert_eq!(held_entry(tmp.path(), "a").unwrap().cooldown_until, None, "still released");

    // A hold learned after the release applies, stamped with its learned-at
    // time (not the merge time), so a later reading can still release it.
    let mut fresh = remote_hold("a", "plan_exhausted", 9000);
    fresh.learned_at = Some(170);
    assert_eq!(merge_codex_holds(tmp.path(), &[fresh], 200), 1);
    let entry = held_entry(tmp.path(), "a").unwrap();
    assert_eq!(entry.updated_at, 170);
    let effect = record_availability_at(
        tmp.path(),
        &id,
        AvailabilityOutcome::Available { observed_at: 180 },
        "probe",
        210,
    )
    .unwrap();
    assert_eq!(effect, AvailabilityEffect::ClearedExhaustionHold);
}

fn codex_id(name: &str) -> AccountId {
    AccountId {
        provider: AccountProvider::Codex,
        name: name.to_string(),
    }
}

fn learned_hold(name: &str, until: u64, learned: u64) -> CodexAccountHold {
    let mut hold = remote_hold(name, "plan_exhausted", until);
    hold.learned_at = Some(learned);
    hold
}

/// A login probe is not quota evidence: it must not suppress a fleet hold
/// learned before it (#15091 review).
#[test]
fn a_login_probe_does_not_suppress_a_fleet_hold() {
    let tmp = tempfile::tempdir().unwrap();
    record_probe_at(tmp.path(), &codex_id("a"), ProbeOutcome::LoggedIn, "probe", 200).unwrap();
    assert_eq!(merge_codex_holds(tmp.path(), &[learned_hold("a", 1000, 100)], 201), 1);
    assert_eq!(held_entry(tmp.path(), "a").unwrap().cooldown_until, Some(1000));
}

#[test]
fn relaying_a_released_hold_preserves_its_original_observation() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let id = codex_id("a");
    local_exhaustion(a.path(), "a", 1000);
    let original = live_codex_holds(a.path(), 101).unwrap();
    record_availability_at(
        a.path(),
        &id,
        AvailabilityOutcome::Available { observed_at: 150 },
        "quota",
        151,
    )
    .unwrap();

    record_probe_at(b.path(), &id, ProbeOutcome::LoggedIn, "login", 200).unwrap();
    assert_eq!(merge_codex_holds(b.path(), &original, 201), 1);
    // An independent model-class failure also leaves the plan evidence alone.
    record_terminal_for_class_at(
        b.path(),
        &id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt"),
        "dispatch",
        202,
    )
    .unwrap();
    let echo = live_codex_holds(b.path(), 203).unwrap();
    assert_eq!(echo, original);
    assert_eq!(merge_codex_holds(a.path(), &echo, 204), 0);
    assert!(held_entry(a.path(), "a").unwrap().is_eligible_at(204));
}

#[test]
fn successful_dispatch_supersedes_older_timed_holds() {
    let root = tempfile::tempdir().unwrap();
    let id = codex_id("a");
    local_exhaustion(root.path(), "a", 1000);
    record_terminal_at(root.path(), &id, TerminalClassification::Success, "dispatch", 180).unwrap();

    assert_eq!(merge_codex_holds(root.path(), &[learned_hold("a", 1000, 100)], 201), 0);
    assert!(held_entry(root.path(), "a").unwrap().is_eligible_at(201));
    assert_eq!(merge_codex_holds(root.path(), &[learned_hold("a", 1000, 190)], 201), 1);
    assert!(!held_entry(root.path(), "a").unwrap().is_eligible_at(201));
}

#[test]
fn plan_recovery_does_not_release_an_independent_session_limit() {
    let root = tempfile::tempdir().unwrap();
    let id = codex_id("a");
    local_exhaustion(root.path(), "a", 1000);
    let available = AvailabilityOutcome::Available { observed_at: 150 };
    record_availability_at(root.path(), &id, available, "quota", 150).unwrap();
    let session = CodexAccountHold {
        reason: "session_limit".into(),
        ..learned_hold("a", 170, 140)
    };
    assert_eq!(merge_codex_holds(root.path(), &[session], 151), 1);
    // Neither a replay of the old longer plan hold nor another headroom
    // reading can erase the independent session limit.
    assert_eq!(merge_codex_holds(root.path(), &[learned_hold("a", 1000, 100)], 152), 0);
    record_availability_at(root.path(), &id, available, "quota", 153).unwrap();
    let entry = held_entry(root.path(), "a").unwrap();
    assert_eq!(entry.reason, HealthReason::SessionLimit);
    assert!(!entry.is_eligible_at(153));
    assert!(entry.is_eligible_at(170));
}

#[test]
fn replaying_an_identical_hold_preserves_the_health_file() {
    let root = tempfile::tempdir().unwrap();
    let holds = [learned_hold("a", 1000, 100)];
    assert_eq!(merge_codex_holds(root.path(), &holds, 101), 1);
    let path = state_path(root.path());
    let file = fs::File::options().write(true).open(&path).unwrap();
    file.set_modified(std::time::UNIX_EPOCH).unwrap();
    let modified = file.metadata().unwrap().modified().unwrap();
    assert_eq!(merge_codex_holds(root.path(), &holds, 102), 0);
    assert_eq!(fs::metadata(path).unwrap().modified().unwrap(), modified);
}

/// A headroom reading observed *before* the hold was learned, however late
/// it was processed, establishes nothing about the hold.
#[test]
fn a_late_processed_old_reading_does_not_suppress_a_fleet_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let effect = record_availability_at(
        tmp.path(),
        &codex_id("a"),
        AvailabilityOutcome::Available { observed_at: 50 },
        "probe",
        200,
    )
    .unwrap();
    assert_eq!(effect, AvailabilityEffect::Unchanged);
    assert_eq!(merge_codex_holds(tmp.path(), &[learned_hold("a", 1000, 100)], 201), 1);
    assert_eq!(held_entry(tmp.path(), "a").unwrap().cooldown_until, Some(1000));
}

/// Reports with the same deadline keep the freshest exhaustion evidence, so
/// a reading between the two learn times cannot release the hold.
#[test]
fn equal_deadline_reports_keep_the_freshest_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    merge_codex_holds(tmp.path(), &[learned_hold("a", 1000, 100)], 150);
    assert_eq!(merge_codex_holds(tmp.path(), &[learned_hold("a", 1000, 300)], 350), 1);
    let entry = held_entry(tmp.path(), "a").unwrap();
    assert_eq!((entry.cooldown_until, entry.updated_at), (Some(1000), 300));
    // An older equal report changes nothing.
    assert_eq!(merge_codex_holds(tmp.path(), &[learned_hold("a", 1000, 200)], 350), 0);
    let effect = record_availability_at(
        tmp.path(),
        &codex_id("a"),
        AvailabilityOutcome::Available { observed_at: 200 },
        "probe",
        360,
    )
    .unwrap();
    assert_eq!(
        effect,
        AvailabilityEffect::Unchanged,
        "a reading from 200 cannot release a hold confirmed at 300"
    );
    assert_eq!(held_entry(tmp.path(), "a").unwrap().cooldown_until, Some(1000));
}

/// The relay rule: later deadline wins, equal deadline keeps the freshest
/// `learned_at`, and the winner always carries the freshest of both.
#[test]
fn relayed_holds_keep_the_freshest_evidence_on_equal_deadlines() {
    let older = learned_hold("a", 1000, 100);
    let newer = learned_hold("a", 1000, 300);
    assert_eq!(older.clone().relayed(newer.clone()).learned_at, Some(300));
    assert_eq!(newer.clone().relayed(older.clone()).learned_at, Some(300));
    let longer = learned_hold("a", 2000, 100);
    let merged = newer.relayed(longer);
    assert_eq!((merged.cooldown_until, merged.learned_at), (2000, Some(300)));
    // A different kind of failure cannot confirm plan exhaustion.
    let session = CodexAccountHold {
        reason: "session_limit".into(),
        ..learned_hold("a", 1000, 400)
    };
    let plan = learned_hold("a", 2000, 100);
    assert_eq!(plan.clone().relayed(session.clone()), plan);
    assert_eq!(session.relayed(plan.clone()), plan);
    // A peer without `learned_at` never erases one that has it.
    assert_eq!(
        older
            .relayed(remote_hold("a", "plan_exhausted", 1000))
            .learned_at,
        Some(100)
    );
}

/// Two registered roots learning the same deadline at different times: the
/// union publishes the later `learned_at` whichever root is read first.
#[test]
fn the_root_union_publishes_the_freshest_learned_at() {
    let old_root = tempfile::tempdir().unwrap();
    let new_root = tempfile::tempdir().unwrap();
    for (root, learned) in [(&old_root, 100), (&new_root, 300)] {
        merge_codex_holds(root.path(), &[learned_hold("a", 1000, learned)], 400);
    }
    for roots in [
        [old_root.path().to_path_buf(), new_root.path().to_path_buf()],
        [new_root.path().to_path_buf(), old_root.path().to_path_buf()],
    ] {
        let holds = live_codex_holds_across(&roots, 400).unwrap();
        assert_eq!((holds[0].cooldown_until, holds[0].learned_at), (1000, Some(300)));
    }
}

#[test]
fn the_merge_never_shortens_a_local_hold() {
    let tmp = tempfile::tempdir().unwrap();
    local_exhaustion(tmp.path(), "a", 5000);
    assert_eq!(
        merge_codex_holds(tmp.path(), &[remote_hold("a", "plan_exhausted", 3000)], 200),
        0,
        "an earlier remote hold is ignored"
    );
    assert_eq!(
        merge_codex_holds(tmp.path(), &[remote_hold("a", "transient_failure", 5000)], 200),
        0,
        "an equal remote hold is ignored"
    );
    let entry = held_entry(tmp.path(), "a").unwrap();
    assert_eq!(entry.cooldown_until, Some(5000));
    assert_eq!(entry.signal_provenance, "adapter_v1", "nothing was rewritten");

    // Strictly later wins wholesale: the deadline extends and the record
    // names the fact that carries the later deadline.
    assert_eq!(
        merge_codex_holds(tmp.path(), &[remote_hold("a", "plan_exhausted", 9000)], 200),
        1
    );
    let entry = held_entry(tmp.path(), "a").unwrap();
    assert_eq!(entry.cooldown_until, Some(9000));
    assert_eq!(entry.reason, HealthReason::PlanExhausted);
    assert_eq!(entry.signal_provenance, FLEET_HOLD_PROVENANCE);
}

/// A sticky reauth hold is never touched by a remote timed hold, however
/// far out its deadline is (#15091 AC2).
#[test]
fn a_reauth_hold_is_untouched_by_remote_holds() {
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "a".to_string(),
    };
    record_terminal_at(tmp.path(), &id, TerminalClassification::TokenExpired, "adapter_v1", 100)
        .unwrap();
    assert_eq!(
        merge_codex_holds(tmp.path(), &[remote_hold("a", "plan_exhausted", 9000)], 200),
        0
    );
    let entry = held_entry(tmp.path(), "a").unwrap();
    assert_eq!(entry.reason, HealthReason::ReauthRequired);
    assert_eq!(entry.cooldown_until, None);
}

/// Expired, implausibly-far, unknown-reason and malformed holds are ignored,
/// never an error — and a live hold beside them still lands (#15091 AC2/AC3).
#[test]
fn invalid_holds_are_ignored_not_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let now = 1000;
    let long = "x".repeat(MAX_FLEET_HOLD_NAME_LEN + 1);
    let holds = vec![
        remote_hold("expired", "plan_exhausted", now),
        remote_hold("past", "plan_exhausted", now - 1),
        remote_hold("far", "plan_exhausted", now + MAX_EXHAUSTION_RESET_HORIZON_SECS + 1),
        remote_hold("unknown", "quota_nuked", now + 500),
        remote_hold("healthy", "healthy", now + 500),
        remote_hold("reauth", "reauth_required", now + 500),
        remote_hold("", "plan_exhausted", now + 500),
        remote_hold(&long, "plan_exhausted", now + 500),
        remote_hold("good", "session_limit", now + 500),
    ];
    assert_eq!(merge_codex_holds(tmp.path(), &holds, now), 1);
    for name in [
        "expired", "past", "far", "unknown", "healthy", "reauth", "", &long,
    ] {
        assert!(held_entry(tmp.path(), name).is_none(), "{name:?} must not land");
    }
    let entry = held_entry(tmp.path(), "good").unwrap();
    assert_eq!(entry.reason, HealthReason::SessionLimit);
    assert_eq!(entry.cooldown_until, Some(now + 500));
}

/// The merge touches only the account-wide hold: class-scoped cooldowns,
/// transient counters and success stamps survive it (#15091 AC2).
#[test]
fn the_merge_leaves_class_cooldowns_and_counters_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let id = AccountId {
        provider: AccountProvider::Codex,
        name: "a".to_string(),
    };
    record_terminal_for_model_at(
        tmp.path(),
        &id,
        TerminalClassification::ModelCreditsExhausted,
        Some("gpt-5-codex"),
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_at(tmp.path(), &id, TerminalClassification::Recoverable, "adapter_v1", 110)
        .unwrap();
    let before = held_entry(tmp.path(), "a").unwrap();
    assert_eq!(before.class_cooldowns.len(), 1);
    let class_deadline = *before.class_cooldowns.values().next().unwrap();
    assert_eq!(before.consecutive_transient_failures, 1);

    // An account-wide remote hold strictly later than the 60 s transient
    // backoff applies without narrowing the class hold into anything.
    assert_eq!(
        merge_codex_holds(tmp.path(), &[remote_hold("a", "plan_exhausted", 5000)], 120),
        1
    );
    let after = held_entry(tmp.path(), "a").unwrap();
    assert_eq!(after.cooldown_until, Some(5000));
    assert_eq!(after.class_cooldowns.len(), 1);
    assert_eq!(*after.class_cooldowns.values().next().unwrap(), class_deadline);
    assert_eq!(after.consecutive_transient_failures, 1);
}

/// [`live_codex_holds`] publishes exactly the live timed Codex holds — never
/// reauth, never expired, never another provider — and nothing at all when
/// the state is unreadable or quiet (#15091 AC1/AC4).
#[test]
fn live_codex_holds_publishes_only_live_timed_codex_holds() {
    let tmp = tempfile::tempdir().unwrap();
    assert_eq!(live_codex_holds(tmp.path(), 100), None, "no state file");

    local_exhaustion(tmp.path(), "a", 5000);
    record_terminal_at(
        tmp.path(),
        &AccountId {
            provider: AccountProvider::Codex,
            name: "reauth".to_string(),
        },
        TerminalClassification::TokenExpired,
        "adapter_v1",
        100,
    )
    .unwrap();
    record_terminal_at(
        tmp.path(),
        &AccountId {
            provider: AccountProvider::Claude,
            name: "claude-1".to_string(),
        },
        TerminalClassification::TokenExhausted,
        "adapter_v1",
        100,
    )
    .unwrap();
    assert_eq!(
        live_codex_holds(tmp.path(), 101),
        Some(vec![CodexAccountHold {
            learned_at: Some(100),
            ..remote_hold("a", "plan_exhausted", 5000)
        }])
    );
    assert_eq!(
        live_codex_holds(tmp.path(), 5000),
        None,
        "a hold is live only strictly before its deadline"
    );
}

/// [`CodexAccountHold::is_carriable`] is the one gate both the captain's
/// ingest and the worker's merge apply (#15091).
#[test]
fn is_carriable_gates_name_reason_and_deadline() {
    let now = 1000;
    assert!(remote_hold("a", "plan_exhausted", now + 1).is_carriable(now));
    assert!(remote_hold("a", "transient_failure", now + 1).is_carriable(now));
    assert!(remote_hold("a", "model_credits_exhausted", now + 1).is_carriable(now));
    assert!(!remote_hold("a", "plan_exhausted", now).is_carriable(now));
    assert!(!remote_hold("a", "plan_exhausted", now - 1).is_carriable(now));
    assert!(!remote_hold("a", "bogus", now + 1).is_carriable(now));
    assert!(!remote_hold("a", "reauth_required", now + 1).is_carriable(now));
    assert!(!remote_hold("", "plan_exhausted", now + 1).is_carriable(now));
    assert!(!remote_hold(&"x".repeat(129), "plan_exhausted", now + 1).is_carriable(now));
    assert!(!remote_hold("a", "plan_exhausted", now + MAX_EXHAUSTION_RESET_HORIZON_SECS + 1)
        .is_carriable(now));
}
