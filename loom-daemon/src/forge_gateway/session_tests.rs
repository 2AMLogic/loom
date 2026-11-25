#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::time::SystemTime;

use serde_json::json;

use super::*;
use crate::forge_gateway::publish::GH_MARKER;
use crate::forge_gateway::{AwsAccess, GhVersion, KeyRef};

const KEY: &str = "gp_0123456789abcdef0123456789abcdef";
const OTHER_KEY: &str = "gp_fedcba0123456789abcdef0123456789";

/// Env-var tests move process-global state ([`SESSION_ROOT_ENV`]): run one
/// at a time, like the publisher tests above.
fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cfg(owners: &[(&str, &str)]) -> Value {
    let refs: serde_json::Map<String, Value> = owners
        .iter()
        .map(|(o, r)| ((*o).to_string(), json!(r)))
        .collect();
    json!({"forge": {"gateway": {"origin": "https://gw.example.com", "keyRefs": refs}}})
}

/// A fake outside world for the session pass: a fixed gh, keys by file path.
struct Fake {
    gh: Option<GhVersion>,
    keys: BTreeMap<String, Result<String, String>>,
}

impl Fake {
    fn new(gh: Option<GhVersion>) -> Self {
        Self {
            gh,
            keys: BTreeMap::new(),
        }
    }
    fn key(mut self, r: &str, k: Result<&str, &str>) -> Self {
        self.keys
            .insert(r.to_string(), k.map(str::to_string).map_err(str::to_string));
        self
    }
}

impl Deps for Fake {
    fn gh(&self, _: &Path) -> Option<(PathBuf, GhVersion)> {
        self.gh.map(|v| (PathBuf::from("/opt/gh/bin/gh"), v))
    }
    fn read_key(&self, r: &KeyRef, _aws: &AwsAccess<'_>) -> Result<String, String> {
        let KeyRef::File(p) = r else {
            return Err("ssm not faked".into());
        };
        self.keys
            .get(&p.to_string_lossy().into_owned())
            .cloned()
            .unwrap_or_else(|| Err("no such fake key".into()))
    }
}

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

#[test]
fn refresh_publishes_each_owner_privately_and_picks_up_a_rotation() {
    let tmp = tempfile::tempdir().unwrap();
    let base = session_base_at(tmp.path());
    let host = cfg(&[("sess-a", "file:/k/a"), ("sess-b", "file:/k/b")]);
    let fake = Fake::new(Some(GhVersion(2, 102, 0)))
        .key("/k/a", Ok(KEY))
        .key("/k/b", Err("permission denied"));
    let (out, warnings) = refresh(&base, &host, &fake);
    assert_eq!(
        out,
        PassOutcome::Published {
            owners: vec!["sess-a".into()],
            failed: vec!["sess-b".into()],
        }
    );
    assert_eq!(warnings, vec!["sess-b: permission denied".to_string()]);
    let hosts = std::fs::read_to_string(base.join("sess-a/hosts.yml")).unwrap();
    assert!(hosts.contains(KEY) && hosts.contains("api_host: gw.example.com"), "{hosts}");
    assert!(!base.join("sess-b").exists(), "an unresolvable key withdraws the profile");
    assert_eq!(mode(&base), 0o700);
    assert_eq!(mode(&base.join("sess-a")), 0o700);
    assert_eq!(mode(&base.join("sess-a/hosts.yml")), 0o600);
    assert!(base.join(GH_MARKER).is_file());
    assert_eq!(
        std::fs::read_to_string(base.join(GH_MARKER))
            .unwrap()
            .trim(),
        "/opt/gh/bin/gh"
    );

    // A second invocation picks up a rotated key (and the dropped owner from
    // the first pass stays withdrawn).
    let host = cfg(&[("sess-a", "file:/k/a")]);
    let fake = Fake::new(Some(GhVersion(2, 102, 0))).key("/k/a", Ok(OTHER_KEY));
    let (out, warnings) = refresh(&base, &host, &fake);
    assert!(matches!(out, PassOutcome::Published { .. }), "{out:?}");
    assert!(warnings.is_empty(), "{warnings:?}");
    let hosts = std::fs::read_to_string(base.join("sess-a/hosts.yml")).unwrap();
    assert!(hosts.contains(OTHER_KEY) && !hosts.contains(KEY), "{hosts}");
    assert!(!base.join("sess-b").exists(), "the dropped owner stays withdrawn");

    // An unset host tier withdraws everything.
    let fake = Fake::new(Some(GhVersion(2, 102, 0))).key("/k/a", Ok(KEY));
    assert!(base.join("sess-a/hosts.yml").is_file());
    let (out, _) = refresh(&base, &json!({}), &fake);
    assert_eq!(out, PassOutcome::Off);
    assert!(!base.exists());
}

#[test]
fn refresh_with_an_old_gh_publishes_nothing_and_removes_what_was_there() {
    let tmp = tempfile::tempdir().unwrap();
    let base = session_base_at(tmp.path());
    publish::publish_profile(&base.join("sess-a"), KEY, "gw.example.com").unwrap();
    for gh in [Some(GhVersion(2, 96, 0)), None] {
        let fake = Fake::new(gh).key("/k/a", Ok(KEY));
        let (out, warnings) = refresh(&base, &cfg(&[("sess-a", "file:/k/a")]), &fake);
        assert_eq!(out, PassOutcome::GhTooOld(gh), "{out:?}");
        assert!(warnings.is_empty());
        assert!(!base.exists(), "an old gh withdraws the publication");
    }
}

#[test]
fn refresh_never_touches_the_process_gateway_state() {
    let tmp = tempfile::tempdir().unwrap();
    let base = session_base_at(tmp.path());
    let fake = Fake::new(Some(GhVersion(2, 102, 0))).key("/k/a", Ok(KEY));
    refresh(&base, &cfg(&[("sess-state", "file:/k/a")]), &fake);
    // A one-shot tool owns no route: the session publication is never
    // registered as this process's daemon `Active` set.
    assert!(crate::forge_gateway::active().is_none());
    assert_eq!(crate::forge_gateway::profile_for("sess-state"), None);
}

#[test]
fn a_base_inside_a_checkout_is_refused() {
    let _serial = serial();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::env::set_var(SESSION_ROOT_ENV, repo.join("sess"));
    let err = checked_base().unwrap_err();
    assert!(err.contains("checkout") && err.contains(SESSION_ROOT_ENV), "{err}");
    // A worktree's `.git` file counts too.
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: /elsewhere\n").unwrap();
    assert!(checked_base().is_err());
    std::env::remove_var(SESSION_ROOT_ENV);

    // The default base shape still ends in the suffix readers validate.
    let plain = tempfile::tempdir().unwrap();
    std::env::set_var(SESSION_ROOT_ENV, plain.path());
    let base = checked_base().unwrap();
    std::env::remove_var(SESSION_ROOT_ENV);
    assert_eq!(base, session_base_at(plain.path()));
    assert!(base.ends_with(std::path::Path::new(".loom").join(DIR_NAME)));
}

#[test]
fn a_relative_root_inside_a_checkout_is_refused_from_below_the_root() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    let deeper = repo.join("subdir/deeper");
    std::fs::create_dir_all(&deeper).unwrap();
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    for cwd in [repo.clone(), deeper.clone()] {
        let err = checked_base_at(Path::new("sess"), || Ok(cwd.clone())).unwrap_err();
        assert!(
            err.contains("checkout") && err.contains(SESSION_ROOT_ENV),
            "{err} (cwd {})",
            cwd.display()
        );
    }
    // A worktree's `.git` file counts too.
    std::fs::remove_dir_all(repo.join(".git")).unwrap();
    std::fs::write(repo.join(".git"), "gitdir: /elsewhere\n").unwrap();
    assert!(
        checked_base_at(Path::new("sess"), || Ok(deeper.clone())).is_err(),
        "a worktree `.git` file is refused from below the root"
    );
}

#[test]
fn a_relative_root_traversing_into_a_checkout_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let outside = tmp.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(tmp.path().join("repo/.git")).unwrap();
    let err = checked_base_at(Path::new("../repo/sess"), || Ok(outside.clone())).unwrap_err();
    assert!(err.contains("checkout") && err.contains(SESSION_ROOT_ENV), "{err}");
}

#[test]
fn a_relative_root_outside_every_checkout_resolves_to_an_absolute_base() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let base = checked_base_at(Path::new("sess"), || Ok(home.clone())).unwrap();
    assert!(base.is_absolute());
    assert_eq!(base, home.join("sess/.loom").join(DIR_NAME));
    assert!(!inside_a_checkout(&base));
}

#[test]
fn an_unresolvable_cwd_fails_closed_for_a_relative_root() {
    let err =
        checked_base_at(Path::new("sess"), || Err(std::io::Error::other("gone"))).unwrap_err();
    assert!(err.contains(SESSION_ROOT_ENV) && err.contains("cannot resolve"), "{err}");
    // An absolute root needs no cwd and still resolves.
    let tmp = tempfile::tempdir().unwrap();
    assert!(
        checked_base_at(&tmp.path().join("sess"), || Err(std::io::Error::other("gone"))).is_ok()
    );
}

#[test]
fn clean_removes_only_the_session_publication() {
    let home = tempfile::tempdir().unwrap();
    let base = session_base_at(home.path());
    publish::publish_profile(&base.join("sess-a"), KEY, "gw.example.com").unwrap();
    // A daemon host's workspace publication, elsewhere on the machine.
    let ws = tempfile::tempdir().unwrap();
    let ws_base = crate::forge_gateway::base_dir(ws.path());
    publish::publish_profile(&ws_base.join("fleet-a"), KEY, "gw.example.com").unwrap();

    assert!(clean(&base));
    assert!(!base.exists());
    assert!(!clean(&base), "idempotent");
    assert!(
        ws_base.join("fleet-a/hosts.yml").is_file(),
        "a workspace publication survives the session cleanup"
    );
}

#[test]
fn status_classifies_current_due_missing_and_stray() {
    let tmp = tempfile::tempdir().unwrap();
    let base = session_base_at(tmp.path());
    let host = cfg(&[
        ("sess-a", "file:/k/a"),
        ("sess-old", "file:/k/o"),
        ("sess-gone", "file:/k/g"),
    ]);

    // Nothing published: every configured owner is missing.
    let st = status(&base, &host, SystemTime::now());
    assert!(!st.exists && !st.marker);
    assert_eq!(
        st.owners,
        vec![
            ("sess-a".to_string(), OwnerState::Missing),
            ("sess-gone".to_string(), OwnerState::Missing),
            ("sess-old".to_string(), OwnerState::Missing),
        ]
    );

    // Publish (current), backdate one (due), add unconfigured ones (stray).
    publish::publish_profile(&base.join("sess-a"), KEY, "gw.example.com").unwrap();
    publish::publish_profile(&base.join("sess-old"), KEY, "gw.example.com").unwrap();
    publish::publish_profile(&base.join("sess-stray"), KEY, "gw.example.com").unwrap();
    std::fs::write(base.join(GH_MARKER), "/opt/gh/bin/gh\n").unwrap();
    let stale_mtime = SystemTime::now() - REFRESH_CADENCE - std::time::Duration::from_secs(60);
    std::fs::File::options()
        .append(true)
        .open(base.join("sess-old/hosts.yml"))
        .unwrap()
        .set_modified(stale_mtime)
        .unwrap();

    let now = SystemTime::now();
    let st = status(&base, &host, now);
    assert!(st.exists && st.marker);
    let by: BTreeMap<_, _> = st.owners.clone().into_iter().collect();
    assert!(matches!(by["sess-a"], OwnerState::Current { .. }), "{:?}", by["sess-a"]);
    assert_eq!(
        by["sess-gone"],
        OwnerState::Missing,
        "a configured owner without a profile is missing"
    );
    assert!(
        matches!(&by["sess-old"], OwnerState::Due { age_secs } if *age_secs > REFRESH_CADENCE.as_secs()),
        "{:?}",
        by["sess-old"]
    );
    assert!(matches!(&by["sess-stray"], OwnerState::Stray { .. }), "{:?}", by["sess-stray"]);

    // The rendered report carries owners, ages and the marker — never a key.
    let text = render_status(&st).join("\n");
    assert!(text.contains("current") && text.contains("due for refresh"), "{text}");
    assert!(text.contains("sess-gone: missing"), "{text}");
    assert!(text.contains("stray"), "{text}");
    assert!(text.contains("gh-ok marker: present"), "{text}");
    assert!(!text.contains(KEY), "a credential never reaches the status report");
}

#[test]
fn a_missing_publication_names_the_refresh_command() {
    let tmp = tempfile::tempdir().unwrap();
    let st = status(&session_base_at(tmp.path()), &json!({}), SystemTime::now());
    let lines = render_status(&st);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("none at"), "{}", lines[0]);
    assert!(lines[0].contains("gateway refresh"), "{}", lines[0]);
}

#[test]
fn the_host_tier_is_the_only_config_source() {
    let f = tempfile::tempdir().unwrap();
    let path = f.path().join("defaults.json");
    std::fs::write(
        &path,
        r#"{"forge": {"gateway": {"origin": "https://host-tier.example.com",
        "keyRefs": {"acme": "file:/k/a"}}}}"#,
    )
    .unwrap();
    let v = host_tier_config_from(Some(&path));
    let (cfg, warnings) = parse(&v);
    assert!(warnings.is_empty(), "{warnings:?}");
    let cfg = cfg.unwrap();
    assert_eq!(cfg.api_host, "host-tier.example.com");
    assert_eq!(cfg.key_refs.keys().collect::<Vec<_>>(), ["acme"]);
    // No path, or an unusable file: an empty tier, never a fallback to a repo.
    assert_eq!(host_tier_config_from(None), json!({}));
    let bad = f.path().join("bad.json");
    std::fs::write(&bad, "not json").unwrap();
    assert_eq!(host_tier_config_from(Some(&bad)), json!({}));
}
