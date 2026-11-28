//! Library-test fixture: write a file and mark it executable.
//!
//! Tests stand in fake `gh`, `git`, hook and spawn scripts all the time, and
//! every one needs the same two steps: write the body, then `chmod 0755`. One
//! helper here replaces the private per-file copies that had accumulated
//! under four different names. [`retry_while_executable_busy`] is its
//! companion for the `ETXTBSY` window writing that fixture opens, and
//! [`hung_with_grandchild`] with [`grandchild_outlived`] is the pair a
//! deadline test uses to ask whether a kill reached the whole process group.

use std::cell::Cell;
use std::fs;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

/// Write `body` to `path` and make it executable (mode 0755).
///
/// Missing parent directories are created, so a fixture can name a script at
/// `root/.loom/scripts/x.sh` without laying the tree out first. Panics on
/// failure: a fixture that cannot be built must fail the test loudly.
///
/// # The `ETXTBSY` hazard (#15013)
///
/// Under plain `cargo test` — every test a thread of one process — a fixture
/// exec'd straight after this can be refused with
/// [`io::ErrorKind::ExecutableFileBusy`] ("Text file busy"): a sibling test
/// thread's `fork` briefly holds a copy of the write fd this helper opened,
/// and the kernel refuses to `exec` an inode that still has writers.
/// `cargo nextest` runs one process per test, so CI cannot see this. Execs
/// through [`crate::proc_exec::run_bounded`] are waited out at the spawn in
/// `--lib` test builds (not integration tests under `tests/`, which link the
/// lib without `cfg(test)`); an exec that does not go through it must be wrapped in
/// [`retry_while_executable_busy`] itself.
pub(crate) fn write_executable(path: &Path, body: impl AsRef<[u8]>) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

/// How often [`retry_while_executable_busy`] tries again while the program
/// stays busy. With [`BUSY_PAUSE`] between attempts that is about two seconds
/// for a forked-but-not-yet-`execve`'d sibling to reach its `exec` — orders
/// of magnitude past any scheduler latency a loaded host shows.
const BUSY_ATTEMPTS: usize = 1_000;

/// The pause between two attempts of [`retry_while_executable_busy`].
const BUSY_PAUSE: Duration = Duration::from_millis(2);

/// Run `attempt` again while it fails with [`io::ErrorKind::ExecutableFileBusy`].
///
/// A refused attempt ran nothing — the kernel refused the `exec` itself — so
/// repeating `attempt` has no side effect of its own, and once one attempt
/// succeeds no copy of the write fd can reappear. Any other outcome is
/// returned at once, and after [`BUSY_ATTEMPTS`] attempts the last busy error
/// is: the caller decides whether that is fatal.
pub(crate) fn retry_while_executable_busy<T>(
    attempt: impl FnMut() -> io::Result<T>,
) -> io::Result<T> {
    retry_while_busy(attempt, BUSY_ATTEMPTS, || std::thread::sleep(BUSY_PAUSE))
}

/// [`retry_while_executable_busy`] with the attempt cap and pause exposed, so
/// the unit tests below do not sleep.
fn retry_while_busy<T>(
    mut attempt: impl FnMut() -> io::Result<T>,
    attempts: usize,
    mut pause: impl FnMut(),
) -> io::Result<T> {
    for _ in 1..attempts {
        match attempt() {
            Err(error) if error.kind() == io::ErrorKind::ExecutableFileBusy => {
                REFUSED_EXECS.with(|count| count.set(count.get() + 1));
                pause();
            }
            outcome => return outcome,
        }
    }
    attempt()
}

thread_local! {
    /// Execs [`retry_while_busy`] refused on this thread since the last
    /// [`take_refused_execs`].
    static REFUSED_EXECS: Cell<usize> = const { Cell::new(0) };
}

/// Forget the refusals recorded on the calling thread so far. Call it just
/// before an exec whose wait a test wants witnessed, so that
/// [`take_refused_execs`] afterwards speaks for that exec alone.
pub(crate) fn reset_refused_execs() {
    REFUSED_EXECS.with(|count| count.set(0));
}

/// How many execs [`retry_while_executable_busy`] refused with `ETXTBSY` on
/// the calling thread since this was last called, starting the count over.
///
/// The witness for a "waited for, not failed" test (#15668). A duration only
/// says the call took long, which a loaded host or a slow preamble produces
/// with no wait at all; a refusal is the wait's own cause, so zero means the
/// wait was never exercised however long the call took. Per thread: a sibling
/// test's refusals vouch for nothing here.
#[must_use]
pub(crate) fn take_refused_execs() -> usize {
    REFUSED_EXECS.with(|count| count.replace(0))
}

/// The body of a script that starts a sleeping grandchild, records its pid in
/// `pidfile`, then hangs: the fixture for a deadline having to take a whole
/// process group, not only the child it spawned.
#[cfg(unix)]
pub(crate) fn hung_with_grandchild(pidfile: &Path) -> String {
    format!("/bin/sleep 120 & echo $! > '{}'; exec /bin/sleep 120", pidfile.display())
}

/// How long [`grandchild_outlived`] waits for the pid file, and then for a
/// deadline kill still in flight to be reaped, before it reads a survivor.
#[cfg(unix)]
const GRANDCHILD_GRACE: Duration = Duration::from_secs(2);

#[cfg(unix)]
const GRANDCHILD_POLL: Duration = Duration::from_millis(20);

/// Whether the grandchild [`hung_with_grandchild`] recorded in `pidfile` is
/// still running, checked by pid. A survivor is killed before `true` is
/// returned, so a failing assertion never leaks it.
#[cfg(unix)]
pub(crate) fn grandchild_outlived(pidfile: &Path) -> bool {
    let deadline = Instant::now() + GRANDCHILD_GRACE;
    let pid = loop {
        let recorded = fs::read_to_string(pidfile)
            .ok()
            .and_then(|raw| raw.trim().parse::<libc::pid_t>().ok())
            .filter(|pid| *pid > 0);
        if let Some(pid) = recorded {
            break pid;
        }
        if Instant::now() >= deadline {
            panic!(
                "the fixture's child never recorded its grandchild's pid in {}",
                pidfile.display()
            );
        }
        std::thread::sleep(GRANDCHILD_POLL);
    };
    let deadline = Instant::now() + GRANDCHILD_GRACE;
    loop {
        // SAFETY: signal 0 performs error checking only; nothing is delivered.
        let alive = unsafe { libc::kill(pid, 0) == 0 };
        if !alive || Instant::now() >= deadline {
            if alive {
                // SAFETY: `pid` is the fixture's own grandchild.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            return alive;
        }
        std::thread::sleep(GRANDCHILD_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> io::Error {
        io::Error::from(io::ErrorKind::ExecutableFileBusy)
    }

    /// An exec that is refused `busy_left` times, then succeeds, counting
    /// every call.
    struct Flaky {
        busy_left: usize,
        calls: usize,
    }

    impl Flaky {
        fn attempt(&mut self) -> io::Result<usize> {
            self.calls += 1;
            if self.busy_left > 0 {
                self.busy_left -= 1;
                return Err(busy());
            }
            Ok(self.calls)
        }
    }

    #[test]
    fn a_program_that_stops_being_busy_is_executed() {
        let mut flaky = Flaky {
            busy_left: 3,
            calls: 0,
        };
        assert_eq!(retry_while_busy(|| flaky.attempt(), 10, || ()).unwrap(), 4);
        assert_eq!(flaky.calls, 4, "each pause must be followed by one more attempt");
    }

    #[test]
    fn a_program_busy_at_every_attempt_yields_the_last_busy_error() {
        let mut flaky = Flaky {
            busy_left: usize::MAX,
            calls: 0,
        };
        let error = retry_while_busy(|| flaky.attempt(), 5, || ()).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ExecutableFileBusy);
        assert_eq!(flaky.calls, 5, "the cap counts attempts, not pauses");
    }

    #[test]
    fn any_other_error_is_returned_at_once() {
        let mut calls = 0;
        let attempt = || -> io::Result<usize> {
            calls += 1;
            Err(io::Error::from(io::ErrorKind::NotFound))
        };
        let error =
            retry_while_busy(attempt, 10, || panic!("a non-busy error must not be paused on"))
                .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(calls, 1);
    }

    #[test]
    fn the_default_pauses_wait_out_one_transient() {
        let mut flaky = Flaky {
            busy_left: 1,
            calls: 0,
        };
        assert_eq!(retry_while_executable_busy(|| flaky.attempt()).unwrap(), 2);
    }

    #[test]
    fn every_refused_attempt_is_counted_once_on_the_calling_thread() {
        reset_refused_execs();
        let mut flaky = Flaky {
            busy_left: 2,
            calls: 0,
        };
        assert_eq!(retry_while_executable_busy(|| flaky.attempt()).unwrap(), 3);
        assert_eq!(take_refused_execs(), 2, "one per refused attempt");
        assert_eq!(take_refused_execs(), 0, "taking starts the count over");

        let mut prompt = Flaky {
            busy_left: 0,
            calls: 0,
        };
        retry_while_executable_busy(|| prompt.attempt()).unwrap();
        assert_eq!(take_refused_execs(), 0, "an exec never refused is no wait");
    }

    #[test]
    fn a_sibling_threads_refusals_are_not_mine() {
        reset_refused_execs();
        let theirs = std::thread::spawn(|| {
            let mut flaky = Flaky {
                busy_left: 2,
                calls: 0,
            };
            retry_while_busy(|| flaky.attempt(), 10, || {}).unwrap();
            take_refused_execs()
        })
        .join()
        .unwrap();
        assert_eq!(theirs, 2, "the thread that was refused sees its own count");
        assert_eq!(
            take_refused_execs(),
            0,
            "a sibling test's wait must not vouch for this thread's exec"
        );
    }
}
