//! 2am#4367 P4 break-glass: the `fleet.json` fetch never *depends* on the
//! forge gateway, so a broken gateway can always be unset from the store.
//!
//! 2am#15098: a cacheable store read goes through the gateway first when one
//! is live for the store's owner (its `If-None-Match` intact, so the relay
//! answers from its ETag cache) and falls back to the readers when the
//! gateway cannot answer. A fresh claim read and every write stay off it.
#![allow(clippy::unwrap_used)]

use super::*;

/// The facade never routes a store request through the gateway itself: reads
/// reach it only through [`GhTransport::gateway_read`], which names the
/// profile explicitly.
#[test]
fn every_store_request_is_exempt_from_the_facades_gateway_routing() {
    let tmp = tempfile::tempdir().unwrap();
    let t = GhTransport::new(tmp.path(), "acme/fleet-state");
    let dir = tmp.path().join("cfg");
    for cred in [
        Credential::Ambient,
        Credential::ConfigDir {
            dir: dir.clone(),
            label: "writer app".into(),
        },
    ] {
        for intent in [AccessIntent::Read, AccessIntent::Write] {
            let inv = t.invocation("fleet_store", ops::GIT_READ_OBJECTS, intent, &cred);
            assert!(inv.is_gateway_exempt(), "{cred:?} {intent:?}");
        }
    }
}

/// The exempt router is the reader pool alone, even while a gateway profile
/// is live for the store's owner.
#[test]
fn the_exempt_router_never_answers_with_the_gateway() {
    use crate::forge_bucket_book::Resource;
    use crate::forge_identity::RouteRequest;
    let tmp = tempfile::tempdir().unwrap();
    let live = |_: &str| Some(tmp.path().to_path_buf());
    let req = RouteRequest::gate("gw-fleet-store/fleet-state", None, Resource::Core);
    let now = SystemTime::now();
    // The gateway would serve it ...
    assert!(crate::forge_gateway::decide(&req, now, &live).is_some());
    // ... but the exempt router never consults it: outside a daemon the
    // reader pool has no workspace, so the answer is the writer.
    let routed = crate::gh_invocation::read_router(true)(&req, now);
    assert_eq!(routed, crate::forge_identity::RouteDecision::NoPool);
}

mod via_the_gateway {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A stub `gh` logging `<GH_CONFIG_DIR name> <args>` per call. The
    /// gateway profile answers by its directory name: `gw` serves (`304` for
    /// a conditional read), `down` is a `502`, `blind` a `404`; any other
    /// directory is a reader and serves `200`.
    struct Stub {
        tmp: tempfile::TempDir,
        log: PathBuf,
    }

    fn stub() -> Stub {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let gh = tmp.path().join("gh-stub");
        let script = format!(
            r#"#!/bin/sh
dir="${{GH_CONFIG_DIR:-writer}}"
echo "$(basename "$dir") $*" >> '{log}'
case "$(basename "$dir")" in
  gw)
    case "$*" in
      *If-None-Match*) printf 'HTTP/2.0 304 Not Modified\r\nEtag: "e1"\r\n\r\n';;
      *) printf 'HTTP/2.0 200 OK\r\nEtag: "e1"\r\n\r\n{{"ok":true}}';;
    esac;;
  down)
    printf 'HTTP/2.0 502 Bad Gateway\r\n\r\n'
    echo 'gh: Bad Gateway (HTTP 502)' >&2; exit 1;;
  blind)
    printf 'HTTP/2.0 404 Not Found\r\n\r\n{{"message":"Not Found"}}'
    echo 'gh: Not Found (HTTP 404)' >&2; exit 1;;
  *)
    printf 'HTTP/2.0 200 OK\r\nEtag: "e2"\r\n\r\n{{"ok":true}}';;
esac
"#,
            log = log.display()
        );
        std::fs::write(&gh, script).unwrap();
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Stub { tmp, log }
    }

    impl Stub {
        fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(&self.log)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        /// A pooled transport with one reader `r1`, and the gateway profile
        /// `gateway` (a directory name) live for every owner, or none.
        fn transport(&self, repo: &str, gateway: Option<&str>) -> GhTransport {
            let reader = self.tmp.path().join("r1");
            std::fs::create_dir_all(&reader).unwrap();
            let app = format!("app-r1-{repo}");
            let mut t = GhTransport::with_test_readers(
                self.tmp.path(),
                repo,
                true,
                Box::new(move || Some((reader.clone(), app.clone()))),
                Box::new(|_: &str, _: &str, _: Failure, _: &str| {}),
            );
            t.gh_bin = self.tmp.path().join("gh-stub").display().to_string();
            let dir = gateway.map(|n| {
                let d = self.tmp.path().join(n);
                std::fs::create_dir_all(&d).unwrap();
                d
            });
            t.gateway = Box::new(move |_| dir.clone());
            t
        }
    }

    const PATH: &str = "repos/acme/fleet-state/contents/fleet.json";

    #[test]
    fn a_cacheable_read_goes_to_the_gateway_first_with_its_validator() {
        let s = stub();
        let t = s.transport("acme/fleet-state", Some("gw"));
        let reply = t.get(PATH, None, Some("\"e1\"")).unwrap();
        assert_eq!(reply.status, 304);
        let calls = s.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].starts_with("gw "), "{calls:?}");
        assert!(calls[0].contains("If-None-Match: \"e1\""), "{calls:?}");
        assert_eq!(t.last_credential().unwrap().label(), "gateway app gateway");
    }

    #[test]
    fn no_live_gateway_keeps_todays_reader_route() {
        let s = stub();
        let t = s.transport("acme/fleet-state", None);
        assert_eq!(t.get(PATH, None, None).unwrap().status, 200);
        let calls = s.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].starts_with("r1 "), "{calls:?}");
    }

    #[test]
    fn a_down_gateway_falls_back_to_the_reader() {
        let s = stub();
        let t = s.transport("acme/fleet-state", Some("down"));
        let reply = t.get(PATH, None, None).unwrap();
        assert_eq!(reply.status, 200, "fleet.json stays readable with the gateway down");
        let calls = s.calls();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[0].starts_with("down "), "{calls:?}");
        assert!(calls[1].starts_with("r1 "), "{calls:?}");
        assert!(calls.iter().all(|c| !c.starts_with("writer ")), "{calls:?}");
    }

    #[test]
    fn a_gateway_that_cannot_see_the_repo_leaves_the_answer_to_the_reader() {
        let s = stub();
        let t = s.transport("acme/fleet-state", Some("blind"));
        assert_eq!(t.get(PATH, None, None).unwrap().status, 200);
        let calls = s.calls();
        assert!(calls[0].starts_with("blind ") && calls[1].starts_with("r1 "), "{calls:?}");
    }

    #[test]
    fn a_fresh_claim_read_never_uses_the_gateway() {
        let s = stub();
        let t = s.transport("acme/fleet-state", Some("gw"));
        assert_eq!(
            t.get_fresh("repos/acme/fleet-state/git/ref/loom/resync-claim")
                .unwrap()
                .status,
            200
        );
        let calls = s.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].starts_with("r1 "), "{calls:?}");
    }
}
