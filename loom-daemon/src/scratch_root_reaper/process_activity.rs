//! A scratch root can be reclaimed only after a complete process check.
//! Other worktree-cleaning callers use best-effort probes with different semantics.

use std::path::Path;

#[cfg(unix)]
pub(super) fn pid_alive(pid: u32) -> Option<bool> {
    let pid = i32::try_from(pid).ok().filter(|p| *p > 0)?;
    // Signal zero checks existence without delivering a signal.
    if crate::sweep_registry::libc_kill(pid, 0) == 0 {
        return Some(true);
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) => Some(false),
        Some(libc::EPERM) => Some(true),
        _ => None,
    }
}

#[cfg(not(unix))]
pub(super) fn pid_alive(_: u32) -> Option<bool> {
    None
}

#[cfg(target_os = "linux")]
pub(super) fn directory_in_use(root: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;
    // linux/nsfs.h reserves this inode for the initial PID namespace. A
    // container's procfs cannot account for host users of a shared volume.
    const PID_NS_INIT_INO: u64 = 0xEFFF_FFFC;
    let namespace = std::fs::metadata("/proc/1/ns/pid").ok()?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    if namespace.ino() != PID_NS_INIT_INO || !unrestricted_proc_mount(&mounts) {
        return None;
    }
    scan_proc(Path::new("/proc"), root)
}

#[cfg(target_os = "linux")]
fn unrestricted_proc_mount(mounts: &str) -> bool {
    // hidepid may omit PIDs from readdir without returning any error.
    let mut proc_mounts = mounts.lines().filter_map(|line| {
        let (mount, fs) = line.split_once(" - ")?;
        (mount.split_whitespace().nth(4)? == "/proc").then_some(fs)
    });
    let Some(fs) = proc_mounts.next() else {
        return false;
    };
    let mut fields = fs.split_whitespace();
    proc_mounts.next().is_none()
        && fields.next() == Some("proc")
        && fields.nth(1).is_some_and(|options| {
            options
                .split(',')
                .all(|o| !o.starts_with("hidepid=") || matches!(o, "hidepid=0" | "hidepid=off"))
        })
}

#[cfg(target_os = "linux")]
pub(super) fn scan_proc(procfs: &Path, root: &Path) -> Option<bool> {
    use std::io::ErrorKind::NotFound;
    let mut saw_process = false;
    for entry in std::fs::read_dir(procfs).ok()? {
        let entry = entry.ok()?;
        if entry.file_name().to_str()?.parse::<u32>().is_err() {
            continue;
        }
        saw_process = true;
        let process = entry.path();
        match process_uses_root(&process, root) {
            Ok(true) => return Some(true),
            Ok(false) => {}
            Err(e) if e.kind() == NotFound && matches!(process.try_exists(), Ok(false)) => {}
            Err(_) => return None,
        }
    }
    saw_process.then_some(false)
}

#[cfg(target_os = "linux")]
fn process_uses_root(process: &Path, root: &Path) -> std::io::Result<bool> {
    match std::fs::read_link(process.join("cwd")) {
        Ok(cwd) if cwd.starts_with(root) => return Ok(true),
        Ok(_) => {}
        // An exited child awaiting its parent's wait() keeps its /proc entry
        // but has no cwd or descriptors. Skip it only when every task of the
        // thread group is confirmed dead; any live or unreadable task keeps
        // the scan inconclusive.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return if thread_group_exited(process)? {
                Ok(false)
            } else {
                Err(e)
            };
        }
        Err(e) => return Err(e),
    }
    // The executable image need not appear in the numbered descriptors.
    let kernel_thread = match std::fs::read_link(process.join("exe")) {
        Ok(exe) if exe.starts_with(root) => return Ok(true),
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Kernel threads have no userspace executable. Still check their
            // descriptors, and preserve on a missing image for any other task.
            let status = std::fs::read_to_string(process.join("status"))?;
            if !status.lines().any(|line| line == "Kthread:\t1") {
                return Err(e);
            }
            true
        }
        Err(e) => return Err(e),
    };
    for fd in std::fs::read_dir(process.join("fd"))? {
        match std::fs::read_link(fd?.path()) {
            Ok(target) if target.starts_with(root) => return Ok(true),
            Ok(_) => {}
            // Descriptors can close between readdir and readlink.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    if kernel_thread {
        return Ok(false); // No userspace address space.
    }
    mapped_file_under_root(process, root)
}

#[cfg(target_os = "linux")]
fn mapped_file_under_root(process: &Path, root: &Path) -> std::io::Result<bool> {
    use std::ffi::OsStr;
    use std::io::{BufRead, BufReader, Error, ErrorKind::InvalidData};
    use std::os::unix::ffi::OsStrExt;

    // procfs renders a newline in a pathname as a literal backslash-012.
    // If that ambiguity occurs in the root itself we cannot prove absence.
    if root.as_os_str().as_bytes().contains(&b'\n') {
        return Err(Error::from(InvalidData));
    }
    let mut maps = BufReader::new(std::fs::File::open(process.join("maps"))?);
    let mut line = Vec::new();
    let mut saw_mapping = false;
    loop {
        line.clear();
        if maps.read_until(b'\n', &mut line)? == 0 {
            return if saw_mapping {
                Ok(false)
            } else {
                Err(Error::from(InvalidData))
            };
        }
        let mut rest = line.strip_suffix(b"\n").ok_or(InvalidData)?;
        // Skip address, permissions, offset and device. Only the columns
        // before the pathname are space-delimited; filenames can contain
        // spaces, tabs and non-UTF-8 bytes.
        for _ in 0..4 {
            let end = rest
                .iter()
                .position(|&c| c == b' ')
                .filter(|&i| i > 0)
                .ok_or(InvalidData)?;
            rest = rest[end..].trim_ascii_start();
        }
        let end = rest.iter().position(|&c| c == b' ').unwrap_or(rest.len());
        let inode = std::str::from_utf8(&rest[..end])
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or(InvalidData)?;
        let path = rest[end..].trim_ascii_start();
        if path.starts_with(b"/") {
            if Path::new(OsStr::from_bytes(path)).starts_with(root) {
                return Ok(true);
            }
        } else if !(inode == 0 && path.is_empty() || path.starts_with(b"[") && path.ends_with(b"]"))
        {
            return Err(Error::from(InvalidData));
        }
        saw_mapping = true;
    }
}

/// True when every task in the process is a zombie or dead; errors when any
/// task's state cannot be read.
#[cfg(target_os = "linux")]
fn thread_group_exited(process: &Path) -> std::io::Result<bool> {
    let mut any = false;
    for task in std::fs::read_dir(process.join("task"))? {
        let stat = std::fs::read_to_string(task?.path().join("stat"))?;
        // The command name may contain spaces and parentheses; the state
        // follows the last ')'.
        let state = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.trim_start().chars().next())
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidData))?;
        if !matches!(state, 'Z' | 'X' | 'x') {
            return Ok(false);
        }
        any = true;
    }
    Ok(any)
}

#[cfg(target_os = "macos")]
pub(super) fn directory_in_use(root: &Path) -> Option<bool> {
    use std::process::Command;
    // A path-filtered lsof can silently skip inaccessible processes and their
    // "err" descriptors. Check the full field listing against the process table.
    let listing = |program, args: &[&str]| {
        let mut cmd = Command::new(program);
        cmd.args(args);
        let out = crate::proc_exec::run_bounded(cmd, std::time::Duration::from_secs(30))
            .ok()?
            .output()?;
        if !out.status.success() || !out.stderr.is_empty() {
            return None;
        }
        String::from_utf8(out.stdout).ok()
    };
    let table = listing("ps", &["-A", "-o", "pid=,stat="])?;
    let files = listing("lsof", &["+w", "-nP", "-F0pftn"])?;
    check_lsof(root, &table, &files, pid_alive)
}

#[cfg(any(target_os = "macos", test))]
fn check_lsof(
    root: &Path,
    table: &str,
    files: &str,
    alive: impl Fn(u32) -> Option<bool>,
) -> Option<bool> {
    use std::collections::BTreeSet;
    // lsof can escape non-printable bytes in names. Only compare prefixes it
    // prints literally; an unrepresentable root cannot be proven idle here.
    if !root
        .to_str()?
        .bytes()
        .all(|b| (b' '..=b'~').contains(&b) && b != b'\\')
    {
        return None;
    }
    let mut without_cwd = BTreeSet::new();
    let mut with_cwd = BTreeSet::new();
    let mut pid = None;
    for record in files.strip_suffix("\0\n")?.split("\0\n") {
        let mut fields = record.split('\0');
        let header = fields.next()?;
        if let Some(value) = header.strip_prefix('p') {
            let id = value.parse::<u32>().ok()?;
            without_cwd.insert(id);
            pid = Some(id);
            continue;
        }
        let fd = header.strip_prefix('f')?.trim();
        let mut kind = None;
        let mut name = None;
        for field in fields {
            if let Some(t) = field.strip_prefix('t') {
                kind = Some(t);
            }
            if let Some(n) = field.strip_prefix('n') {
                name = Some(Path::new(n));
            }
        }
        if name.is_some_and(|p| p.starts_with(root)) {
            return Some(true);
        }
        if fd == "err" || fd == "NOFD" {
            return None;
        }
        if matches!(fd, "cwd" | "twd") {
            if !name?.is_absolute() {
                return None;
            }
            if fd == "cwd" {
                with_cwd.insert(pid?);
                without_cwd.remove(&pid?);
            }
        } else if fd.parse::<u32>().is_ok() && !name.is_some_and(Path::is_absolute) {
            // These descriptor types do not refer to filesystem vnodes.
            // An absent path on a vnode, or an unknown type, is inconclusive.
            if !matches!(
                kind?,
                "PIPE"
                    | "KQUEUE"
                    | "IPv4"
                    | "IPv6"
                    | "unix"
                    | "systm"
                    | "PSXSEM"
                    | "PSXSHM"
                    | "NPOLICY"
                    | "FSEVENTS"
            ) {
                return None;
            }
        }
    }
    if with_cwd.is_empty() || table.trim().is_empty() {
        return None;
    }
    for line in table.lines() {
        let mut columns = line.split_whitespace();
        let id = columns.next()?.parse::<u32>().ok()?;
        let state = columns.next()?;
        if id == 0 || state.starts_with('Z') {
            without_cwd.remove(&id);
        } else if !with_cwd.contains(&id) {
            without_cwd.insert(id);
        }
    }
    without_cwd
        .into_iter()
        .all(|id| alive(id) == Some(false))
        .then_some(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(super) fn directory_in_use(_: &Path) -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pid_probe_rejects_group_ids_and_recognizes_a_live_process() {
        assert_eq!(pid_alive(0), None);
        assert_eq!(pid_alive(u32::MAX), None);
        assert_eq!(pid_alive(std::process::id()), Some(true));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_mount_must_show_other_users() {
        let mount = "1 2 0:1 / /proc rw - proc proc rw";
        assert!(unrestricted_proc_mount(mount));
        assert!(!unrestricted_proc_mount(&format!("{mount},hidepid=invisible")));
        assert!(!unrestricted_proc_mount(&format!("{mount}\n{mount},hidepid=2")));
        assert!(!unrestricted_proc_mount(""));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_scan_requires_cwd_exe_fds_and_file_mappings_to_be_inspectable() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("candidate");
        let procfs = tmp.path().join("proc");
        assert_eq!(scan_proc(&procfs, &root), None);
        let pid = procfs.join("41");
        std::fs::create_dir_all(&pid).unwrap();
        assert_eq!(scan_proc(&procfs, &root), None);
        symlink(tmp.path(), pid.join("cwd")).unwrap();
        symlink(tmp.path().join("worker"), pid.join("exe")).unwrap();
        assert_eq!(scan_proc(&procfs, &root), None);
        std::fs::create_dir(pid.join("fd")).unwrap();
        assert_eq!(scan_proc(&procfs, &root), None, "missing maps");
        let maps = pid.join("maps");
        for invalid in ["", "truncated", "100-200 r--p 0 00:00 7\n"] {
            std::fs::write(&maps, invalid).unwrap();
            assert_eq!(scan_proc(&procfs, &root), None);
        }
        let outside = format!("100-200 r--p 0 00:00 7 {}-peer/data\n", root.display());
        std::fs::write(&maps, outside).unwrap();
        assert_eq!(scan_proc(&procfs, &root), Some(false));
        let mut mapped = format!("100-200 r--p 0 00:00 7 {}/db ", root.display()).into_bytes();
        mapped.extend_from_slice(b"index\xff (deleted)\n");
        std::fs::write(&maps, mapped).unwrap();
        assert_eq!(scan_proc(&procfs, &root), Some(true));
        std::fs::remove_file(maps).unwrap();
        std::fs::remove_file(pid.join("exe")).unwrap();
        std::fs::write(pid.join("status"), "Kthread:\t0\n").unwrap();
        assert_eq!(scan_proc(&procfs, &root), None);
        std::fs::write(pid.join("status"), "Kthread:\t1\n").unwrap();
        assert_eq!(scan_proc(&procfs, &root), Some(false));
        symlink(root.join("build.log"), pid.join("fd/5")).unwrap();
        assert_eq!(scan_proc(&procfs, &root), Some(true));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn proc_scan_skips_exited_children_but_not_live_or_unreadable_tasks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("candidate");
        let procfs = tmp.path().join("proc");
        // Idle control process with readable cwd, executable and fds.
        let idle = procfs.join("40");
        std::fs::create_dir_all(idle.join("fd")).unwrap();
        symlink(tmp.path(), idle.join("cwd")).unwrap();
        symlink(tmp.path().join("worker"), idle.join("exe")).unwrap();
        std::fs::write(idle.join("maps"), "100-200 rw-p 0 00:00 0 [heap]\n").unwrap();
        // Zombie: no cwd or fd entries, every task in state Z.
        let zombie = procfs.join("41");
        let task = |tid: &str, stat: &str| {
            let dir = zombie.join("task").join(tid);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("stat"), stat).unwrap();
        };
        task("41", "41 (a (b) c) Z 1 41 41 0 -1");
        assert_eq!(scan_proc(&procfs, &root), Some(false));
        // A live sibling thread whose cwd cannot be read is inconclusive.
        task("42", "41 (worker) S 1 41 41 0 -1");
        assert_eq!(scan_proc(&procfs, &root), None);
        // An unreadable task state is inconclusive too.
        std::fs::remove_dir_all(zombie.join("task/42")).unwrap();
        std::fs::create_dir(zombie.join("task/43")).unwrap();
        assert_eq!(scan_proc(&procfs, &root), None);
        // A live file holder is still found next to an unreaped zombie.
        std::fs::remove_dir_all(zombie.join("task/43")).unwrap();
        symlink(root.join("build.log"), idle.join("fd/5")).unwrap();
        assert_eq!(scan_proc(&procfs, &root), Some(true));
    }

    #[test]
    fn lsof_requires_coverage_of_every_live_process_and_readable_descriptors() {
        let root = Path::new("/scratch");
        let files = "p12\0\nfcwd\0tDIR\0n/elsewhere\0\n";
        let check = |table: &str, files: &str| check_lsof(root, table, files, |_| Some(true));
        assert_eq!(check("12 R", files), Some(false));
        assert_eq!(check("12 R\n13 S", files), None);
        assert_eq!(check_lsof(root, "12 R\n13 S", files, |_| Some(false)), Some(false));
        assert_eq!(check("12 R", &format!("{files}ferr\0nFD info error\0\n")), None);
        assert_eq!(check("12 R", &format!("{files}f4\0tREG\0\n")), None);
        assert_eq!(check("12 R", &format!("{files}f4\0tREG\0n/scratch/log\0\n")), Some(true));
    }
}
