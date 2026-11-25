//! Which Codex seat a run used (Issue #15291): `loom.codex.account` on
//! `loom.runtime.run`, `loom.runtime.preflight` and `loom.runtime.usage`.
//!
//! Without it, "why is this seat not taking work?" cannot be answered from
//! traces: runs and usage per seat cannot be counted. The key is **Codex
//! only** — a Claude, native or unresolved runtime never carries it — and the
//! value is the seat's account name, emitted only when it passes
//! [`is_account_name`] (the same conservative charset the egress proxy
//! accepts), so no raw string ever reaches an exported span.
//!
//! This is its own key, not part of [`super::llm_billing`]: that module's rule
//! is that its attributes never carry an account name.

use crate::telemetry::trace::TraceAttributes;
use crate::worker_spawn::egress_proxy::rotation::is_account_name;

/// The span attribute naming the Codex seat a run used.
pub const KEY: &str = "loom.codex.account";

/// The seat name for a launch, when it is a Codex launch with a valid name.
#[must_use]
pub fn seat(runtime: &str, account: Option<&str>) -> Option<String> {
    (runtime == "codex")
        .then_some(account)
        .flatten()
        .filter(|name| is_account_name(name))
        .map(str::to_owned)
}

/// Stamp [`KEY`] on a launch's attributes. No-op for a non-Codex runtime or an
/// invalid/empty name.
pub fn stamp(attributes: &mut TraceAttributes, runtime: &str, account: Option<&str>) {
    if let Some(name) = seat(runtime, account) {
        attributes.insert(KEY.into(), name);
    }
}

/// [`stamp`] from the launch's own environment: `LOOM_ACCOUNT_NAME` names the
/// seat, and `LOOM_ACCOUNT_PROVIDER` (when set) must say `codex`.
pub fn stamp_from_env(attributes: &mut TraceAttributes, runtime: &str) {
    let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    if var("LOOM_ACCOUNT_PROVIDER").is_some_and(|provider| provider != "codex") {
        return;
    }
    stamp(attributes, runtime, var("LOOM_ACCOUNT_NAME").as_deref());
}

/// The seat every one of `launches` agrees on, for usage that cannot be split
/// between them. No launch carrying a seat, or two seats, is `None` — never one
/// launch's seat for all of them.
#[must_use]
pub fn agreed<'a>(launches: impl IntoIterator<Item = &'a TraceAttributes>) -> Option<String> {
    let mut agreed: Option<&str> = None;
    let mut any = false;
    for launch in launches {
        let seat = launch.get(KEY).map(String::as_str);
        if any && seat != agreed {
            return None;
        }
        any = true;
        agreed = seat;
    }
    agreed.map(str::to_owned)
}

/// Stamp the seat `launches` agree on onto a usage span's attributes.
pub fn stamp_agreed<'a>(
    attributes: &mut TraceAttributes,
    launches: impl IntoIterator<Item = &'a TraceAttributes>,
) {
    if let Some(name) = agreed(launches) {
        attributes.insert(KEY.into(), name);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[path = "codex_seat_tests.rs"]
mod tests;
