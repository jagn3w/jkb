//! Reading and writing a synced file without following a symbolic link anywhere on its path.
//!
//! **Why.** `jkb sync --watch` runs on the host and writes a bound file when the knowledge base
//! changes, and since tasks S6.2 a dev container can cause such a change through `jkb serve`. The
//! container can also write inside the host directories it binds (`~/repos`): replacing a bound
//! `tasks.md` — or a directory above it — with a symlink to `~/.zshrc` turned the next sync of a
//! container's task edit into a write of the container's text to that file (stage-6.2 review). A
//! check of the path's spelling cannot see a link, and a check followed by a write races one.
//!
//! **How.** The path is walked from `/` one component at a time, each opened relative to the last
//! with `O_NOFOLLOW`, so a link anywhere is refused at the moment it is met rather than checked
//! beforehand. A read opens the file itself with `O_NOFOLLOW | O_NONBLOCK` and refuses anything but a
//! regular file (a FIFO would hang the watcher). A write goes to a fresh temporary file in the same
//! directory and is `renameat`-ed over the name, which replaces whatever is there — a link included —
//! rather than writing through it, and keeps an existing file's permission bits.
//!
//! **What this asks of a path:** that it be absolute and have no link in it at all. A mount's
//! directory is stored canonical (`jkb mount create` canonicalizes it), so its files qualify; a path
//! through a link — on macOS, anything under `/var` or `/tmp` spelled without `/private` — is refused,
//! naming the link.

use std::io;
use std::path::Path;

/// Why a path was refused, as an I/O error of kind `InvalidInput`.
fn refusal(path: &Path, at: &Path, why: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!(
            "refusing {}: {} {why} — sync does not follow symbolic links; if the directory really \
             moved, re-create its mount at the real path",
            path.display(),
            at.display()
        ),
    )
}

/// The file's bytes, or `None` when it does not exist. Refuses a path with a symlink anywhere in it,
/// and a final component that is not a regular file.
///
/// # Errors
/// A refusal (`InvalidInput`), or any other I/O failure.
pub fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
    imp::read(path)
}

/// Write `bytes` to `path` — creating missing directories, never through a symlink — by writing a
/// temporary file beside it and renaming it into place.
///
/// # Errors
/// A refusal (`InvalidInput`), or any other I/O failure.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    imp::write(path, bytes)
}

#[cfg(unix)]
mod imp {
    use std::io::{self, Read as _, Write as _};
    use std::os::fd::OwnedFd;
    use std::path::{Component, Path};

    use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    use super::refusal;

    /// The directory `dir`, walked from `/` without following a link; with `create`, missing
    /// directories are made on the way. `Ok(None)` when it does not exist and `create` is false.
    fn open_dir(path: &Path, dir: &Path, create: bool) -> io::Result<Option<OwnedFd>> {
        if !dir.is_absolute() {
            return Err(refusal(path, dir, "is not an absolute path"));
        }
        let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC;
        let mut fd = fs::open("/", flags, Mode::empty())?;
        let mut walked = std::path::PathBuf::from("/");
        for component in dir.components() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => return Err(refusal(path, dir, "has a `.` or `..` component")),
            };
            walked.push(name);
            let next = match fs::openat(&fd, name, flags, Mode::empty()) {
                Ok(next) => next,
                Err(Errno::NOENT) if create => {
                    match fs::mkdirat(&fd, name, Mode::from_raw_mode(0o755)) {
                        Ok(()) | Err(Errno::EXIST) => {}
                        Err(e) => return Err(e.into()),
                    }
                    fs::openat(&fd, name, flags, Mode::empty())
                        .map_err(|e| judge(path, &walked, e))?
                }
                Err(Errno::NOENT) => return Ok(None),
                Err(e) => return Err(judge(path, &walked, e)),
            };
            fd = next;
        }
        Ok(Some(fd))
    }

    /// An `openat` failure at `at`: a link (`ELOOP`) or a non-directory on the way (`ENOTDIR`, which
    /// some systems give for a link to one) is a refusal; anything else is the error it is.
    fn judge(path: &Path, at: &Path, e: Errno) -> io::Error {
        match e {
            Errno::LOOP | Errno::NOTDIR => {
                refusal(path, at, "is a symbolic link, or not a directory")
            }
            other => other.into(),
        }
    }

    fn split(path: &Path) -> io::Result<(&Path, &std::ffi::OsStr)> {
        match (path.parent(), path.components().next_back()) {
            (Some(parent), Some(Component::Normal(name))) => Ok((parent, name)),
            _ => Err(refusal(path, path, "names no file")),
        }
    }

    pub(super) fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
        let (parent, name) = split(path)?;
        let Some(dir) = open_dir(path, parent, false)? else {
            return Ok(None);
        };
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let fd = match fs::openat(&dir, name, flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(e) => return Err(judge(path, path, e)),
        };
        if FileType::from_raw_mode(fs::fstat(&fd)?.st_mode) != FileType::RegularFile {
            return Err(refusal(path, path, "is not a regular file"));
        }
        let mut bytes = Vec::new();
        std::fs::File::from(fd).read_to_end(&mut bytes)?;
        Ok(Some(bytes))
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let (parent, name) = split(path)?;
        let dir = open_dir(path, parent, true)?
            .ok_or_else(|| refusal(path, parent, "cannot be created"))?;
        // Keep an existing regular file's permission bits; refuse to replace anything else.
        let mode = match fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => Mode::from_raw_mode(stat.st_mode & 0o7777),
                FileType::Symlink => {
                    return Err(refusal(path, path, "is a symbolic link"));
                }
                _ => return Err(refusal(path, path, "is not a regular file")),
            },
            Err(Errno::NOENT) => Mode::from_raw_mode(0o644),
            Err(e) => return Err(e.into()),
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let mut temp_name = std::ffi::OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".jkb-sync-{}-{nanos}.tmp", std::process::id()));
        let flags =
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = fs::openat(&dir, &temp_name, flags, mode)?;
        let written = (|| -> io::Result<()> {
            // The umask narrowed the create; the mode asked for is the existing file's.
            fs::fchmod(&fd, mode)?;
            let mut file = std::fs::File::from(fd);
            file.write_all(bytes)?;
            file.flush()?;
            fs::renameat(&dir, &temp_name, &dir, name)?;
            Ok(())
        })();
        if written.is_err() {
            let _ = fs::unlinkat(&dir, &temp_name, AtFlags::empty());
        }
        written
    }
}

#[cfg(not(unix))]
mod imp {
    use std::io;
    use std::path::Path;

    /// Off Unix there is no dev container kernel binding host directories; plain I/O.
    pub(super) fn read(path: &Path) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, bytes)
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;

    use super::{read, write};

    fn real_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        (dir, real)
    }

    #[test]
    fn a_plain_file_reads_and_writes_creating_its_directories_and_keeping_its_mode() {
        let (_keep, root) = real_tempdir();
        let path = root.join("a/b/tasks.md");
        assert_eq!(read(&path).unwrap(), None, "absent reads as None");
        write(&path, b"one").unwrap();
        assert_eq!(read(&path).unwrap().as_deref(), Some(&b"one"[..]));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(root.join("a/b")).unwrap().count(),
            1,
            "no temporary file left behind"
        );
    }

    #[test]
    fn a_link_at_the_file_or_any_directory_above_it_is_refused_and_nothing_outside_changes() {
        let (_keep, root) = real_tempdir();
        let outside = root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("zshrc"), b"precious").unwrap();
        let mount = root.join("repos/proj");
        std::fs::create_dir_all(&mount).unwrap();

        // The file itself replaced by a link.
        symlink(outside.join("zshrc"), mount.join("tasks.md")).unwrap();
        let path = mount.join("tasks.md");
        assert_eq!(
            read(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            write(&path, b"curl | sh").unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(std::fs::read(outside.join("zshrc")).unwrap(), b"precious");

        // A directory above it replaced by a link.
        symlink(&outside, mount.join("sub")).unwrap();
        let through = mount.join("sub/zshrc");
        assert!(read(&through).is_err());
        assert!(write(&through, b"curl | sh").is_err());
        assert_eq!(std::fs::read(outside.join("zshrc")).unwrap(), b"precious");

        // A dangling link, which a plain write would have created a file through.
        symlink(outside.join("new-rc"), mount.join("dangling.md")).unwrap();
        assert!(write(&mount.join("dangling.md"), b"x").is_err());
        assert!(!outside.join("new-rc").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_fifo_is_refused_rather_than_read() {
        let (_keep, root) = real_tempdir();
        let fifo = root.join("tasks.md");
        rustix::fs::mknodat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::FileType::Fifo,
            rustix::fs::Mode::from_raw_mode(0o644),
            0,
        )
        .unwrap();
        assert_eq!(
            read(&fifo).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput,
            "refused, and not blocked on"
        );
    }
}
