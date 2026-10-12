//! A closed stdout ends `jkb` quietly, the way it ends `cat`.
//!
//! `jkb design cat … | sed 1q` closes the pipe while `jkb` is still writing. The Rust runtime
//! ignores `SIGPIPE`, so instead of dying of the signal the write fails with `EPIPE` — and
//! `println!` turns that failure into a panic (`failed printing to stdout: Broken pipe`) with a
//! backtrace hint on stderr and exit status 101. Resetting `SIGPIPE` to its default needs
//! `libc::signal`, which is `unsafe`, and the workspace allows exactly one `unsafe` (the
//! `sqlite-vec` registration). So the failure is caught where it surfaces instead, in the two
//! places it can:
//!
//! - **`println!`/`print!`** — five hundred call sites — panic. [`install`] puts a panic hook in
//!   front of the default one that recognises that one panic and exits.
//! - **an explicit write** (`write_all` to stdout) returns the error up through `run`. Such a site
//!   writes through [`write_stdout`], which tags the failure as [`StdoutClosed`], and `main` asks
//!   [`is_stdout_closed`] before printing an error.
//!
//! The tag is a type rather than a check for `ErrorKind::BrokenPipe` anywhere in the error chain
//! on purpose: the daemon client fails with `BrokenPipe` too when `jkb serve` hangs up mid-request
//! (`jkb_daemon::client`), and that is a real error that must still be reported.
//!
//! Either way the exit status is [`EXIT_STATUS`], 141 — what a shell reports for a process killed
//! by `SIGPIPE`, so `set -o pipefail` sees exactly what it sees from `cat`.

use std::io::Write;

/// `128 + SIGPIPE`: the status a shell reports for a process the signal killed.
pub const EXIT_STATUS: i32 = 141;

/// `EPIPE`, which is 32 on Linux, macOS and the BSDs. Spelled here rather than taken from `libc`,
/// which this crate does not otherwise need; pinned by
/// `epipe_is_the_broken_pipe_error`.
#[cfg(unix)]
const EPIPE: i32 = 32;

/// A write to stdout failed because its reader has gone. Not a failure to report.
#[derive(Debug)]
pub struct StdoutClosed;

impl std::fmt::Display for StdoutClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("stdout is closed")
    }
}

impl std::error::Error for StdoutClosed {}

/// Whether `err` is a write to a closed stdout, which `main` ends quietly rather than reporting.
pub fn is_stdout_closed(err: &anyhow::Error) -> bool {
    err.is::<StdoutClosed>()
}

/// Write `bytes` to stdout, tagging a closed pipe as [`StdoutClosed`].
///
/// # Errors
/// [`StdoutClosed`] when the reader has gone; any other write error as it is.
pub fn write_stdout(bytes: &[u8]) -> anyhow::Result<()> {
    write_to(&mut std::io::stdout().lock(), bytes)
}

/// [`write_stdout`] over any writer, so the mapping is testable without a real pipe.
fn write_to(out: &mut impl Write, bytes: &[u8]) -> anyhow::Result<()> {
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(StdoutClosed.into()),
        other => Ok(other?),
    }
}

/// Whether a panic's message is `println!`'s report of a write to a closed stdout.
///
/// The message is std's `failed printing to stdout: {io::Error}`, and the error's text comes from
/// the same `strerror` this compares against, so a localized message still matches.
fn is_closed_stdout_panic(message: &str) -> bool {
    #[cfg(unix)]
    {
        let closed = std::io::Error::from_raw_os_error(EPIPE).to_string();
        message
            .strip_prefix("failed printing to stdout: ")
            .is_some_and(|rest| rest == closed)
    }
    #[cfg(not(unix))]
    {
        let _ = message;
        false
    }
}

/// Exit quietly with [`EXIT_STATUS`] when `println!` panics on a closed stdout; hand every other
/// panic to the hook that was installed before. Call first thing in `main`. A later
/// `std::panic::set_hook` (`jkb serve` installs its own) replaces this one, which is what a
/// daemon with no reader on stdout wants anyway.
pub fn install() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if info.payload_as_str().is_some_and(is_closed_stdout_panic) {
            std::process::exit(EXIT_STATUS);
        }
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    struct Full;
    impl Write for Full {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("disk full"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    #[test]
    fn epipe_is_the_broken_pipe_error() {
        assert_eq!(
            std::io::Error::from_raw_os_error(EPIPE).kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn a_closed_writer_is_tagged_and_recognised() {
        let err = write_to(&mut Closed, b"x").unwrap_err();
        assert!(is_stdout_closed(&err));
        // ...even once a caller has added context on the way up.
        assert!(is_stdout_closed(&err.context("printing the blob")));
    }

    #[test]
    fn any_other_write_error_is_still_reported() {
        let err = write_to(&mut Full, b"x").unwrap_err();
        assert!(!is_stdout_closed(&err));
        assert!(err.to_string().contains("disk full"));
    }

    #[test]
    fn a_bare_broken_pipe_from_elsewhere_is_not_a_closed_stdout() {
        // The daemon client's hang-up surfaces as a plain `BrokenPipe`; it must still be reported.
        let err = anyhow::Error::from(std::io::Error::from(std::io::ErrorKind::BrokenPipe));
        assert!(!is_stdout_closed(&err));
    }

    #[cfg(unix)]
    #[test]
    fn only_the_stdout_epipe_panic_is_recognised() {
        let closed = std::io::Error::from_raw_os_error(EPIPE);
        assert!(is_closed_stdout_panic(&format!(
            "failed printing to stdout: {closed}"
        )));
        assert!(!is_closed_stdout_panic(&format!(
            "failed printing to stderr: {closed}"
        )));
        let other = std::io::Error::from_raw_os_error(28); // ENOSPC
        assert!(!is_closed_stdout_panic(&format!(
            "failed printing to stdout: {other}"
        )));
        assert!(!is_closed_stdout_panic("index out of bounds"));
    }
}
