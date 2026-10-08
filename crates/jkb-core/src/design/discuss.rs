//! What a *Discuss* names (design D53.5): a range of a design's text, at the version its reader saw,
//! turned into the quote Claude addresses it by.
//!
//! The editor knows a selection as UTF-16 offsets into the text it shows; Claude names text by quote
//! (D53.4: a model copies a quote exactly and counts offsets badly). So the range is resolved here,
//! in the version the editor read, into the quote, which match of it the range is, and the spans it
//! touches — everything the prompt needs for `jkb design edit --base … --find … --occurrence …` to
//! land on exactly the words the operator selected.

use rusqlite::Connection;

use super::{crdt, design_id, invalid, load, load_base, span_views, SpanState, Version};
use crate::Result;

/// The longest selection a *Discuss* carries, in UTF-16 units. A prompt is an argument of the
/// program it starts, and a selection longer than this is a discussion of the whole design, which
/// `jkb design cat` already gives.
pub const MAX_DISCUSS_UNITS: u32 = 16 * 1024;

/// A span the selection touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Touched {
    /// The span's uid.
    pub uid: String,
    /// Its state at the version read.
    pub state: SpanState,
    /// Who approves it.
    pub reviewer: super::Reviewer,
}

/// A selection resolved in the version it was made in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discussion {
    /// The design's uid.
    pub uid: String,
    /// Its title.
    pub title: String,
    /// The version the selection was made in.
    pub version: Version,
    /// UTF-16 start.
    pub start: u32,
    /// UTF-16 end.
    pub end: u32,
    /// The selected text: the quote an edit names it by.
    pub quote: String,
    /// Which match of the quote the selection is, from 1, when the quote occurs more than once in
    /// that version — the `--occurrence` an edit needs. `None` when it is the only one.
    pub occurrence: Option<usize>,
    /// How many times the quote occurs in that version.
    pub occurrences: usize,
    /// The anchored spans the selection overlaps, in text order.
    pub spans: Vec<Touched>,
}

/// Resolve UTF-16 `[start, end)` of design `uid` at version `base` (a token from `design.cat`; the
/// current version for `None`).
///
/// # Errors
/// An unknown design, a malformed or foreign token, a base older than the last compaction, an empty
/// or out-of-range selection, one that splits a surrogate pair, one longer than
/// [`MAX_DISCUSS_UNITS`], or one whose text an edit could not address by quote (it starts inside an
/// earlier match of itself).
pub fn discussion(
    conn: &Connection,
    uid: &str,
    base: Option<&str>,
    start: u32,
    end: u32,
) -> Result<Discussion> {
    let id = design_id(conn, uid)?;
    let title = crate::item::get(conn, id)?
        .and_then(|m| m.content)
        .unwrap_or_default();
    let (now, current) = load(conn, id)?;
    let (doc, version) = match base {
        None => (now, current),
        Some(token) => {
            let version = Version::parse(token)?;
            (load_base(conn, id, &version, &now)?, version)
        }
    };
    let text = doc.text();
    let len = crdt::utf16_len(&text)?;
    if start >= end {
        return Err(invalid("select some text to discuss: the range is empty"));
    }
    if end > len {
        return Err(invalid(format!(
            "the range {start}..{end} runs past the end of the text ({len} UTF-16 units) in the \
             version read"
        )));
    }
    if end - start > MAX_DISCUSS_UNITS {
        return Err(invalid(format!(
            "the selection is {} UTF-16 units; a discussion carries at most {MAX_DISCUSS_UNITS} — \
             select less, or discuss the whole design from `jkb design cat`",
            end - start
        )));
    }
    let (from, to) = (
        crdt::utf16_to_byte(&text, start),
        crdt::utf16_to_byte(&text, end),
    );
    // `utf16_to_byte` rounds an offset inside a character to that character's end, so a range that
    // splits a surrogate pair would read a different selection than the editor made.
    if crdt::byte_to_utf16(&text, from)? != start || crdt::byte_to_utf16(&text, to)? != end {
        return Err(invalid(format!(
            "the range {start}..{end} splits a character in the version read"
        )));
    }
    let quote = text[from..to].to_owned();
    // The matches an edit's `--find` counts: `locate`'s, non-overlapping from the left. A selection
    // that starts inside an earlier match of itself is not one of them, and no quote of exactly
    // these words could name it.
    let hits: Vec<usize> = text
        .match_indices(quote.as_str())
        .map(|(at, _)| at)
        .collect();
    let Some(index) = hits.iter().position(|&at| at == from) else {
        return Err(invalid(format!(
            "the selection {quote:?} overlaps an earlier occurrence of the same text, so an edit \
             could not name it by quote — extend the selection by a character"
        )));
    };
    let occurrence = (hits.len() > 1).then_some(index + 1);
    let spans = span_views(conn, id, &doc)?
        .into_iter()
        .filter(|s| s.anchored && s.start < end && start < s.end)
        .map(|s| Touched {
            uid: s.uid,
            state: s.state,
            reviewer: s.reviewer,
        })
        .collect();
    Ok(Discussion {
        uid: uid.to_owned(),
        title,
        version,
        start,
        end,
        quote,
        occurrence,
        occurrences: hits.len(),
        spans,
    })
}
