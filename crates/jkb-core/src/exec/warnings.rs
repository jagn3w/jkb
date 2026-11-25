//! The `warning:` lines of a run's output, collected as the output arrives.
//!
//! A row keeps only the last [`super::MAX_TAIL_BYTES`] of output, and a land's early `warning:` line
//! can fall out of that before the run ends. So [`super::output`], the one writer of chunks, scans each
//! chunk as it arrives ([`collect`]), carrying a line split across chunks in `exec_output.warn_carry`;
//! [`super::finish`] flushes the carry, and scans the tail instead when no chunk was ever streamed.

use rusqlite::{params, Connection};

use crate::Result;

/// The most warning lines a request keeps; the rest are counted (`warnings_dropped`).
pub const MAX_WARNINGS: usize = 50;

/// The longest warning line kept, in bytes; a longer one is cut. Also the carry's cap.
pub const MAX_WARNING_BYTES: usize = 1024;

/// What every collected line starts with, once leading blanks and SGR sequences are stripped.
pub const PREFIX: &[u8] = b"warning:";

/// `line` with its leading blanks and ANSI SGR sequences (`ESC [ <params> m`) stripped, if what is left
/// starts with [`PREFIX`]. A colourised `\x1b[33mwarning:\x1b[0m …` is a warning; a `warning:` in the
/// middle of a line is not.
fn warning(line: &[u8]) -> Option<&[u8]> {
    let mut rest = line.strip_suffix(b"\r").unwrap_or(line);
    loop {
        match rest {
            [b' ' | b'\t', tail @ ..] => rest = tail,
            [0x1b, b'[', tail @ ..] => {
                let params = tail
                    .iter()
                    .take_while(|b| (0x30..=0x3f).contains(*b))
                    .count();
                match tail.get(params) {
                    Some(b'm') => rest = &tail[params + 1..],
                    _ => return None,
                }
            }
            _ => break,
        }
    }
    rest.starts_with(PREFIX).then_some(rest)
}

/// `bytes` as text, cut to [`MAX_WARNING_BYTES`] at a character boundary.
fn text(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut end = s.len().min(MAX_WARNING_BYTES);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_owned()
}

/// Scan `chunk` after the unfinished line `carry`: the warning lines it completes, and the new carry
/// (at most [`MAX_WARNING_BYTES`]: a longer line is cut, which is all a kept warning line keeps anyway).
/// With `flush`, the last unfinished line counts as complete.
#[must_use]
pub fn scan(carry: &[u8], chunk: &[u8], flush: bool) -> (Vec<u8>, Vec<String>) {
    let mut line = carry.to_vec();
    let mut found = Vec::new();
    let mut pieces = chunk.split(|b| *b == b'\n').peekable();
    while let Some(piece) = pieces.next() {
        let room = MAX_WARNING_BYTES.saturating_sub(line.len());
        line.extend_from_slice(&piece[..piece.len().min(room)]);
        // Every piece but the last ended at a newline.
        if pieces.peek().is_some() {
            if let Some(w) = warning(&line) {
                found.push(text(w));
            }
            line.clear();
        }
    }
    if flush {
        if let Some(w) = warning(&line) {
            found.push(text(w));
        }
        line.clear();
    }
    (line, found)
}

/// Scan `bytes` into request `id`'s stored warnings, adding `streamed` to its `streamed_bytes`. The row
/// in `exec_output` must exist.
///
/// # Errors
/// A database error, or stored warnings that are not a JSON array of strings.
pub fn collect(
    conn: &Connection,
    id: i64,
    bytes: &[u8],
    flush: bool,
    streamed: usize,
) -> Result<()> {
    let (stored, dropped, carry): (String, i64, Vec<u8>) = conn
        .prepare_cached(
            "SELECT warnings, warnings_dropped, warn_carry FROM exec_output WHERE request_id = ?1",
        )?
        .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    let mut warnings: Vec<String> = serde_json::from_str(&stored).map_err(|e| {
        crate::mq::QueueError::Corrupt(format!(
            "exec request {id}'s warnings are not a list of strings: {e}"
        ))
    })?;
    let (carry, lines) = scan(&carry, bytes, flush);
    let mut dropped = dropped;
    for line in lines {
        if warnings.len() < MAX_WARNINGS {
            warnings.push(line);
        } else {
            dropped = dropped.saturating_add(1);
        }
    }
    let warnings = serde_json::to_string(&warnings)
        .map_err(|e| jkb_types::Error::Validation(format!("warnings do not serialize: {e}")))?;
    conn.prepare_cached(
        "UPDATE exec_output SET warnings = ?2, warnings_dropped = ?3, warn_carry = ?4, \
             streamed_bytes = streamed_bytes + ?5 WHERE request_id = ?1",
    )?
    .execute(params![
        id,
        warnings,
        dropped,
        carry,
        i64::try_from(streamed).unwrap_or(i64::MAX)
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{scan, MAX_WARNING_BYTES};

    #[test]
    fn a_line_split_across_chunks_is_collected_once() {
        let (carry, first) = scan(b"", b"building\nwarn", false);
        assert!(first.is_empty());
        assert_eq!(carry, b"warn");
        let (carry, second) = scan(&carry, b"ing: 2 tasks unplanned\nok\n", false);
        assert_eq!(second, vec!["warning: 2 tasks unplanned".to_owned()]);
        assert!(carry.is_empty());
    }

    #[test]
    fn an_ansi_prefixed_line_is_collected_and_a_mid_line_one_is_not() {
        let (_, found) = scan(
            b"",
            b"\x1b[1;33mwarning:\x1b[0m colour\n  \twarning: indented\nnote: warning: inside\n\x1b[2Kwarning: erased\r\nwarning: crlf\r\n",
            false,
        );
        assert_eq!(
            found,
            vec![
                "warning:\u{1b}[0m colour".to_owned(),
                "warning: indented".to_owned(),
                "warning: crlf".to_owned(),
            ],
            "only SGR sequences are stripped: `ESC [2K` is not colour, so that line is not one"
        );
    }

    #[test]
    fn the_last_line_counts_only_when_flushed() {
        let (carry, found) = scan(b"", b"warning: no newline", false);
        assert!(found.is_empty());
        let (carry, found) = scan(&carry, b"", true);
        assert_eq!(found, vec!["warning: no newline".to_owned()]);
        assert!(carry.is_empty());
    }

    #[test]
    fn a_long_line_is_cut_and_the_carry_stays_bounded() {
        let mut long = b"warning: ".to_vec();
        long.extend(std::iter::repeat_n(b'x', 5000));
        let (carry, _) = scan(b"", &long, false);
        assert_eq!(carry.len(), MAX_WARNING_BYTES);
        let (carry, more) = scan(&carry, &[b'y'; 3000], false);
        assert!(more.is_empty());
        assert_eq!(carry.len(), MAX_WARNING_BYTES);
        let (_, found) = scan(&carry, b"\n", false);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].len(), MAX_WARNING_BYTES);
    }

    #[test]
    fn invalid_utf8_is_replaced_and_still_cut_at_a_character() {
        let mut line = b"warning: ".to_vec();
        line.extend(std::iter::repeat_n(0xff, 1100));
        line.push(b'\n');
        let (_, found) = scan(b"", &line, false);
        assert!(found[0].len() <= MAX_WARNING_BYTES);
        assert!(found[0].starts_with("warning: \u{fffd}"));
    }
}
