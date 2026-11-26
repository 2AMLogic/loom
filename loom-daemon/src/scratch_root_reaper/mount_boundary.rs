//! Check every entry before reporting bytes. During deletion, pin each
//! directory by descriptor and recheck its mount before changing anything in it.

use std::io;
use std::path::Path;

#[cfg(unix)]
use rustix::{
    fd::AsFd,
    fs::{self, AtFlags, Mode, OFlags},
};

pub(super) const UNKNOWN: &str = "mount boundary could not be verified";
const NESTED: &str = "root contains a mount point";

#[cfg(target_os = "linux")]
fn mount_id_at(dir: impl AsFd, path: &Path) -> io::Result<u64> {
    use fs::StatxFlags;
    // A bind mount can have the same st_dev as its parent, even for a file.
    let flags = AtFlags::SYMLINK_NOFOLLOW | AtFlags::NO_AUTOMOUNT;
    let stat = fs::statx(dir, path, flags, StatxFlags::MNT_ID)?;
    (stat.stx_mask & StatxFlags::MNT_ID.bits() != 0)
        .then_some(stat.stx_mnt_id)
        .ok_or_else(|| io::Error::other(UNKNOWN))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn mount_id_at(dir: impl AsFd, path: &Path) -> io::Result<u64> {
    Ok(fs::statat(dir, path, AtFlags::SYMLINK_NOFOLLOW)?.st_dev as u64)
}

#[cfg(unix)]
pub(super) fn mount_id(path: &Path) -> Option<u64> {
    mount_id_at(fs::CWD, path).ok()
}

#[cfg(not(unix))]
pub(super) fn mount_id(_: &Path) -> Option<u64> {
    None
}

pub(super) fn tree_size(
    root: &Path,
    mount: u64,
    mount_id_of: &dyn Fn(&Path) -> Option<u64>,
) -> Result<u64, &'static str> {
    let mut bytes = 0u64;
    let mut dirs = vec![root.to_owned()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(dir).map_err(|_| UNKNOWN)? {
            let entry = entry.map_err(|_| UNKNOWN)?;
            let path = entry.path();
            if mount_id_of(&path).ok_or(UNKNOWN)? != mount {
                return Err(NESTED);
            }
            // DirEntry::metadata reports the symlink itself.
            let meta = entry.metadata().map_err(|_| UNKNOWN)?;
            if meta.is_dir() {
                dirs.push(path);
            } else {
                bytes = bytes.saturating_add(meta.len());
            }
        }
    }
    Ok(bytes)
}

#[cfg(unix)]
const OPEN_DIR: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

#[cfg(unix)]
pub(super) fn remove_root(path: &Path) -> io::Result<()> {
    let parent =
        fs::open(path.parent().ok_or(io::ErrorKind::InvalidInput)?, OPEN_DIR, Mode::empty())?;
    let name = path.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let mount = mount_id_at(&parent, Path::new("."))?;
    remove_directory(&parent, Path::new(name), mount)
}

#[cfg(unix)]
fn remove_directory(parent: impl AsFd, name: &Path, mount: u64) -> io::Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let dir = fs::openat(&parent, name, OPEN_DIR, Mode::empty())?;
    if mount_id_at(&dir, Path::new("."))? != mount {
        return Err(io::Error::other(NESTED));
    }
    // Unlinking a read-only file requires write access to its directory.
    // Changing the file's own mode could also affect a hard link elsewhere.
    let mode = Mode::from_raw_mode(fs::fstat(&dir)?.st_mode);
    if !mode.contains(Mode::WUSR) {
        fs::fchmod(&dir, mode | Mode::WUSR)?;
    }
    for entry in fs::Dir::read_from(&dir)? {
        let entry = entry?;
        let bytes = entry.file_name().to_bytes();
        if matches!(bytes, b"." | b"..") {
            continue;
        }
        let child = Path::new(OsStr::from_bytes(bytes));
        let stat = fs::statat(&dir, child, AtFlags::SYMLINK_NOFOLLOW)?;
        if fs::FileType::from_raw_mode(stat.st_mode) == fs::FileType::Directory {
            remove_directory(&dir, child, mount)?;
        } else {
            // unlinkat never follows a symlink and refuses a mounted file.
            fs::unlinkat(&dir, child, AtFlags::empty())?;
        }
    }
    fs::unlinkat(parent, name, AtFlags::REMOVEDIR)?;
    Ok(())
}

#[cfg(not(unix))]
pub(super) fn remove_root(_: &Path) -> io::Result<()> {
    Err(io::Error::other(UNKNOWN))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn live_removal_rechecks_the_open_directory_before_chmod_or_unlink() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        std::fs::create_dir(&cache).unwrap();
        std::fs::write(cache.join("data"), b"preserve").unwrap();
        let read_only = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&cache, read_only).unwrap();
        let parent = fs::open(tmp.path(), OPEN_DIR, Mode::empty()).unwrap();
        let different_mount = mount_id(&cache).unwrap() ^ 1;
        let result = remove_directory(&parent, Path::new("cache"), different_mount);
        assert_eq!(result.unwrap_err().to_string(), NESTED);
        assert_eq!(cache.metadata().unwrap().permissions().mode() & 0o777, 0o555);
        assert_eq!(std::fs::read(cache.join("data")).unwrap(), b"preserve");
        std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}
