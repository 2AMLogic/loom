//! Host-direct Codex seats use the host sandbox. Test that boundary with one
//! local command: an AppArmor setting alone cannot tell whether the installed
//! bubblewrap has a policy allowing it to create namespaces (#14455).
//!
//! Only the first bare-metal draw in a process runs the bounded probe inline.
//! The daemon's session watch refreshes the result off the role loop, even on
//! hosts with no session containers. Container-only pools never run the probe.

#[cfg(all(target_os = "linux", not(test)))]
mod linux {
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::OnceLock;
    use std::time::Duration;

    static BLOCKED: OnceLock<AtomicBool> = OnceLock::new();

    fn probe() -> bool {
        use crate::proc_exec::{run_bounded, Completion};
        let mut command = Command::new("codex");
        command
            .args([
                "sandbox",
                "-c",
                "sandbox_mode=\"read-only\"",
                "--",
                "/bin/true",
            ])
            .current_dir("/")
            .stdin(Stdio::null());
        match run_bounded(command, Duration::from_secs(2)) {
            Ok(Completion::Exited(output)) => super::namespace_refused(
                output.status.success(),
                &String::from_utf8_lossy(&output.stderr),
            ),
            _ => false,
        }
    }

    pub(super) fn blocked() -> bool {
        if std::env::var("LOOM_CODEX_SANDBOX").as_deref() == Ok("danger-full-access") {
            return false;
        }
        BLOCKED
            .get_or_init(|| AtomicBool::new(probe()))
            .load(Ordering::Acquire)
    }

    pub(super) fn refresh_if_used() {
        if let Some(blocked) = BLOCKED.get() {
            blocked.store(probe(), Ordering::Release);
        }
    }
}

/// A missing CLI, unsupported command, timeout or unrelated diagnostic gives
/// no evidence against a seat. Only a failure emitted by bubblewrap counts.
#[cfg(any(test, target_os = "linux"))]
fn namespace_refused(success: bool, stderr: &str) -> bool {
    !success
        && stderr
            .lines()
            .filter_map(|line| line.strip_prefix("bwrap: "))
            .any(|detail| detail.contains("namespace") || detail.starts_with("loopback:"))
}

pub(super) fn blocked() -> bool {
    #[cfg(test)]
    return test_support::BLOCKED.get();
    #[cfg(all(not(test), target_os = "linux"))]
    return linux::blocked();
    #[cfg(all(not(test), not(target_os = "linux")))]
    false
}

pub(crate) fn refresh_if_used() {
    #[cfg(all(not(test), target_os = "linux"))]
    linux::refresh_if_used();
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::cell::Cell;

    thread_local! {
        pub(super) static BLOCKED: Cell<bool> = const { Cell::new(false) };
    }

    pub(crate) struct BlockedGuard(bool);

    impl Drop for BlockedGuard {
        fn drop(&mut self) {
            BLOCKED.set(self.0);
        }
    }

    pub(crate) fn block() -> BlockedGuard {
        BlockedGuard(BLOCKED.replace(true))
    }

    #[test]
    fn only_a_failed_namespace_setup_marks_the_host_blocked() {
        use super::namespace_refused;
        let refused = "bwrap: No permissions to create a new namespace";
        assert!(namespace_refused(false, refused));
        assert!(namespace_refused(
            false,
            "bwrap: loopback: Failed RTM_NEWADDR: Operation not permitted"
        ));
        assert!(!namespace_refused(true, refused));
        assert!(!namespace_refused(false, "warning: namespace support unknown"));
        assert!(!namespace_refused(false, "bwrap: execvp /bin/true: No such file"));
    }
}
