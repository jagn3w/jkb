//! The lines `jkb serve` prints. Under launchd every one of them lands in `serve.log`, which holds
//! the output of every daemon that ever ran there; a line that does not say when, and which process,
//! cannot be told from a line of the daemon before it.
//!
//! **The format, kept stable:** an RFC 3339 UTC timestamp with milliseconds and a `Z` suffix, a
//! space, `pid <n>`, a space, then the message — e.g.
//! `2026-10-10T08:15:02.317Z pid 4242 jkb serve listening on http://127.0.0.1:7117 (token: …)`.
//! Something parsing a line can split on the first two spaces; the tests that read the listening
//! line find its address by the word that starts `http://`.

use std::time::SystemTime;

/// `message` as `jkb serve` prints it, stamped at `at` by process `pid` (see the module docs).
#[must_use]
pub fn format_line(at: SystemTime, pid: u32, message: &str) -> String {
    let at = chrono::DateTime::<chrono::Utc>::from(at)
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    format!("{at} pid {pid} {message}")
}

/// `message`, stamped now by this process.
#[must_use]
pub fn line(message: &str) -> String {
    format_line(SystemTime::now(), std::process::id(), message)
}

/// Print `message`, stamped, to stdout, and flush it: a supervisor's log and a test reading the pipe
/// both see the line at once.
pub fn out(message: &str) {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", line(message));
    let _ = stdout.flush();
}

/// Print `message`, stamped, to stderr.
pub fn err(message: &str) {
    eprintln!("{}", line(message));
}

/// What a panic said: its message, when it carried one.
#[must_use]
pub fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a panic with no message".to_owned())
}

/// The message of a panic's line: where, on which thread, and what it said.
#[must_use]
pub fn panic_message(
    thread: Option<&str>,
    location: Option<&std::panic::Location<'_>>,
    payload: &(dyn std::any::Any + Send),
) -> String {
    let at = location.map_or_else(|| "an unknown location".to_owned(), ToString::to_string);
    format!(
        "jkb serve: thread '{}' panicked at {at}: {}",
        thread.unwrap_or("<unnamed>"),
        panic_text(payload)
    )
}

/// Report every panic in this process as a stamped line on stderr, in place of the default report —
/// so a panic in `serve.log` says when, and in which daemon, like every other line there.
pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let current = std::thread::current();
        err(&panic_message(
            current.name(),
            info.location(),
            info.payload(),
        ));
    }));
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    #[test]
    fn a_line_starts_with_an_rfc3339_utc_timestamp_and_the_pid() {
        let at = UNIX_EPOCH + Duration::from_millis(1_791_620_102_317);
        assert_eq!(
            format_line(at, 4242, "jkb serve listening on http://127.0.0.1:7117"),
            "2026-10-10T08:15:02.317Z pid 4242 jkb serve listening on http://127.0.0.1:7117"
        );
    }

    #[test]
    fn a_panic_line_says_where_which_thread_and_what() {
        let here = std::panic::Location::caller();
        let payload: Box<dyn std::any::Any + Send> =
            Box::new(String::from("the accept loop broke"));
        assert_eq!(
            panic_message(Some("jkb-serve-accept"), Some(here), payload.as_ref()),
            format!(
                "jkb serve: thread 'jkb-serve-accept' panicked at {}:{}:{}: the accept loop broke",
                here.file(),
                here.line(),
                here.column()
            )
        );
        let silent: Box<dyn std::any::Any + Send> = Box::new(7_u8);
        assert_eq!(
            panic_message(None, None, silent.as_ref()),
            "jkb serve: thread '<unnamed>' panicked at an unknown location: a panic with no message"
        );
    }

    #[test]
    fn line_stamps_with_this_process() {
        let l = line("hi");
        let mut words = l.split(' ');
        let stamp = words.next().unwrap();
        assert!(
            chrono::DateTime::parse_from_rfc3339(stamp).is_ok() && stamp.ends_with('Z'),
            "{l}"
        );
        assert_eq!(words.next(), Some("pid"));
        assert_eq!(words.next(), Some(std::process::id().to_string().as_str()));
        assert_eq!(words.collect::<Vec<_>>(), ["hi"]);
    }
}
