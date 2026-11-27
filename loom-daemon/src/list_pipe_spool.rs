//! Spool `--list` output when stdout is a pipe (#15553).
//!
//! cargo-nextest discovers tests by running every test binary with `--list
//! --format terse` and reading the listing through a pipe. `libtest` writes
//! the listing through `std::io::stdout()`, a `LineWriter`, so this crate's
//! library test binary issues one `write(1, …)` syscall per test: 20,497
//! writes carrying 1.8 MiB (measured at `d495cc3c4`), all into a 64 KiB pipe
//! whose read end nextest polls edge-triggered (`EPOLLET`, mio). Six times
//! (#11121, #14066, #14691, #15094, #15378, #15553) the shard stalled there:
//! the binary sat in `anon_pipe_write` on a full pipe while every nextest
//! thread idled in `epoll_wait`. The parent had stopped reading, and a writer
//! that blocks on an already-full pipe adds no data, so it wakes nobody: the
//! stalled writer had no signal left to send, and the step died on its
//! timeout.
//!
//! This module takes the long per-line exchange off the pipe. Before `main`,
//! a test binary invoked with `--list` whose `fd 1` is a pipe:
//!
//! 1. enlarges that pipe toward `/proc/sys/fs/pipe-max-size` (best effort);
//! 2. saves the real stdout on a close-on-exec descriptor and `dup2`s an
//!    in-memory file (`memfd_create`) onto `fd 1`, so `libtest`'s writes
//!    never reach the pipe;
//! 3. registers an `atexit` hook that copies the whole spool into the saved
//!    pipe in 1 MiB chunks, then closes it.
//!
//! The pipe now stays empty while the tests are enumerated, so the child's
//! first bytes always land in an empty pipe: a write that adds data and so
//! wakes the parent, where the stalled writer had none left to make. Its
//! whole pipe traffic becomes a couple of writes at exit instead of ~20,000
//! spread across discovery.
//!
//! Output integrity fails closed. A spool that cannot be set up leaves `fd 1`
//! untouched, so `libtest` writes straight to stdout as before. Once `fd 1`
//! is redirected, any error while flushing the listing to the pipe writes a
//! diagnostic to `stderr` and exits 101, never 0 with a truncated list.
//!
//! Only the library test binary mounts this module (`lib.rs`), beside
//! `live_gh_guard`; no integration-test binary and not the shipped binary
//! ever redirects `fd 1`.
//!
//! Two switches need no rebuild: `LOOM_LIST_PIPE_SPOOL_TRACE=1` prints one
//! `stderr` summary per spooled listing (`<bytes> bytes -> stdout pipe in
//! <writes> writes`), and `LOOM_LIST_PIPE_SPOOL_DISABLE=1` leaves `fd 1`
//! alone.

use std::ffi::CString;
use std::io;
use std::io::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::io::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};

/// Prefix of every diagnostic this module writes to `stderr`.
const MESSAGE_PREFIX: &str = "loom-daemon list-pipe-spool: ";

/// Set to a non-empty value to print one summary line per spooled `--list`.
const TRACE_ENV: &str = "LOOM_LIST_PIPE_SPOOL_TRACE";

/// Set to a non-empty value to leave `fd 1` alone (bisect and roll-back).
const DISABLE_ENV: &str = "LOOM_LIST_PIPE_SPOOL_DISABLE";

/// Bytes per flush `write`. `libtest` emits ~85 bytes per syscall; a listing
/// crosses the pipe in a couple of these instead of ~20,000 of those.
const FLUSH_CHUNK: usize = 1024 * 1024;

/// Upper bound on the requested pipe capacity, whatever `pipe-max-size` says.
const MAX_PIPE_CAPACITY: usize = 8 * 1024 * 1024;

/// The kernel's default pipe capacity on a 4 KiB-page system (16 pages).
const DEFAULT_PIPE_CAPACITY: usize = 64 * 1024;

/// `MFD_CLOEXEC` from `linux/memfd.h`; `libc` does not export it for glibc.
const MFD_CLOEXEC: libc::c_uint = 0x0001;

/// The saved write end of the real stdout pipe; -1 when no spool is active.
/// Stored once before `main`, while the process is single-threaded.
static SAVED_STDOUT: AtomicI32 = AtomicI32::new(-1);

/// Run [`install`] before `main` (the mechanism the `ctor` crate uses), while
/// the process is single-threaded and nothing has written to stdout.
#[used]
#[link_section = ".init_array"]
static INSTALL: extern "C" fn() = install;

extern "C" fn install() {
    // A spool that cannot be set up must not take every `--list` run down
    // with a pre-main panic: say so and let the listing go out unspooled.
    if let Err(e) = try_install() {
        eprint_diag(&format!("not installed: {e}"));
    }
}

/// Activate the spool for a `--list` run writing to a pipe. Every failure
/// returns before `fd 1` is touched, or puts it back.
fn try_install() -> io::Result<()> {
    if env_flag(DISABLE_ENV) {
        return Ok(());
    }
    // No argv to read is not a listing anyone can redirect safely.
    let Some(args) = cmdline_args() else {
        return Ok(());
    };
    if !is_list_invocation(&args) || !is_fifo(libc::STDOUT_FILENO) {
        return Ok(());
    }
    // Best effort: a default-sized pipe still works, with more flush writes.
    enlarge_pipe(libc::STDOUT_FILENO, pipe_max_size());

    let saved = dup_cloexec(libc::STDOUT_FILENO)?;
    let spool = match create_spool() {
        Ok(fd) => fd,
        Err(e) => {
            let _ = close_fd(saved);
            return Err(e);
        }
    };
    // SAFETY: `dup2` replaces `fd 1`, which `saved` keeps open, with the spool.
    if unsafe { libc::dup2(spool, libc::STDOUT_FILENO) } < 0 {
        let e = io::Error::last_os_error();
        let _ = close_fd(spool);
        let _ = close_fd(saved);
        return Err(e);
    }
    let _ = close_fd(spool);
    SAVED_STDOUT.store(saved, Ordering::SeqCst);
    // SAFETY: registering a plain `extern "C" fn()` with libc's atexit table.
    if unsafe { libc::atexit(flush_at_exit) } != 0 {
        let e = io::Error::last_os_error();
        SAVED_STDOUT.store(-1, Ordering::SeqCst);
        let _ = restore_stdout(saved);
        return Err(e);
    }
    Ok(())
}

/// Deliver the spooled listing once nothing else will write to stdout. From
/// here on the listing exists only in the spool, so any failure to hand it
/// over ends the process through [`fail`], never with status 0.
extern "C" fn flush_at_exit() {
    let saved = SAVED_STDOUT.swap(-1, Ordering::SeqCst);
    if saved < 0 {
        return;
    }
    // A reader that vanished must come back as `EPIPE`, to be reported, not
    // end the process with `SIGPIPE` before the diagnostic is out.
    // SAFETY: `SIG_IGN` is a standard disposition.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
    // Anything still buffered without a newline goes into the spool first.
    let _ = std::io::stdout().flush();
    let delivered = copy_spool(libc::STDOUT_FILENO, saved).and_then(|(bytes, writes)| {
        restore_stdout(saved)?;
        Ok((bytes, writes))
    });
    match delivered {
        Ok((bytes, writes)) if env_flag(TRACE_ENV) => {
            eprint_diag(&format!("{bytes} bytes -> stdout pipe in {writes} writes"));
        }
        Ok(_) => {}
        Err(e) => fail(&format!("cannot deliver the --list listing to stdout ({e})")),
    }
}

/// Put the pipe back on `fd 1` and let go of the saved descriptor.
fn restore_stdout(saved: RawFd) -> io::Result<()> {
    // SAFETY: `dup2` replaces `fd 1` with the saved pipe end.
    if unsafe { libc::dup2(saved, libc::STDOUT_FILENO) } < 0 {
        let e = io::Error::last_os_error();
        let _ = close_fd(saved);
        return Err(e);
    }
    close_fd(saved)
}

/// The exit-101 path: the listing is not on the pipe in full, so this run
/// must not pass for a successful listing.
fn fail(why: &str) -> ! {
    eprint_diag(&format!("{why}; exiting 101 rather than 0 with an incomplete test list"));
    // SAFETY: ending the process from an exit hook. `_exit` skips the hooks
    // still queued, none of which may turn this into a 0.
    unsafe { libc::_exit(101) }
}

/// Whether argv names `--list` as a flag: an exact token before the first
/// `--`, which hands the rest to the tests.
fn is_list_invocation(args: &[Vec<u8>]) -> bool {
    args.iter()
        .take_while(|arg| arg.as_slice() != b"--")
        .any(|arg| arg.as_slice() == b"--list")
}

/// This process's argv, read from `/proc/self/cmdline` (NUL-separated), which
/// is available before `main` initializes `std::env::args`.
fn cmdline_args() -> Option<Vec<Vec<u8>>> {
    let raw = std::fs::read("/proc/self/cmdline").ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
}

/// Whether `fd` is a pipe's end (`S_ISFIFO`); false when it cannot be told.
fn is_fifo(fd: RawFd) -> bool {
    // SAFETY: `stat` is plain old data; all zeros is a valid value for it.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `st` is valid for writes and `fstat` only writes to it.
    let ok = unsafe { libc::fstat(fd, &mut st) } == 0;
    ok && st.st_mode & libc::S_IFMT == libc::S_IFIFO
}

/// `/proc/sys/fs/pipe-max-size`, or 0 when it cannot be read.
fn pipe_max_size() -> usize {
    std::fs::read_to_string("/proc/sys/fs/pipe-max-size")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// A pipe end's current capacity, in bytes.
fn pipe_capacity(fd: RawFd) -> io::Result<usize> {
    // SAFETY: `F_GETPIPE_SZ` takes no argument and only reads pipe state.
    let size = unsafe { libc::fcntl(fd, libc::F_GETPIPE_SZ) };
    if size < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(size as usize)
}

fn set_pipe_capacity(fd: RawFd, size: usize) -> io::Result<()> {
    // SAFETY: `F_SETPIPE_SZ` takes one `int` argument, which is what is passed.
    if unsafe { libc::fcntl(fd, libc::F_SETPIPE_SZ, size as libc::c_int) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Raise a pipe end's capacity toward `upper`, never above
/// `MAX_PIPE_CAPACITY` and never below what it already has. Best effort: an
/// unprivileged `F_SETPIPE_SZ` beyond the user's allowance fails with
/// `EPERM`, so each halving is tried in turn. Returns the capacity the pipe
/// has when this returns (0 when it cannot be read).
fn enlarge_pipe(fd: RawFd, upper: usize) -> usize {
    let Ok(current) = pipe_capacity(fd) else {
        return 0;
    };
    let mut candidate = upper.min(MAX_PIPE_CAPACITY);
    while candidate > current {
        if set_pipe_capacity(fd, candidate).is_ok() {
            break;
        }
        candidate /= 2;
    }
    pipe_capacity(fd).unwrap_or(current)
}

/// A new seekable, write-readable, close-on-exec scratch file: a `memfd`, or
/// an unlinked temp file where `memfd_create` is unavailable.
fn create_spool() -> io::Result<RawFd> {
    match memfd_spool() {
        Ok(fd) => Ok(fd),
        Err(e) => tempfile_spool().map_err(|fallback| {
            io::Error::new(e.kind(), format!("{e}; temp-file fallback: {fallback}"))
        }),
    }
}

fn memfd_spool() -> io::Result<RawFd> {
    let name = c"loom-list-spool";
    // SAFETY: `name` is a valid NUL-terminated string and is only read.
    let fd = unsafe { libc::memfd_create(name.as_ptr(), MFD_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(fd)
}

fn tempfile_spool() -> io::Result<RawFd> {
    let path = std::env::temp_dir().join(format!("loom-list-spool-{}.tmp", std::process::id()));
    let cpath = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: `cpath` is a valid NUL-terminated path and the mode is a plain
    // `int`. `O_EXCL` refuses a stale file left by a reused pid, so a second
    // attempt after removing it cannot clobber anyone's file.
    let open = || unsafe {
        libc::open(
            cpath.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600,
        )
    };
    let mut fd = open();
    if fd < 0 && io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
        let _ = std::fs::remove_file(&path);
        fd = open();
    }
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Unlinked at once: the file lives on as long as `fd` is open, under no name.
    let _ = std::fs::remove_file(&path);
    Ok(fd)
}

/// Duplicate `fd` onto the lowest free descriptor at or above 3 that closes
/// on `exec`, so no child inherits it and keeps the pipe open.
fn dup_cloexec(fd: RawFd) -> io::Result<RawFd> {
    // SAFETY: `F_DUPFD_CLOEXEC` takes one `int` argument, which is what is passed.
    let dup = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if dup < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(dup)
}

fn close_fd(fd: RawFd) -> io::Result<()> {
    // SAFETY: closing a descriptor this module opened; nothing else uses it.
    if unsafe { libc::close(fd) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Copy the whole spool at `src` (from its start) to `dst` in `FLUSH_CHUNK`
/// writes, retrying on `EINTR` and on partial writes. Returns the byte count
/// and the number of `write` syscalls it took.
fn copy_spool(src: RawFd, dst: RawFd) -> io::Result<(usize, usize)> {
    // SAFETY: seeking a seekable descriptor; the spool is one by construction.
    if unsafe { libc::lseek(src, 0, libc::SEEK_SET) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut buf = vec![0u8; FLUSH_CHUNK];
    let (mut bytes, mut writes) = (0, 0);
    loop {
        // SAFETY: `buf` is initialized for `buf.len()` bytes; `read` fills it.
        let n = unsafe { libc::read(src, buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
            continue;
        }
        let n = n as usize;
        if n == 0 {
            return Ok((bytes, writes));
        }
        writes += write_all(dst, &buf[..n])?;
        bytes += n;
    }
}

/// Write all of `buf`; returns the number of `write` syscalls it took.
fn write_all(fd: RawFd, mut buf: &[u8]) -> io::Result<usize> {
    let mut writes = 0;
    while !buf.is_empty() {
        // SAFETY: `buf` is valid for `buf.len()` bytes and is only read.
        let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if n <= 0 {
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0"));
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
            continue;
        }
        writes += 1;
        buf = &buf[n as usize..];
    }
    Ok(writes)
}

/// Whether `name` is set to a non-empty value.
fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// One `MESSAGE_PREFIX`-tagged line on `stderr`, through a raw `write`: an
/// exit hook has no panic path, whatever state `std`'s streams are in.
fn eprint_diag(msg: &str) {
    let _ = write_all(libc::STDERR_FILENO, format!("{MESSAGE_PREFIX}{msg}\n").as_bytes());
}

mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::io::FromRawFd;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    /// Set only by [`a_test_run_keeps_stdout_on_the_pipe`] for its child, so
    /// the fixture below asserts only there.
    const FIXTURE_ENV: &str = "LOOM_LIST_PIPE_SPOOL_FIXTURE";

    /// A pipe as `(read, write)` files that close on drop.
    fn pipe() -> (File, File) {
        let mut fds = [0; 2];
        // SAFETY: `fds` is valid for two `int`s and `pipe2` only writes them.
        assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
        // SAFETY: both descriptors are fresh and owned by the `File`s alone.
        let (r, w) = unsafe { (File::from_raw_fd(fds[0]), File::from_raw_fd(fds[1])) };
        (r, w)
    }

    /// This test binary running `--list --format terse`, the invocation
    /// nextest makes, with its `stdout` arranged by `arrange`.
    fn list_command(arrange: impl FnOnce(&mut Command)) -> Command {
        let mut cmd = Command::new(std::env::current_exe().unwrap());
        cmd.args(["--list", "--format", "terse"]);
        arrange(&mut cmd);
        cmd
    }

    /// The listing as written to a regular file, where nothing intercepts it.
    fn file_listing() -> Vec<u8> {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("list.txt");
        let out = list_command(|c| {
            c.stdout(Stdio::from(File::create(&path).unwrap()))
                .stderr(Stdio::null());
        })
        .output()
        .unwrap();
        assert!(out.status.success(), "{:?}", out.status.code());
        std::fs::read(&path).unwrap()
    }

    /// Drain a spooled listing the way nextest's reader would, with a buffer
    /// large enough for a whole flush chunk, and record each `read` size.
    fn drain(mut stdout: std::process::ChildStdout) -> (Vec<u8>, Vec<usize>, usize) {
        let mut got = Vec::new();
        let mut reads = Vec::new();
        let mut buf = vec![0u8; FLUSH_CHUNK];
        loop {
            let n = stdout.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            reads.push(n);
            got.extend_from_slice(&buf[..n]);
        }
        let capacity = pipe_capacity(stdout.as_raw_fd()).unwrap();
        (got, reads, capacity)
    }

    #[test]
    fn list_flag_is_matched_only_before_the_separator() {
        let args = |a: &[&str]| a.iter().map(|s| s.as_bytes().to_vec()).collect::<Vec<_>>();
        assert!(is_list_invocation(&args(&["/bin/loom_daemon", "--list"])));
        assert!(is_list_invocation(&args(&["/bin/loom_daemon", "--format", "terse", "--list"])));
        assert!(is_list_invocation(&args(&["/bin/loom_daemon", "--list", "--", "--list"])));
        assert!(!is_list_invocation(&args(&["/bin/loom_daemon"])));
        assert!(!is_list_invocation(&args(&["/bin/loom_daemon", "--", "--list"])));
        assert!(!is_list_invocation(&args(&["/bin/loom_daemon", "--listed"])));
        assert!(!is_list_invocation(&args(&["/bin/loom_daemon", "--list=terse"])));
        assert!(!is_list_invocation(&args(&["/bin/loom_daemon", "-Zunstable-options"])));
        assert!(!is_list_invocation(&args(&[])));
    }

    #[test]
    fn cmdline_args_are_this_process_argv() {
        let from_proc = cmdline_args().unwrap();
        let expected: Vec<Vec<u8>> = std::env::args_os().map(|a| a.as_bytes().to_vec()).collect();
        assert!(!expected.is_empty());
        assert_eq!(from_proc, expected);
    }

    #[test]
    fn fifo_detection_sees_pipes_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let (r, w) = pipe();
        assert!(is_fifo(w.as_raw_fd()));
        assert!(is_fifo(r.as_raw_fd()));
        let file = File::create(tmp.path().join("f")).unwrap();
        assert!(!is_fifo(file.as_raw_fd()));
        let null = File::open("/dev/null").unwrap();
        assert!(!is_fifo(null.as_raw_fd()));
    }

    #[test]
    fn enlarge_pipe_raises_capacity_toward_the_limit_and_never_shrinks() {
        let (_r, w) = pipe();
        let w = w.as_raw_fd();
        let before = pipe_capacity(w).unwrap();
        let max = pipe_max_size();
        let after = enlarge_pipe(w, max);
        if max > DEFAULT_PIPE_CAPACITY {
            assert!(after > before, "{before} -> {after} (pipe-max-size {max})");
            // The kernel rounds a request up to whole pages.
            let target = max.min(MAX_PIPE_CAPACITY);
            // SAFETY: `_SC_PAGESIZE` is a pure query with no preconditions.
            let page = usize::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).unwrap();
            assert!(after >= target && after - target < page, "{after} vs {target}");
        } else {
            assert_eq!(after, before, "nothing to enlarge toward");
        }
        assert_eq!(enlarge_pipe(w, 0), after);
        assert_eq!(enlarge_pipe(w, DEFAULT_PIPE_CAPACITY), after);
        // Above the allowance a request is refused and leaves the pipe alone;
        // a privileged runner is allowed it, so only "not smaller" holds.
        assert!(enlarge_pipe(w, usize::MAX) >= after);
    }

    /// Fill a spool with a recognisable pattern of `len` bytes.
    fn fill(spool: RawFd, len: usize) -> Vec<u8> {
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        write_all(spool, &payload).unwrap();
        payload
    }

    /// A spool as a `File`, so a test's spool closes on drop.
    fn spool() -> File {
        // SAFETY: `create_spool` returns a fresh descriptor the `File` owns alone.
        unsafe { File::from_raw_fd(create_spool().unwrap()) }
    }

    #[test]
    fn copy_spool_sends_whole_chunks_and_reports_what_it_sent() {
        let spool = spool();
        let payload = fill(spool.as_raw_fd(), 3 * FLUSH_CHUNK);
        let (mut r, w) = pipe();
        let capacity = enlarge_pipe(w.as_raw_fd(), pipe_max_size());
        let reader = thread::spawn(move || {
            let mut got = Vec::new();
            let mut reads = Vec::new();
            let mut buf = vec![0u8; FLUSH_CHUNK];
            loop {
                let n = r.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                reads.push(n);
                got.extend_from_slice(&buf[..n]);
            }
            (got, reads)
        });
        let (bytes, writes) = copy_spool(spool.as_raw_fd(), w.as_raw_fd()).unwrap();
        drop(w);
        let (got, reads) = reader.join().unwrap();
        assert_eq!(got, payload);
        assert_eq!(bytes, payload.len());
        assert!(writes >= 3, "{writes} writes");
        if capacity >= FLUSH_CHUNK {
            assert_eq!(writes, 3, "one write per chunk");
            assert_eq!(reads, vec![FLUSH_CHUNK; 3], "each chunk arrives whole");
        }
    }

    #[test]
    #[serial]
    fn copy_spool_fails_closed_when_the_reader_is_gone() {
        // SAFETY: the disposition `std` already set for this test process.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };
        let spool = spool();
        fill(spool.as_raw_fd(), 4096);
        let (r, w) = pipe();
        drop(r);
        let err = copy_spool(spool.as_raw_fd(), w.as_raw_fd()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe, "{err}");
    }

    #[test]
    fn copy_spool_accepts_an_empty_spool() {
        let spool = spool();
        let (mut r, w) = pipe();
        assert_eq!(copy_spool(spool.as_raw_fd(), w.as_raw_fd()).unwrap(), (0, 0));
        drop(w);
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert!(got.is_empty());
    }

    #[test]
    fn temp_file_fallback_is_unlinked_and_seekable() {
        // SAFETY: `tempfile_spool` returns a fresh descriptor the `File` owns alone.
        let spool = unsafe { File::from_raw_fd(tempfile_spool().unwrap()) };
        let path = std::env::temp_dir().join(format!("loom-list-spool-{}.tmp", std::process::id()));
        assert!(!path.exists(), "{} survived", path.display());
        let payload = fill(spool.as_raw_fd(), 8192);
        let (mut r, w) = pipe();
        assert_eq!(copy_spool(spool.as_raw_fd(), w.as_raw_fd()).unwrap(), (8192, 1));
        drop(w);
        let mut got = Vec::new();
        r.read_to_end(&mut got).unwrap();
        assert_eq!(got, payload);
    }

    /// nextest's shape: stdout a pipe, stderr captured, the reader a
    /// line-splitting `BufReader`. The bytes must be exactly what a file
    /// redirect receives.
    #[test]
    fn piped_listing_is_byte_identical_to_the_file_listing() {
        let expected = file_listing();
        assert!(expected.len() > DEFAULT_PIPE_CAPACITY, "{} bytes", expected.len());
        let out = list_command(|c| {
            c.stderr(Stdio::piped());
        })
        .output()
        .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.stdout, expected);
    }

    /// The reader starts only after the writer is already flushing, so the
    /// listing (larger than any unprivileged pipe) is under backpressure.
    #[test]
    fn piped_listing_arrives_in_bulk_through_an_enlarged_pipe() {
        let expected = file_listing();
        let mut child = list_command(|c| {
            c.stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env(TRACE_ENV, "1");
        })
        .spawn()
        .unwrap();
        let stdout = child.stdout.take().unwrap();
        thread::sleep(Duration::from_millis(250));
        let capacity = pipe_capacity(stdout.as_raw_fd()).unwrap();
        if expected.len() > capacity {
            assert_eq!(child.try_wait().unwrap(), None, "finished without a reader");
        }
        let (got, reads, _) = drain(stdout);
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(got, expected);

        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(MESSAGE_PREFIX), "{stderr}");
        if pipe_max_size() > DEFAULT_PIPE_CAPACITY {
            assert!(capacity > DEFAULT_PIPE_CAPACITY, "pipe still {capacity} bytes");
        }
        if capacity >= FLUSH_CHUNK {
            // The writer holds the pipe across each whole chunk, so a reader
            // with a chunk-sized buffer sees one bulk read per chunk.
            let bulk: Vec<usize> = expected.chunks(FLUSH_CHUNK).map(<[u8]>::len).collect();
            assert_eq!(reads, bulk, "reads: {reads:?}");
        } else if capacity >= 256 * 1024 {
            assert!(reads[0] >= 256 * 1024, "reads: {reads:?}");
        }
    }

    #[test]
    fn a_reader_that_vanished_fails_the_listing_with_101() {
        let mut child = list_command(|c| {
            c.stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .env(TRACE_ENV, "1");
        })
        .spawn()
        .unwrap();
        drop(child.stdout.take().unwrap());
        let out = child.wait_with_output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(101), "{stderr}");
        assert!(stderr.contains(MESSAGE_PREFIX), "{stderr}");
        assert!(stderr.contains("Broken pipe"), "{stderr}");
    }

    #[test]
    fn an_empty_listing_exits_cleanly() {
        let out = list_command(|c| {
            c.arg("--exact")
                .arg("list_pipe_spool::tests::no-such-test")
                .stderr(Stdio::piped())
                .env(TRACE_ENV, "1");
        })
        .output()
        .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(out.stdout.is_empty(), "{:?}", out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("0 bytes -> stdout pipe"), "{stderr}");
    }

    /// The spool stays out of the way: stdout not a pipe, `--list` after the
    /// `--` separator, and an ordinary test run writing to a pipe.
    #[test]
    fn anything_but_a_list_run_to_a_pipe_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = file_listing();

        let path = tmp.path().join("list.txt");
        let out = list_command(|c| {
            c.stdout(Stdio::from(File::create(&path).unwrap()))
                .stderr(Stdio::piped())
                .env(TRACE_ENV, "1");
        })
        .output()
        .unwrap();
        assert!(out.status.success());
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains(MESSAGE_PREFIX),
            "a regular file was spooled"
        );
        assert_eq!(std::fs::read(&path).unwrap(), expected);

        let out = list_command(|c| {
            c.stdout(Stdio::null())
                .stderr(Stdio::piped())
                .env(TRACE_ENV, "1");
        })
        .output()
        .unwrap();
        assert!(out.status.success());
        assert!(out.stderr.is_empty(), "/dev/null was spooled");

        // `--list` after the separator is a test-name filter, not the flag.
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--format", "terse", "--", "--list"])
            .env(TRACE_ENV, "1")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains(MESSAGE_PREFIX),
            "`-- --list` was spooled"
        );

        let out = list_command(|c| {
            c.env(TRACE_ENV, "1")
                .env(DISABLE_ENV, "1")
                .stderr(Stdio::piped());
        })
        .output()
        .unwrap();
        assert!(out.status.success());
        assert_eq!(out.stdout, expected);
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains(MESSAGE_PREFIX),
            "the kill switch was ignored"
        );
    }

    /// Fixture, run only in the child of [`a_test_run_keeps_stdout_on_the_pipe`].
    #[test]
    #[ignore = "fixture: run only by a_test_run_keeps_stdout_on_the_pipe"]
    fn stdout_on_the_pipe_fixture() {
        if std::env::var_os(FIXTURE_ENV).is_none() {
            return;
        }
        assert!(
            is_fifo(libc::STDOUT_FILENO),
            "an ordinary test run had its stdout taken off the pipe"
        );
    }

    #[test]
    fn a_test_run_keeps_stdout_on_the_pipe() {
        // `module_path!()` names the crate first; test names start at its first module.
        let module = module_path!().split_once("::").unwrap().1;
        let out = Command::new(std::env::current_exe().unwrap())
            .arg(format!("{module}::stdout_on_the_pipe_fixture"))
            .args(["--exact", "--ignored"])
            .env(FIXTURE_ENV, "1")
            .env(TRACE_ENV, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stdout.contains("1 passed"), "stdout: {stdout}\nstderr: {stderr}");
        assert!(!stderr.contains(MESSAGE_PREFIX), "{stderr}");
    }
}
