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
        format!("refusing {}: {} {why}", path.display(), at.display()),
    )
}

/// A [`refusal`] for a link on the path, with what to do about one. Only a link gets the remedy: a
/// file refused for its size or type sent the user to re-create a mount that was fine.
fn link_refusal(path: &Path, at: &Path, why: &str) -> io::Error {
    refusal(
        path,
        at,
        &format!(
            "{why} — jkb does not follow symbolic links here; if the directory really moved, use \
             its real path (for a synced file, re-create its mount there)"
        ),
    )
}

/// The largest synced file [`read`] reads. A sparse file is instant to make and costs no disk, and a
/// read of one planted at a bound path allocated its whole apparent size on the host's watcher.
pub const MAX_READ_BYTES: u64 = 64 * 1024 * 1024;

/// Whether `name` is one of [`write`]'s temporary files. A write interrupted before its rename leaves
/// one inside the mount, and sync must never import it as a second copy of the file it was replacing.
#[must_use]
pub fn is_temp_name(name: &std::ffi::OsStr) -> bool {
    let name = name.to_string_lossy();
    name.starts_with('.') && name.contains(".jkb-sync-") && name.ends_with(".tmp")
}

/// The file's bytes, or `None` when it does not exist. Refuses a path with a symlink anywhere in it,
/// a final component that is not a regular file, and one over [`MAX_READ_BYTES`].
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

/// Move the directory entry `src` to `dest_dir/dest_name` — `dest_dir` created if missing — never
/// through a symlink on either path. The entry itself is moved as it is, a link included, never what it
/// points at. The worktree-removal sweep's archive step (tasks S6.4 stage 3): a dev container can plant
/// links inside the `~/repos` a record names.
///
/// # Errors
/// A refusal (`InvalidInput`), `NotFound` when `src` is not there, or any other I/O failure.
pub fn rename_into(src: &Path, dest_dir: &Path, dest_name: &std::ffi::OsStr) -> io::Result<()> {
    imp::rename_into(src, dest_dir, dest_name)
}

/// Remove `path` if it is an empty directory, never through a symlink — the probe the sweep asks before
/// a removal: `DirectoryNotEmpty` means an unlink is permitted, `PermissionDenied` that it is not.
///
/// # Errors
/// A refusal (`InvalidInput`), or the I/O failure the removal met.
pub fn remove_empty_dir(path: &Path) -> io::Result<()> {
    imp::remove_empty_dir(path)
}

/// Remove `path` and everything under it, never through a symlink: a link on the way is refused, and a
/// link inside the tree is removed rather than followed. Nothing at `path` is not an error.
///
/// # Errors
/// A refusal (`InvalidInput`), or the first I/O failure met; what was removed before it stays removed.
pub fn remove_tree(path: &Path) -> io::Result<()> {
    imp::remove_tree(path)
}

#[cfg(unix)]
mod imp {
    use std::io::{self, Read as _, Write as _};
    use std::os::fd::OwnedFd;
    use std::path::{Component, Path};

    use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;

    use super::{link_refusal, refusal};

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
                link_refusal(path, at, "is a symbolic link, or not a directory")
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
        let stat = fs::fstat(&fd)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(refusal(path, path, "is not a regular file"));
        }
        let too_big = || {
            refusal(
                path,
                path,
                &format!(
                    "is larger than the {} MiB a synced file may be",
                    super::MAX_READ_BYTES / (1024 * 1024)
                ),
            )
        };
        if u64::try_from(stat.st_size).unwrap_or(u64::MAX) > super::MAX_READ_BYTES {
            return Err(too_big());
        }
        let mut bytes = Vec::new();
        // Bounded again as it is read: the size can grow between the stat and the read.
        std::fs::File::from(fd)
            .take(super::MAX_READ_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > super::MAX_READ_BYTES {
            return Err(too_big());
        }
        Ok(Some(bytes))
    }

    pub(super) fn rename_into(
        src: &Path,
        dest_dir: &Path,
        dest_name: &std::ffi::OsStr,
    ) -> io::Result<()> {
        let (parent, name) = split(src)?;
        let from = open_dir(src, parent, false)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let to = open_dir(dest_dir, dest_dir, true)?
            .ok_or_else(|| refusal(dest_dir, dest_dir, "cannot be created"))?;
        if dest_name.is_empty() || Path::new(dest_name).components().count() != 1 {
            return Err(refusal(
                dest_dir,
                dest_dir,
                "was given a name that is not one component",
            ));
        }
        fs::renameat(&from, name, &to, dest_name)?;
        Ok(())
    }

    pub(super) fn remove_empty_dir(path: &Path) -> io::Result<()> {
        let (parent, name) = split(path)?;
        let dir = open_dir(path, parent, false)?
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        match fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode) != FileType::Directory => {
                return Err(link_refusal(path, path, "is not a directory"));
            }
            Ok(_) => {}
            Err(e) => return Err(e.into()),
        }
        fs::unlinkat(&dir, name, AtFlags::REMOVEDIR)?;
        Ok(())
    }

    pub(super) fn remove_tree(path: &Path) -> io::Result<()> {
        let (parent, name) = split(path)?;
        let Some(dir) = open_dir(path, parent, false)? else {
            return Ok(());
        };
        remove_at(&dir, name)
    }

    /// Remove `name` inside `dir`: a directory by its contents first, anything else — a link
    /// included — by unlinking the entry itself.
    fn remove_at(dir: &OwnedFd, name: &std::ffi::OsStr) -> io::Result<()> {
        use std::os::unix::ffi::OsStrExt as _;
        let stat = match fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(Errno::NOENT) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return match fs::unlinkat(dir, name, AtFlags::empty()) {
                Ok(()) | Err(Errno::NOENT) => Ok(()),
                Err(e) => Err(e.into()),
            };
        }
        let flags = OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC;
        let child = fs::openat(dir, name, flags, Mode::empty())?;
        // Names first, removals after: a directory stream is not read while it is being changed.
        let mut names = Vec::new();
        for entry in fs::Dir::read_from(&child)? {
            let entry = entry?;
            let n = entry.file_name().to_bytes();
            if n != b"." && n != b".." {
                names.push(std::ffi::OsStr::from_bytes(n).to_owned());
            }
        }
        for n in names {
            remove_at(&child, &n)?;
        }
        match fs::unlinkat(dir, name, AtFlags::REMOVEDIR) {
            Ok(()) | Err(Errno::NOENT) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    pub(super) fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
        let (parent, name) = split(path)?;
        let dir = open_dir(path, parent, true)?
            .ok_or_else(|| refusal(path, parent, "cannot be created"))?;
        // An existing regular file keeps its permission bits; a new one gets what the umask leaves of
        // 0o666, as a plain create would. Anything but a regular file is refused.
        let existing = match fs::statat(&dir, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => Some(Mode::from_raw_mode(stat.st_mode & 0o7777)),
                FileType::Symlink => {
                    return Err(link_refusal(path, path, "is a symbolic link"));
                }
                _ => return Err(refusal(path, path, "is not a regular file")),
            },
            Err(Errno::NOENT) => None,
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
        let fd = fs::openat(&dir, &temp_name, flags, Mode::from_raw_mode(0o666))?;
        let written = (|| -> io::Result<()> {
            if let Some(mode) = existing {
                // The umask narrowed the create; a replaced file keeps its own mode.
                fs::fchmod(&fd, mode)?;
            }
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

    pub(super) fn rename_into(
        src: &Path,
        dest_dir: &Path,
        dest_name: &std::ffi::OsStr,
    ) -> io::Result<()> {
        std::fs::create_dir_all(dest_dir)?;
        std::fs::rename(src, dest_dir.join(dest_name))
    }

    pub(super) fn remove_empty_dir(path: &Path) -> io::Result<()> {
        std::fs::remove_dir(path)
    }

    pub(super) fn remove_tree(path: &Path) -> io::Result<()> {
        match std::fs::remove_dir_all(path) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            other => other,
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

    use super::{read, remove_empty_dir, remove_tree, rename_into, write};

    /// The sweep's moves and removals refuse a link on the way, and remove a link inside a tree
    /// rather than what it points at.
    #[test]
    fn moving_and_removing_trees_never_follow_a_link() {
        let (_keep, root) = real_tempdir();
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("d")).unwrap();
        std::fs::write(outside.join("d/keep"), "x").unwrap();

        // A tree holding a link out: the link goes, the target stays.
        let tree = root.join("repo/.jkb/archive/s");
        std::fs::create_dir_all(tree.join("sub")).unwrap();
        std::fs::write(tree.join("sub/f"), "y").unwrap();
        symlink(&outside, tree.join("sub/out")).unwrap();
        assert_eq!(
            remove_empty_dir(&tree).unwrap_err().kind(),
            std::io::ErrorKind::DirectoryNotEmpty
        );
        remove_tree(&tree).unwrap();
        assert!(!tree.exists());
        assert!(outside.join("d/keep").exists(), "nothing followed");
        remove_tree(&tree).unwrap();

        // A link on the way is refused, and nothing behind it is touched.
        std::fs::remove_dir_all(root.join("repo/.jkb/archive")).unwrap();
        symlink(&outside, root.join("repo/.jkb/archive")).unwrap();
        let through = root.join("repo/.jkb/archive/d");
        for e in [
            remove_tree(&through).unwrap_err(),
            remove_empty_dir(&through).unwrap_err(),
            rename_into(&through, &root.join("elsewhere"), "d".as_ref()).unwrap_err(),
        ] {
            assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
        }
        std::fs::create_dir_all(root.join("repo/.jkb/work/w")).unwrap();
        let e = rename_into(&root.join("repo/.jkb/work/w"), &through, "w".as_ref()).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput, "{e}");
        assert!(outside.join("d/keep").exists());
        assert!(!outside.join("w").exists());

        // And a plain move works, creating the destination.
        std::fs::remove_file(root.join("repo/.jkb/archive")).unwrap();
        rename_into(
            &root.join("repo/.jkb/work/w"),
            &root.join("repo/.jkb/archive"),
            "w-1".as_ref(),
        )
        .unwrap();
        assert!(root.join("repo/.jkb/archive/w-1").is_dir());
    }

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
        // A mode the umask would narrow on a create, so a replace that only created would lose it.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        write(&path, b"two").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o666
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
        let e = read(&path).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("symbolic link"), "named a link: {e}");
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
        // On a thread with a deadline: without O_NONBLOCK the open blocks forever on a FIFO with no
        // writer, and a test that hangs reports nothing.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read(&fifo).map_err(|e| e.kind()));
        });
        let answer = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("read returned rather than blocking on the FIFO");
        assert_eq!(answer, Err(std::io::ErrorKind::InvalidInput));
    }

    #[test]
    fn a_file_larger_than_a_synced_file_may_be_is_refused_before_it_is_read() {
        let (_keep, root) = real_tempdir();
        let path = root.join("tasks.md");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(super::MAX_READ_BYTES + 1).unwrap(); // sparse: no disk, no time
        let e = read(&path).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
        assert!(e.to_string().contains("64 MiB"), "names the limit: {e}");
        assert!(
            !e.to_string().contains("symbolic link"),
            "and is not a link: {e}"
        );
    }

    /// Run in a child process under `umask 077` — the umask is process-wide, so setting it here would
    /// race every other test creating a file — where a file created `0o644` and left so would be
    /// readable by everyone.
    #[test]
    fn a_new_file_is_created_under_the_umask_and_its_temp_name_is_recognised() {
        const CHILD: &str = "JKB_NOFOLLOW_UMASK_CHILD";
        const NAME: &str =
            "nofollow::tests::a_new_file_is_created_under_the_umask_and_its_temp_name_is_recognised";
        if std::env::var_os(CHILD).is_some() {
            let (_keep, root) = real_tempdir();
            let path = root.join("new.md");
            write(&path, b"x").unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "0o666 less the umask 077");
            return;
        }
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new("sh")
            .args(["-c", "umask 077 && exec \"$0\" \"$@\""])
            .arg(exe)
            .args(["--exact", NAME, "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "the child ran the check and passed: {stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(super::is_temp_name(std::ffi::OsStr::new(
            ".tasks.md.jkb-sync-123-456.tmp"
        )));
        assert!(!super::is_temp_name(std::ffi::OsStr::new("tasks.md")));
    }
}
