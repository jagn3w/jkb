//! The design document as a CRDT (design D53.4), over `yrs` — the Rust port of Yjs by its authors,
//! so an update the app's `yjs` writes is an update this reads, byte for byte.
//!
//! Everything in this module is pure: a [`Crdt`] is a throwaway in-memory document built from the
//! stored updates, asked a question, and dropped. The table of updates is the source of truth
//! ([`super`]); nothing here survives a call.
//!
//! **Offsets are UTF-16 code units**, as they are in Yjs. `yrs` defaults to bytes, and the clock of
//! a string item counts UTF-16 units whatever the offset kind is — so a byte-offset document
//! resolves a sticky index into a non-ASCII paragraph to the wrong character. Every offset a caller
//! sees from here is UTF-16, and [`utf16_to_byte`] / [`byte_to_utf16`] are the one place either is
//! turned into the other.
//!
//! **Garbage collection is off** in every document built here. Two questions need deleted content:
//! a revert re-inserts what an update deleted (the `UndoManager` copies it from the tombstone), and
//! span state compares the text now with the text at approval, which a collected tombstone no
//! longer has.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use jkb_types::Error as TypeError;
use yrs::types::text::{ChangeKind, YChange};
use yrs::undo::{Options as UndoOptions, StackItem, UndoManager};
use yrs::updates::decoder::Decode;
use yrs::updates::encoder::Encode;
use yrs::{
    Any, Assoc, ClientID, Doc, GetString, IdSet, IndexedSequence, Map, MapRef, OffsetKind, Options,
    Out, ReadTxn, Snapshot, StateVector, StickyIndex, Text, TextRef, Transact, TransactionMut,
    Update, ID,
};

use crate::Result;

/// The root `Y.Text` holding the document body — `ydoc.getText("body")` on the app's side.
pub const BODY: &str = "body";
/// The root `Y.Map` holding each span's anchors, keyed by the span's uid.
pub const SPANS: &str = "spans";
/// A span entry's start anchor: an encoded `RelativePosition` (`Y.encodeRelativePosition`).
const START: &str = "start";
/// A span entry's end anchor.
const END: &str = "end";

fn invalid(why: impl Into<String>) -> crate::Error {
    TypeError::Validation(why.into()).into()
}

/// How many UTF-16 code units `s` is.
///
/// # Errors
/// A document past `u32::MAX` units, which `yrs` cannot address either.
pub fn utf16_len(s: &str) -> Result<u32> {
    u32::try_from(s.encode_utf16().count())
        .map_err(|_| invalid("the text is longer than a design document can address"))
}

/// The UTF-16 offset of byte offset `byte` in `s` (a char boundary).
///
/// # Errors
/// As [`utf16_len`].
pub fn byte_to_utf16(s: &str, byte: usize) -> Result<u32> {
    utf16_len(&s[..byte])
}

/// The byte offset of UTF-16 offset `unit` in `s`, clamped to the end. An offset inside a
/// surrogate pair rounds up to the character's end.
#[must_use]
pub fn utf16_to_byte(s: &str, unit: u32) -> usize {
    let mut seen = 0u32;
    for (byte, c) in s.char_indices() {
        if seen >= unit {
            return byte;
        }
        // A char is one or two UTF-16 units; `len_utf16` is at most 2.
        seen += if c.len_utf16() == 2 { 2 } else { 1 };
    }
    s.len()
}

/// A piece of a span's text and whether it is the words that were approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    /// UTF-16 start in the current text.
    pub start: u32,
    /// UTF-16 end; equal to `start` for text removed since the approval.
    pub end: u32,
    /// What kind of piece.
    pub kind: PieceKind,
    /// Its text — for a removed piece, the words that were removed.
    pub text: String,
}

/// What a [`Piece`] is, relative to the approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PieceKind {
    /// Present at approval and present now.
    Approved,
    /// Written since the approval.
    Added,
    /// Present at approval, deleted since.
    Removed,
}

/// A span's text against its approval ([`Crdt::pieces`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pieces {
    /// The span's text in pieces, in text order.
    pub pieces: Vec<Piece>,
    /// Some attested word is present but outside the span's range: its anchors were moved off the
    /// words that were approved. Demotes the span as an edit does.
    pub displaced: bool,
}

/// One in-memory design document.
pub struct Crdt {
    doc: Doc,
    body: TextRef,
    spans: MapRef,
}

impl Crdt {
    /// An empty document with a random client id.
    #[must_use]
    pub fn new() -> Self {
        Self::with_client(ClientID::random())
    }

    /// An empty document whose writes are made as `client`.
    fn with_client(client: ClientID) -> Self {
        let doc = Doc::with_options(Options {
            client_id: client,
            offset_kind: OffsetKind::Utf16,
            skip_gc: true,
            ..Options::default()
        });
        let body = doc.get_or_insert_text(BODY);
        let spans = doc.get_or_insert_map(SPANS);
        Self { doc, body, spans }
    }

    /// An empty document whose client id appears nowhere in `known` — what an edit built against an
    /// old version is written as, so its items cannot collide with a peer's since.
    ///
    /// # Errors
    /// An unreadable state vector.
    pub fn writer_for(known: &[u8]) -> Result<Self> {
        let known = StateVector::decode_v1(known)
            .map_err(|e| invalid(format!("unreadable state vector: {e}")))?;
        let mut client = ClientID::random();
        // 2^53 ids: a collision is astronomically unlikely, and a bounded loop costs nothing.
        for _ in 0..16 {
            if !known.contains_client(&client) {
                break;
            }
            client = ClientID::random();
        }
        Ok(Self::with_client(client))
    }

    /// Merge one Yjs v1 update.
    ///
    /// # Errors
    /// Bytes that are not a v1 update, or one `yrs` refuses to integrate.
    pub fn apply(&self, bytes: &[u8]) -> Result<()> {
        let update =
            Update::decode_v1(bytes).map_err(|e| invalid(format!("not a Yjs v1 update: {e}")))?;
        self.doc
            .transact_mut()
            .apply_update(update)
            .map_err(|e| invalid(format!("the update does not apply: {e}")))
    }

    /// Merge one Yjs v1 update and answer what it **changed** — the insertions and deletions this
    /// document did not already have, as one update — or `None` when it changed nothing.
    ///
    /// That delta, not the bytes a peer sent, is what is stored. A peer's sync-step-2 answer
    /// (`Y.encodeStateAsUpdate(doc, sv)`) carries the document's whole delete set; stored verbatim,
    /// `jkb undo` of it reverted every deletion in it — text deleted long before came back.
    ///
    /// # Errors
    /// As [`Crdt::apply`].
    pub fn merge(&self, bytes: &[u8]) -> Result<Option<Vec<u8>>> {
        let update =
            Update::decode_v1(bytes).map_err(|e| invalid(format!("not a Yjs v1 update: {e}")))?;
        let mut txn = self.doc.transact_mut();
        txn.apply_update(update)
            .map_err(|e| invalid(format!("the update does not apply: {e}")))?;
        let wrote = !txn.insert_set().is_empty() || !txn.delete_set().is_empty();
        Ok(wrote.then(|| txn.encode_update_v1()))
    }

    /// Whether something merged here is waiting on changes this document has not seen — an update
    /// built on another peer's text that never reached the table.
    #[must_use]
    pub fn has_missing(&self) -> bool {
        self.doc.transact().has_missing_updates()
    }

    /// The body as plain text.
    #[must_use]
    pub fn text(&self) -> String {
        self.body.get_string(&self.doc.transact())
    }

    /// The state vector, v1-encoded.
    #[must_use]
    pub fn state_vector(&self) -> Vec<u8> {
        self.doc.transact().state_vector().encode_v1()
    }

    /// What is in the document now: its state vector and its deletions.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.doc.transact().snapshot()
    }

    /// The whole document as one update — what a compaction stores, and what a new peer loads.
    #[must_use]
    pub fn encode_state(&self) -> Vec<u8> {
        self.doc
            .transact()
            .encode_state_as_update_v1(&StateVector::default())
    }

    /// What a peer at state vector `since` lacks, as one update.
    ///
    /// # Errors
    /// An unreadable state vector.
    pub fn encode_since(&self, since: &[u8]) -> Result<Vec<u8>> {
        let sv = StateVector::decode_v1(since)
            .map_err(|e| invalid(format!("unreadable state vector: {e}")))?;
        Ok(self.doc.transact().encode_state_as_update_v1(&sv))
    }

    /// Run `f` in one transaction and return what it answered and the update it wrote — `None`
    /// when it wrote nothing.
    ///
    /// # Errors
    /// What `f` answers.
    pub fn change<T>(
        &self,
        f: impl FnOnce(&mut TransactionMut<'_>, &TextRef, &MapRef) -> Result<T>,
    ) -> Result<(T, Option<Vec<u8>>)> {
        let mut txn = self.doc.transact_mut();
        let out = f(&mut txn, &self.body, &self.spans)?;
        let wrote = !txn.insert_set().is_empty() || !txn.delete_set().is_empty();
        let update = wrote.then(|| txn.encode_update_v1());
        Ok((out, update))
    }

    /// The uids of every span the document anchors, sorted.
    #[must_use]
    pub fn span_uids(&self) -> Vec<String> {
        let txn = self.doc.transact();
        let mut uids: Vec<String> = self.spans.keys(&txn).map(str::to_owned).collect();
        uids.sort();
        uids
    }

    /// A span's two anchors, if the document has an entry for it that reads as one.
    #[must_use]
    pub fn anchors(&self, uid: &str) -> Option<(StickyIndex, StickyIndex)> {
        let txn = self.doc.transact();
        let Some(Out::Any(Any::Map(entry))) = self.spans.get(&txn, uid) else {
            return None;
        };
        let read = |key: &str| match entry.get(key) {
            Some(Any::Buffer(b)) => StickyIndex::decode_v1(b).ok(),
            _ => None,
        };
        Some((read(START)?, read(END)?))
    }

    /// Where a span lies in the text now, as UTF-16 `[start, end)`; `None` for no entry, or anchors
    /// that resolve to nothing (text from a peer this document has not seen).
    #[must_use]
    pub fn range(&self, uid: &str) -> Option<(u32, u32)> {
        let (start, end) = self.anchors(uid)?;
        let txn = self.doc.transact();
        let s = start.get_offset(&txn)?.index;
        let e = end.get_offset(&txn)?.index;
        Some((s, e.max(s)))
    }

    /// Anchor a span over `[start, end)` in `txn`: the start sticks to the first character (text
    /// typed before it stays outside) and the end to the last (text typed after it stays outside),
    /// so only an insertion strictly inside the range lands in the span.
    ///
    /// # Errors
    /// An empty range, or one past the end of the text.
    pub fn anchor(
        txn: &mut TransactionMut<'_>,
        body: &TextRef,
        spans: &MapRef,
        uid: &str,
        start: u32,
        end: u32,
    ) -> Result<()> {
        if start >= end {
            return Err(invalid("a span covers at least one character"));
        }
        let (Some(s), Some(e)) = (
            body.sticky_index(txn, start, Assoc::After),
            body.sticky_index(txn, end, Assoc::Before),
        ) else {
            return Err(invalid("the span runs past the end of the text"));
        };
        let mut entry = HashMap::new();
        entry.insert(START.to_owned(), Any::Buffer(Arc::from(s.encode_v1())));
        entry.insert(END.to_owned(), Any::Buffer(Arc::from(e.encode_v1())));
        spans.insert(txn, uid, Any::Map(Arc::new(entry)));
        Ok(())
    }

    /// The Yjs ids of the characters in UTF-16 `[start, end)` — what a quote resolved against an old
    /// version names, independent of where that text has moved since.
    #[must_use]
    pub fn ids(&self, start: u32, end: u32) -> Vec<ID> {
        let txn = self.doc.transact();
        (start..end)
            .filter_map(|i| {
                self.body
                    .sticky_index(&txn, i, Assoc::After)
                    .and_then(|s| s.id().copied())
            })
            .collect()
    }

    /// Whether every id in `ids` is a character this document has and has not deleted.
    #[must_use]
    pub fn all_present(&self, ids: &[ID]) -> bool {
        let snap = self.snapshot();
        ids.iter().all(|id| visible(&snap, id))
    }

    /// What an approval of UTF-16 `[start, end)` attests to: a snapshot in which the body's only
    /// visible characters are the ones in that range now.
    ///
    /// Approval attests to *words*, so the record is the words' Yjs ids, not the anchors and not
    /// the whole document. Diffing the text against this (in [`Crdt::pieces`]) reads a character as
    /// approved only when it is one of those ids and still present: text written inside the span
    /// since, and text an anchor was moved over, both read as not approved. The record is a
    /// [`Snapshot`] — the state vector now, with every id *outside* the range counted deleted — so
    /// it is stored, decoded and diffed exactly as a plain snapshot is.
    ///
    /// # Errors
    /// A document past what UTF-16 offsets address.
    pub fn attest(&self, start: u32, end: u32) -> Result<Snapshot> {
        let mut txn = self.doc.transact_mut();
        let now = txn.snapshot();
        // Against an empty snapshot every visible character reads as added, item by item, each
        // chunk carrying its item's first id — the ids of the characters, in text order.
        let diff = self.body.diff_range(
            &mut txn,
            Some(&now),
            Some(&Snapshot::default()),
            YChange::identity,
        );
        let mut words = IdSet::new();
        let mut at = 0u32;
        for chunk in diff {
            let Out::Any(Any::String(s)) = &chunk.insert else {
                continue;
            };
            let len = utf16_len(s)?;
            if let Some(YChange { id, .. }) = chunk.ychange {
                let (lo, hi) = (at.max(start), (at + len).min(end));
                if lo < hi {
                    words.insert(ID::new(id.client, id.clock + (lo - at)), hi - lo);
                }
            }
            at += len;
        }
        let mut everything = IdSet::new();
        for (client, clock) in now.state_map.iter() {
            everything.insert(ID::new(*client, 0), *clock);
        }
        Ok(Snapshot::new(now.state_map, everything.diff(&words)))
    }

    /// A span's text in pieces, against the words its approval attested to (`attested`, from
    /// [`Crdt::attest`]): the attested words still present, the words written inside it since, and
    /// the attested words deleted since. `None` when the span is not anchored.
    ///
    /// A removed piece has no width now; it is placed where the words were, clamped into the span.
    /// Every one belongs to the span, wherever its anchors are now: the attested ids are the span's
    /// words by construction, so no anchor is consulted to decide it.
    ///
    /// # Errors
    /// A document past what UTF-16 offsets address.
    pub fn pieces(&self, uid: &str, attested: &Snapshot) -> Result<Option<Pieces>> {
        let Some((start, end)) = self.range(uid) else {
            return Ok(None);
        };
        let mut txn = self.doc.transact_mut();
        let now = txn.snapshot();
        let diff = self
            .body
            .diff_range(&mut txn, Some(&now), Some(attested), YChange::identity);
        let mut out = Pieces {
            pieces: Vec::new(),
            displaced: false,
        };
        let mut at = 0u32;
        for chunk in diff {
            let Out::Any(Any::String(s)) = &chunk.insert else {
                continue;
            };
            let len = utf16_len(s)?;
            match chunk.ychange {
                Some(YChange {
                    kind: ChangeKind::Removed,
                    ..
                }) => {
                    let p = at.clamp(start, end);
                    out.pieces.push(Piece {
                        start: p,
                        end: p,
                        kind: PieceKind::Removed,
                        text: s.to_string(),
                    });
                }
                change => {
                    let kind = if matches!(
                        change,
                        Some(YChange {
                            kind: ChangeKind::Added,
                            ..
                        })
                    ) {
                        PieceKind::Added
                    } else {
                        PieceKind::Approved
                    };
                    // Attested words outside the span's range: its anchors no longer hold the
                    // words that were approved, which only a rewrite of the anchors does.
                    if kind == PieceKind::Approved && (at < start || at + len > end) {
                        out.displaced = true;
                    }
                    let (lo, hi) = (at.max(start), (at + len).min(end));
                    if lo < hi {
                        let from = utf16_to_byte(s, lo - at);
                        let to = utf16_to_byte(s, hi - at);
                        push_merged(
                            &mut out.pieces,
                            Piece {
                                start: lo,
                                end: hi,
                                kind,
                                text: s[from..to].to_owned(),
                            },
                        );
                    }
                    at += len;
                }
            }
        }
        Ok(Some(out))
    }

    /// One forward update that reverts the update `bytes` — deletes what it inserted, puts back what
    /// it deleted — merged over everything since. `None` when there is nothing left to revert (its
    /// insertions are all deleted and its deletions all restored already).
    ///
    /// This is the Yjs `UndoManager` construction, seeded with the one stack item the update is
    /// rather than with a session's history: the manager knows how to restore a deleted run next to
    /// text written after it, which is the hard part.
    ///
    /// # Errors
    /// Bytes that are not a v1 update.
    pub fn revert(&self, bytes: &[u8]) -> Result<Option<Vec<u8>>> {
        let update =
            Update::decode_v1(bytes).map_err(|e| invalid(format!("not a Yjs v1 update: {e}")))?;
        let item: StackItem<()> = StackItem::new(
            self.doc.guid(),
            update.delete_set().clone(),
            update.insertions(true),
        );
        let mut manager: UndoManager<()> = UndoManager::with_options(UndoOptions {
            init_undo_stack: vec![item],
            ..UndoOptions::default()
        });
        manager.expand_scope(&self.doc, &self.body);
        manager.expand_scope(&self.doc, &self.spans);
        let written: Rc<RefCell<Option<Vec<u8>>>> = Rc::default();
        let sink = Rc::clone(&written);
        self.doc
            .observe_update_v1("jkb-revert", move |_, e| {
                *sink.borrow_mut() = Some(e.update.clone());
            })
            .map_err(|e| invalid(format!("cannot observe the document: {e}")))?;
        let changed = manager.undo_blocking();
        drop(manager);
        self.doc
            .unobserve_update_v1("jkb-revert")
            .map_err(|e| invalid(format!("cannot observe the document: {e}")))?;
        let out = written.borrow_mut().take();
        Ok(out.filter(|_| changed))
    }
}

impl Default for Crdt {
    fn default() -> Self {
        Self::new()
    }
}

/// `Snapshot::is_visible`, which `yrs` keeps crate-private.
fn visible(snap: &Snapshot, id: &ID) -> bool {
    snap.state_map.get(&id.client) > id.clock && !snap.delete_set.contains(id)
}

/// Append `piece`, joining it to the last one when they touch and are the same kind — the diff
/// splits text at every item boundary, which is noise to a reader.
fn push_merged(pieces: &mut Vec<Piece>, piece: Piece) {
    if let Some(last) = pieces.last_mut() {
        if last.kind == piece.kind && last.end == piece.start && piece.kind != PieceKind::Removed {
            last.end = piece.end;
            last.text.push_str(&piece.text);
            return;
        }
    }
    pieces.push(piece);
}

/// Decode a stored approval snapshot.
///
/// # Errors
/// Bytes that are not a v1 snapshot.
pub fn decode_snapshot(bytes: &[u8]) -> Result<Snapshot> {
    Snapshot::decode_v1(bytes).map_err(|e| invalid(format!("unreadable approval snapshot: {e}")))
}

/// Encode a snapshot for storage.
#[must_use]
pub fn encode_snapshot(snap: &Snapshot) -> Vec<u8> {
    snap.encode_v1()
}
