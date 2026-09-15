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
//! So the rule lives here, called from the two places jkb creates or opens a database file —
//! `db::open` and `Db::backup` (`VACUUM INTO` writes one) — rather than in the container's
//! environment or in each caller: an open that would put a database, or any of its files, on such
//! a filesystem is refused, whatever `JKB_DB`, `--db` or `--backup` says.
//!
//! **Linux only, and that is the argument rather than a gap.** The side that must never open the
//! host's database is the Linux container; the host's `~/.jkb` is a local disk.
//!
//! Residual, stated: this guards jkb and `scripts/lib.sh`'s `jkb_sqlite`. Any other `SQLite`
//! client run in the container (a Python `sqlite3.connect`, a hand-typed `sqlite3`) is not jkb and
//! is not guarded; what closes that for good is the container not seeing the file at all.

use std::path::{Path, PathBuf};

use crate::{Error, Result};

/// `statfs` magic numbers of filesystems a database must not live on, with a name for the error.
///
/// FUSE is the measured one: inside the dev container both host binds report
/// `FUSE_SUPER_MAGIC` (`stat -f -c %t` → `65735546`). The rest are the other ways a directory ends
/// up shared with a different kernel — 9p (colima, podman machine), NFS, and SMB — none of which
/// shares advisory locks and a page cache with every process that can open the file.
///
/// Every entry is a hex tuple on its own line: `scripts/tests/dev-scripts.test.sh` case11 reads
/// this block to prove `scripts/lib.sh` refuses the same set, and fails on an entry in any other
/// shape.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux statfs guard calls it; its tests run everywhere
const SHARED: &[(u32, &str)] = &[
    (0x6573_5546, "FUSE (virtiofs, gRPC-FUSE, sshfs)"),
    (0x0102_1997, "9p"),
    (0x0000_6969, "NFS"),
    (0xFE53_4D42, "SMB2"),
    (0xFF53_4D42, "CIFS"),
];

/// Symlink hops followed before a path is treated as unresolvable (the kernel's own limit).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux statfs guard calls it; its tests run everywhere
const MAX_LINK_HOPS: usize = 40;

/// The filesystem name if `f_type` is one a database must not live on.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux statfs guard calls it; its tests run everywhere
fn shared_kind(f_type: u32) -> Option<&'static str> {
    SHARED
        .iter()
        .find(|&&(magic, _)| magic == f_type)
        .map(|&(_, name)| name)
}

/// Follow `path` through symlinks — including a DANGLING one, which `SQLite` also follows and
/// creates the database at the far end of — to the path whose directory will hold the files.
/// `None` when the chain does not end within [`MAX_LINK_HOPS`].
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux statfs guard calls it; its tests run everywhere
fn follow_links(path: &Path) -> Option<PathBuf> {
    let mut at = path.to_path_buf();
    for _ in 0..MAX_LINK_HOPS {
        match std::fs::symlink_metadata(&at) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let target = std::fs::read_link(&at).ok()?;
                at = if target.is_absolute() {
                    target
                } else {
                    at.parent().unwrap_or_else(|| Path::new(".")).join(target)
                };
            }
            _ => return Some(at),
        }
    }
    None
}

/// Everything whose filesystem must be judged before `path` is opened as a database.
///
/// - the directory the files will be created in — for a path that does not exist yet, its nearest
///   existing ancestor, which is where the missing directories would be created;
/// - the database file itself when it exists, and its `-wal`/`-shm`/`-journal` siblings: a single
///   file bind-mounted from the host into a local directory lives on the host's filesystem while
///   its directory does not, and `statfs` of the directory cannot see that.
///
/// Symlinks are followed first, dangling ones included. An `Err` carries why the path cannot be
/// judged, which the caller refuses rather than reading as local.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))] // only the Linux statfs guard calls it; its tests run everywhere
fn paths_to_ask(path: &Path) -> std::result::Result<Vec<PathBuf>, String> {
    let target = follow_links(path)
        .ok_or_else(|| format!("its symlinks do not resolve within {MAX_LINK_HOPS} hops"))?;
    let mut asks = Vec::new();
    let mut dir = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    loop {
        if let Ok(resolved) = std::fs::canonicalize(&dir) {
            asks.push(resolved);
            break;
        }
        match dir.parent() {
            Some(p) if !p.as_os_str().is_empty() => dir = p.to_path_buf(),
            _ => {
                asks.push(PathBuf::from("."));
                break;
            }
        }
    }
    let name = target
        .file_name()
        .ok_or_else(|| "it names no file (a directory, `.` or `..`)".to_owned())?
        .to_os_string();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let mut file = name.clone();
        file.push(suffix);
        let candidate = target.with_file_name(file);
        if candidate.exists() {
            asks.push(candidate);
        }
    }
    Ok(asks)
}

/// Refuse a database path that is a URI, or that is on a filesystem shared with another kernel.
///
/// **A `file:` string is refused on every platform, before anything else.** The `SQLite` that
/// `libsqlite3-sys` bundles is compiled with `-DSQLITE_USE_URI`, so it parses `file:` URIs whatever
/// flags the open passes. Measured in the dev container: `jkb --db file:/home/vscode/.jkb/jkb.db
/// ns ls` listed the HOST knowledge base's namespaces and touched its `-shm`, while the statfs
/// guard below judged a relative directory named `file:` in the working directory. jkb has never
/// opened a database by URI, so the only safe reading of one is none.
///
/// # Errors
/// [`Error::UriPath`] for a `file:` string; [`Error::SharedFilesystem`] when the database, one of its
/// files, or the directory that will hold them is on one of the refused filesystems;
/// [`Error::FilesystemUnknown`] when that cannot be established — an unreadable answer is not
/// spelled as "local".
pub(crate) fn refuse(path: &Path) -> Result<()> {
    if path.as_os_str().to_string_lossy().starts_with("file:") {
        return Err(Error::UriPath {
            path: path.to_path_buf(),
        });
    }
    refuse_shared(path)
}

#[cfg(target_os = "linux")]
fn refuse_shared(path: &Path) -> Result<()> {
    let unknown = |at: &Path, reason: String| Error::FilesystemUnknown {
        path: at.to_path_buf(),
        reason,
    };
    let asks = paths_to_ask(path).map_err(|reason| unknown(path, reason))?;
    judge(asks, unknown)
}

/// Judge each of `asks` — the directory first, then the files ([`paths_to_ask`]).
///
/// **A file that is gone by the time it is asked is skipped, not refused.** The files are listed
/// because they existed a moment earlier, and `SQLite` deletes `-wal`/`-shm` when another process
/// closes its last connection — so a host `jkb` opening beside one that was just exiting was refused
/// as "cannot tell what filesystem" (a CLI test met it under a parallel run). Its directory, always
/// asked first, is still judged; only the directory's own disappearance, or any other failure, refuses.
#[cfg(target_os = "linux")]
fn judge(asks: Vec<PathBuf>, unknown: impl Fn(&Path, String) -> Error) -> Result<()> {
    for (i, at) in asks.into_iter().enumerate() {
        let stat = match rustix::fs::statfs(&at) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) if i > 0 => continue,
            Err(e) => return Err(unknown(&at, e.to_string())),
        };
        // `f_type` is a kernel long: i64 on 64-bit targets, i32 on 32-bit ones, and the SMB magics
        // do not fit in an i32. The magic is the low 32 bits either way. The widening is a no-op
        // on 64-bit, which is what the lint sees; it is not one on the 32-bit targets.
        #[allow(clippy::useless_conversion)]
        let wide = i64::from(stat.f_type);
        // Cannot fail after the mask, and a failure is still not spelled as "local".
        let f_type = u32::try_from(wide & 0xFFFF_FFFF)
            .map_err(|_| unknown(&at, format!("f_type {wide:#x} is out of range")))?;
        if let Some(kind) = shared_kind(f_type) {
            return Err(Error::SharedFilesystem { path: at, kind });
        }
    }
    Ok(())
}

/// Off Linux there is no container kernel to be on the wrong side of.
#[cfg(not(target_os = "linux"))]
#[allow(clippy::unnecessary_wraps)]
fn refuse_shared(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{follow_links, paths_to_ask, shared_kind, SHARED};

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
        // ext4, overlayfs, tmpfs, btrfs, xfs.
        for magic in [0xEF53, 0x794C_7630, 0x0102_1994, 0x9123_683E, 0x5846_5342] {
            assert_eq!(shared_kind(magic), None, "{magic:#x}");
        }
    }

    #[test]
    fn a_fresh_path_is_judged_by_its_nearest_existing_ancestor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let fresh = tmp.path().join("not/yet/created/jkb.db");
        assert_eq!(
            paths_to_ask(&fresh).unwrap(),
            vec![std::fs::canonicalize(tmp.path()).unwrap()]
        );
    }

    /// A database file listed and then deleted — another process closing its last connection removes
    /// `-shm` — is skipped; the directory vanishing is still a refusal.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_file_gone_before_it_is_asked_is_skipped_but_its_directory_is_not() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap();
        let unknown = |at: &std::path::Path, reason: String| crate::Error::FilesystemUnknown {
            path: at.to_path_buf(),
            reason,
        };
        let gone = dir.join("jkb.db-shm");
        assert!(super::judge(vec![dir.clone(), gone.clone()], unknown).is_ok());
        assert!(
            matches!(
                super::judge(vec![dir.join("no-such-dir"), gone], unknown),
                Err(crate::Error::FilesystemUnknown { .. })
            ),
            "the directory is the one thing that must answer"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_database_is_judged_by_where_it_resolves_and_by_each_of_its_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let real_dir = tmp.path().join("real");
        let link_dir = tmp.path().join("links");
        std::fs::create_dir_all(&real_dir).unwrap();
        std::fs::create_dir_all(&link_dir).unwrap();
        let target = real_dir.join("jkb.db");
        std::fs::write(&target, b"").unwrap();
        std::fs::write(real_dir.join("jkb.db-wal"), b"").unwrap();
        let link = link_dir.join("jkb.db");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let asks = paths_to_ask(&link).unwrap();
        let real = std::fs::canonicalize(&real_dir).unwrap();
        assert_eq!(
            asks[0], real,
            "SQLite puts -wal/-shm beside the link's target"
        );
        // The file and its WAL are asked about themselves: a single-file bind mount is invisible
        // to statfs of the directory it sits in.
        assert!(asks.iter().any(|p| p.ends_with("real/jkb.db")), "{asks:?}");
        assert!(
            asks.iter().any(|p| p.ends_with("real/jkb.db-wal")),
            "{asks:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_is_judged_by_where_it_points_not_where_it_sits() {
        // SQLite follows a dangling link and creates the database at its target, so judging the
        // link's own (local) directory would let it create one on a shared filesystem.
        let tmp = tempfile::TempDir::new().unwrap();
        let far = tmp.path().join("far/away");
        std::fs::create_dir_all(&far).unwrap();
        let near = tmp.path().join("near");
        std::fs::create_dir_all(&near).unwrap();
        let link = near.join("jkb.db");
        std::os::unix::fs::symlink(far.join("missing.db"), &link).unwrap();

        assert_eq!(follow_links(&link).unwrap(), far.join("missing.db"));
        assert_eq!(
            paths_to_ask(&link).unwrap(),
            vec![std::fs::canonicalize(&far).unwrap()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_unresolvable_not_local() {
        let tmp = tempfile::TempDir::new().unwrap();
        let a = tmp.path().join("a.db");
        let b = tmp.path().join("b.db");
        std::os::unix::fs::symlink(&b, &a).unwrap();
        std::os::unix::fs::symlink(&a, &b).unwrap();
        assert!(paths_to_ask(&a).unwrap_err().contains("symlinks"));
    }

    #[test]
    fn a_path_naming_no_file_says_so_rather_than_blaming_symlinks() {
        let tmp = tempfile::TempDir::new().unwrap();
        let err = paths_to_ask(&tmp.path().join("sub/..")).unwrap_err();
        assert!(err.contains("names no file"), "{err}");
    }

    #[test]
    fn a_uri_is_refused_before_any_filesystem_is_asked() {
        // Relative to a LOCAL working directory the statfs guard would pass it; SQLite would then
        // open the URI's path — the host's database, when run in the container.
        let err = super::refuse(std::path::Path::new(
            "file:/home/vscode/.jkb/jkb.db?mode=rw",
        ))
        .expect_err("a file: URI must never reach SQLite");
        assert!(matches!(err, crate::Error::UriPath { .. }), "{err}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_database_in_a_local_temp_directory_opens() {
        let tmp = tempfile::TempDir::new().unwrap();
        super::refuse(&tmp.path().join("jkb.db")).expect("a temp dir is not a shared filesystem");
    }
}
