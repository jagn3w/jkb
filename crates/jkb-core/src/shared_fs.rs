//! Refuse to open a database on a filesystem shared with another kernel (design r3.2 H1).
//!
//! `SQLite`'s WAL mode needs every process using a database to share two operating-system
//! facilities: POSIX advisory locks on the database and its `-shm` file, and a `MAP_SHARED`
//! mapping of `-shm` (the wal-index). Across the dev container's bind mount neither holds.
//! Measured with `.container/sqlite-share-probe.py`, one process on each kernel (virtiofs, Docker
//! Desktop on macOS, 2026-09-13): a lock taken in the container was invisible on macOS and could
//! be taken there too, a counter written 2,514 times through the mapping showed 2 distinct values
//! on the other side, and four writers per side corrupted a scratch database within 38 commits.
//! A rollback journal relies on the same locks, and a *reader* is not safe either: jkb opens
//! read-write, and closing what a connection believes is the last WAL connection checkpoints and
//! truncates a WAL the other side is still writing.
//!
//! So the rule lives here, at the one place a database file is opened, rather than in the
//! container's environment or in each caller: an open that would put a database on such a
//! filesystem is refused, whatever `JKB_DB` or `--db` says.
//!
//! **Linux only, and that is the argument rather than a gap.** The side that must never open the
//! host's database is the Linux container; the host's `~/.jkb` is a local disk.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// `statfs` magic numbers of filesystems a database must not live on, with a name for the error.
///
/// FUSE is the measured one: inside the dev container both host binds report
/// `FUSE_SUPER_MAGIC` (`stat -f -c %t` → `65735546`). The rest are the other ways a directory ends
/// up shared with a different kernel — 9p (colima, podman machine), NFS, and SMB — none of which
/// shares advisory locks and a page cache with every process that can open the file.
const SHARED: &[(u32, &str)] = &[
    (0x6573_5546, "FUSE (virtiofs, gRPC-FUSE, sshfs)"),
    (0x0102_1997, "9p"),
    (0x0000_6969, "NFS"),
    (0xFE53_4D42, "SMB2"),
    (0xFF53_4D42, "CIFS"),
];

/// The filesystem name if `f_type` is one a database must not live on.
fn shared_kind(f_type: u32) -> Option<&'static str> {
    SHARED
        .iter()
        .find(|&&(magic, _)| magic == f_type)
        .map(|&(_, name)| name)
}

/// The directory whose filesystem the database's files will actually live on.
///
/// Not the path as given: `SQLite` resolves a symlinked database to its target and creates
/// `-wal`/`-shm` beside the *target*, so a link in a local directory pointing into a shared one
/// must be judged by where it points. And a fresh `--db` path has no file — nor perhaps a parent
/// — yet, so for a path that does not exist the nearest existing ancestor is asked, which is where
/// the directories would be created.
fn directory_to_ask(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved
            .parent()
            .map_or_else(|| resolved.clone(), Path::to_path_buf);
    }
    let mut dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&dir) {
            return resolved;
        }
        match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => dir = p.to_path_buf(),
            _ => return PathBuf::from("."),
        }
    }
}

/// Refuse a database path on a filesystem shared with another kernel.
///
/// # Errors
/// [`Error::SharedFilesystem`] when the directory holding the database is on one of the refused
/// filesystems, and [`Error::FilesystemUnknown`] when that cannot be established — an unreadable
/// answer is not spelled as "local".
#[cfg(target_os = "linux")]
pub(crate) fn refuse(path: &Path) -> Result<()> {
    let dir = directory_to_ask(path);
    let stat = rustix::fs::statfs(&dir).map_err(|e| Error::FilesystemUnknown {
        path: dir.clone(),
        reason: e.to_string(),
    })?;
    // `f_type` is a kernel long: i64 on 64-bit targets, i32 on 32-bit ones, and the SMB magics
    // do not fit in an i32. The magic is the low 32 bits either way. The widening is a no-op on
    // 64-bit, which is what the lint sees; it is not one on the 32-bit targets.
    #[allow(clippy::useless_conversion)]
    let wide = i64::from(stat.f_type);
    // Cannot fail after the mask, and a failure is still not spelled as "local".
    let Ok(f_type) = u32::try_from(wide & 0xFFFF_FFFF) else {
        return Err(Error::FilesystemUnknown {
            path: dir,
            reason: format!("f_type {wide:#x} is out of range"),
        });
    };
    match shared_kind(f_type) {
        Some(kind) => Err(Error::SharedFilesystem { path: dir, kind }),
        None => Ok(()),
    }
}

/// Off Linux there is no container kernel to be on the wrong side of.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn refuse(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{directory_to_ask, shared_kind, SHARED};

    #[test]
    fn the_measured_fuse_magic_and_the_other_shared_filesystems_are_refused() {
        // The value `stat -f -c %t` printed for the container's ~/.jkb and ~/repos binds.
        assert_eq!(
            shared_kind(0x6573_5546),
            Some("FUSE (virtiofs, gRPC-FUSE, sshfs)")
        );
        for &(magic, name) in SHARED {
            assert_eq!(shared_kind(magic), Some(name));
        }
    }

    #[test]
    fn local_filesystems_are_not() {
        // ext4, overlayfs, tmpfs, btrfs, xfs, apfs-in-a-VM never reports these.
        for magic in [0xEF53, 0x794C_7630, 0x0102_1994, 0x9123_683E, 0x5846_5342] {
            assert_eq!(shared_kind(magic), None, "{magic:#x}");
        }
    }

    #[test]
    fn a_fresh_path_is_judged_by_its_nearest_existing_ancestor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fresh = tmp.path().join("not/yet/created/jkb.db");
        assert_eq!(
            directory_to_ask(&fresh),
            std::fs::canonicalize(tmp.path()).unwrap()
        );
    }

    #[test]
    fn an_existing_database_is_judged_by_the_directory_it_resolves_into() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real_dir = tmp.path().join("real");
        let link_dir = tmp.path().join("links");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let target = real_dir.join("jkb.db");
        std::fs::write(&target, b"").unwrap();
        let link = link_dir.join("jkb.db");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(unix)]
        assert_eq!(
            directory_to_ask(&link),
            std::fs::canonicalize(&real_dir).unwrap(),
            "SQLite puts -wal/-shm beside the link's target, so that is the directory to ask"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_database_in_a_local_temp_directory_opens() {
        let tmp = tempfile::TempDir::new().unwrap();
        super::refuse(&tmp.path().join("jkb.db")).expect("a temp dir is not a shared filesystem");
    }
}
