//! The bearer token: minted per daemon start, written 0600 by tmp+rename, compared in constant time.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// Why the token could not be written or read.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    /// No randomness available.
    #[error("generating a token: {0}")]
    Random(String),
    /// A filesystem failure.
    #[error("{what} {}: {source}", path.display())]
    Io {
        /// What was being done.
        what: &'static str,
        /// Where.
        path: PathBuf,
        /// The failure.
        source: std::io::Error,
    },
    /// The file holds no token.
    #[error("{} holds no token", .0.display())]
    Empty(PathBuf),
    /// The token's directory is a symlink. `~/.jkb` is writable from the dev container, so a link
    /// there could send the host daemon's writes anywhere the user can write.
    #[error("refusing to write the token through a symlinked directory: {}", .0.display())]
    SymlinkedDir(PathBuf),
}

/// A fresh 256-bit token, hex-encoded.
///
/// # Errors
/// [`TokenError::Random`] when the platform has no randomness to give.
pub fn mint() -> Result<String, TokenError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| TokenError::Random(e.to_string()))?;
    Ok(bytes.iter().fold(String::with_capacity(64), |mut s, b| {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// Write `token` to `path`, readable by this user only, via a temp file renamed into place — so a
/// client reading concurrently sees the old token or the new one, never a half-written file.
///
/// **The directory is shared with the dev container** (`~/.jkb` is bind-mounted read-write), so
/// everything here assumes the container may have planted links in it:
/// - the token directory is opened once with `O_NOFOLLOW`, and the temp file is created and renamed
///   **relative to that handle** — so a `daemon` link planted before the call is refused, and one
///   swapped in mid-call cannot redirect the writes (a check on the path, then an open of the path,
///   loses that race);
/// - the temp file is created `O_CREAT|O_EXCL|O_NOFOLLOW` under an unpredictable name — a
///   predictable `token.tmp.<pid>` let a planted link make the daemon truncate any host file the
///   user can write, and a planted regular file kept its own mode, leaving the token world-readable;
/// - the final rename replaces a planted `token` link itself rather than writing through it.
///
/// Ancestors of the directory are trusted: the container cannot replace its bind's own root.
///
/// # Errors
/// [`TokenError::SymlinkedDir`], or [`TokenError::Io`] when the directory or file cannot be written.
pub fn write(path: &Path, token: &str) -> Result<(), TokenError> {
    use rustix::fs::{fsync, mkdirat, openat, renameat, Mode, OFlags, CWD};
    let io = |what, path: &Path| {
        let path = path.to_path_buf();
        move |e: std::io::Error| TokenError::Io {
            what,
            path,
            source: e,
        }
    };
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path.file_name().ok_or_else(|| TokenError::Io {
        what: "naming",
        path: path.to_path_buf(),
        source: std::io::ErrorKind::InvalidInput.into(),
    })?;
    if let Some(grand) = dir.parent().filter(|g| !g.as_os_str().is_empty()) {
        std::fs::create_dir_all(grand).map_err(|source| TokenError::Io {
            what: "creating",
            path: grand.to_path_buf(),
            source,
        })?;
    }
    match mkdirat(CWD, dir, Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(e) => return Err(io("creating", dir)(e.into())),
    }
    let dir_fd = openat(
        CWD,
        dir,
        OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| {
        // Linux answers ELOOP (measured); other platforms may pick another errno for a link opened
        // O_DIRECTORY|O_NOFOLLOW, so the link itself is what is checked, not the errno.
        let linked = std::fs::symlink_metadata(dir).is_ok_and(|m| m.file_type().is_symlink());
        if linked {
            TokenError::SymlinkedDir(dir.to_path_buf())
        } else {
            io("opening", dir)(e.into())
        }
    })?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".tmp.{}", mint()?));
    let tmp = dir.join(&tmp_name);
    let fd = openat(
        &dir_fd,
        &tmp_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::RUSR | Mode::WUSR,
    )
    .map_err(|e| io("creating", &tmp)(e.into()))?;
    let mut file = std::fs::File::from(fd);
    let written = file
        .write_all(token.as_bytes())
        .and_then(|()| Ok(fsync(&file)?))
        .and_then(|()| Ok(renameat(&dir_fd, &tmp_name, &dir_fd, name)?));
    if let Err(e) = written {
        let _ = rustix::fs::unlinkat(&dir_fd, &tmp_name, rustix::fs::AtFlags::empty());
        return Err(io("writing", path)(e));
    }
    Ok(())
}

/// Read the token at `path`, whitespace trimmed.
///
/// # Errors
/// [`TokenError::Io`] when it cannot be read, [`TokenError::Empty`] when it holds nothing.
pub fn read(path: &Path) -> Result<String, TokenError> {
    let text = std::fs::read_to_string(path).map_err(|source| TokenError::Io {
        what: "reading",
        path: path.to_path_buf(),
        source,
    })?;
    let token = text.trim();
    if token.is_empty() {
        return Err(TokenError::Empty(path.to_path_buf()));
    }
    Ok(token.to_owned())
}

/// Constant-time equality, so a response's timing says nothing about how much of a guess matched.
#[must_use]
pub fn matches(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::{matches, mint, read, write};

    #[test]
    fn a_token_is_256_random_bits_and_never_repeats() {
        let (a, b) = (mint().unwrap(), mint().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn written_owner_only_and_read_back_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon/token");
        write(&path, "abc").unwrap();
        assert_eq!(read(&path).unwrap(), "abc");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        write(&path, "def\n").unwrap();
        assert_eq!(read(&path).unwrap(), "def", "rotated in place");
        std::fs::write(&path, "  \n").unwrap();
        assert!(
            read(&path).is_err(),
            "an empty token is an error, not a match for an empty header"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_token_link_is_replaced_not_written_through() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "precious").unwrap();
        let daemon = dir.path().join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        std::os::unix::fs::symlink(&victim, daemon.join("token")).unwrap();
        write(&daemon.join("token"), "tok").unwrap();
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "precious",
            "the link target is untouched"
        );
        assert!(!std::fs::symlink_metadata(daemon.join("token"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(read(&daemon.join("token")).unwrap(), "tok");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_token_directory_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("daemon")).unwrap();
        let err = write(&dir.path().join("daemon/token"), "tok").unwrap_err();
        assert!(matches!(err, super::TokenError::SymlinkedDir(_)), "{err}");
        assert!(
            std::fs::read_dir(&elsewhere).unwrap().next().is_none(),
            "nothing written there"
        );
    }

    #[test]
    fn comparison_is_exact() {
        assert!(matches("abcd", "abcd"));
        assert!(!matches("abcd", "abce"));
        assert!(!matches("abcd", "abc"));
        assert!(!matches("abcd", ""));
    }
}
