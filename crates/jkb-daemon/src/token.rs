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
/// # Errors
/// [`TokenError::Io`] when the directory or file cannot be written.
pub fn write(path: &Path, token: &str) -> Result<(), TokenError> {
    let io = |what, path: &Path| {
        let path = path.to_path_buf();
        move |source| TokenError::Io { what, path, source }
    };
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir).map_err(io("creating", dir))?;
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options.open(&tmp).map_err(io("creating", &tmp))?;
        file.write_all(token.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(io("writing", &tmp))?;
    }
    std::fs::rename(&tmp, path).map_err(io("replacing", path))
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

    #[test]
    fn comparison_is_exact() {
        assert!(matches("abcd", "abcd"));
        assert!(!matches("abcd", "abce"));
        assert!(!matches("abcd", "abc"));
        assert!(!matches("abcd", ""));
    }
}
