//! Replacing a file without anyone ever seeing half of it.
//!
//! Every installer in this binary writes files that something else reads, or runs, while the
//! write is happening — the launchd/systemd unit a supervisor loads, and the
//! `~/.claude/{workflows,commands}` assets a running workflow reads. `std::fs::write`
//! truncates the destination and rewrites it in place, so a reader can observe the gap.
//!
//! This is one seam rather than a rule each installer remembers, because it already went
//! wrong that way: the shell half of it (`scripts/lib.sh`'s `install_exec`) was fixed first,
//! [`service::install`] second, and [`commands::write_all`] was still truncating in place —
//! on a path `scripts/setup.sh` itself triggers, since a `git pull` runs the post-merge hook,
//! which runs setup.sh, which reinstalls the binary, which reconciles those assets.

use std::ffi::{OsStr, OsString};
use std::path::Path;

use anyhow::{Context, Result};

/// Write `contents` to `path` atomically: a temp file in the same directory, then a rename.
///
/// A rename swaps the directory entry in one step, so every reader sees the whole old file or
/// the whole new one — and a process still *executing* the old one keeps its inode. On any
/// returned error the destination is left exactly as it was and the temp file goes with it.
///
/// The one residue this cannot prevent is the process being killed between the write and the
/// rename, which leaves a `.<name>.<pid>.tmp` sibling; `jkb_core::store::backup` states the
/// same cost for the same reason.
///
/// # Errors
/// Returns an error if the temp file cannot be written or cannot be renamed over `path`.
pub fn write(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp_name = OsString::from(".");
    tmp_name.push(path.file_name().unwrap_or_else(|| OsStr::new("file")));
    // The pid keeps two concurrent installs off one another's temp file.
    tmp_name.push(format!(".{}.tmp", std::process::id()));
    let tmp = dir.join(tmp_name);

    // Both fallible steps clean up through one path — a write that fails part-way leaves a
    // temp file just as surely as a failed rename does.
    let installed = std::fs::write(&tmp, contents)
        .with_context(|| format!("writing {}", tmp.display()))
        .and_then(|()| {
            std::fs::rename(&tmp, path).with_context(|| format!("installing {}", path.display()))
        });
    if installed.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    installed
}

#[cfg(test)]
mod tests {
    use super::write;

    /// Pins the *mechanism*, not just the result: a plain `std::fs::write` passes every
    /// assertion about the file's final contents, because writing does work — it is the
    /// reader mid-load that an in-place rewrite corrupts. So the test holds one open, the
    /// way `launchctl load` does.
    #[test]
    #[cfg(unix)]
    fn write_installs_by_rename_never_rewriting_in_place() {
        use std::io::Read;

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("com.jkb.sync.plist");
        write(&path, b"first").expect("first install");

        let mut reader = std::fs::File::open(&path).expect("open the installed file");
        write(&path, b"second").expect("replacing install");
        let mut seen = String::new();
        reader
            .read_to_string(&mut seen)
            .expect("read through the open handle");
        assert_eq!(
            seen, "first",
            "the reinstall rewrote the file a reader was already holding"
        );

        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "second");
        assert_eq!(
            entries(dir.path()),
            vec!["com.jkb.sync.plist".to_string()],
            "a successful install left something else behind"
        );
    }

    /// The twin of `install-exec.test.sh`'s "a failed install leaves the destination
    /// untouched". A rename onto an existing directory is the one failure reachable without
    /// fault injection, and it exercises the cleanup both fallible steps share.
    #[test]
    fn a_failed_write_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("occupied");
        std::fs::create_dir(&path).expect("occupy the destination");

        write(&path, b"unit").expect_err("installing over a directory should fail");

        assert_eq!(
            entries(dir.path()),
            vec!["occupied".to_string()],
            "a failed install left a temp file behind"
        );
    }

    /// Every entry in the directory, sorted — never a search for the temp name we expect,
    /// which would stop holding the moment the template changed.
    fn entries(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .filter_map(std::result::Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }
}
