#![allow(clippy::unwrap_used)]
//! #15158: every `gh release …` read skips the forge gateway, decided from the
//! argv at the facade, so no site needs its own `.gateway_exempt()`.
//!
//! `forge_gateway` documents that releases never use it, and a refused asset
//! download classifies as `Failure::Credential`, which withdraws the gateway
//! for the whole owner (#15134). Which release reads the relay actually
//! refuses is not measured here; these tests pin the routing decision only.

use super::*;
use std::time::Duration;

fn read(op: &'static str, args: &[&str]) -> GhInvocation {
    GhInvocation::new(
        Operation::new(op),
        AccessIntent::Read,
        GhTarget::None,
        Duration::from_secs(1),
    )
    .reader_repo("acme/loom")
    .args(args)
}

#[test]
fn release_view_assets_and_download_skip_the_gateway_without_a_site_flag() {
    for (op, args) in [
        // `daemon_update::artifact::release_view_args`
        (
            "release.view",
            &[
                "release",
                "view",
                "v1",
                "--json",
                "tagName",
                "-R",
                "acme/loom",
            ][..],
        ),
        // `release_resolve::asset_names_detailed`
        (
            "release.assets",
            &[
                "release",
                "view",
                "v1",
                "--json",
                "assets",
                "-R",
                "acme/loom",
            ][..],
        ),
        // `release_fetch::download`
        (
            "release.download",
            &["release", "download", "v1", "-R", "acme/loom", "-p", "x"][..],
        ),
    ] {
        let inv = read(op, args);
        assert!(!inv.is_gateway_exempt(), "{op}: no per-site flag is set");
        assert!(inv.skips_gateway(), "{op}");
    }
}

#[test]
fn a_non_release_read_is_still_gateway_first() {
    for args in [
        &["issue", "view", "7", "-R", "acme/loom"][..],
        &["api", "repos/acme/loom/releases"][..],
        &["pr", "list", "--search", "release"][..],
    ] {
        assert!(!read("issue.view", args).skips_gateway(), "{args:?}");
    }
}

#[test]
fn an_explicit_site_exemption_still_applies() {
    assert!(read("issue.view", &["issue", "view", "7"])
        .gateway_exempt()
        .skips_gateway());
}
