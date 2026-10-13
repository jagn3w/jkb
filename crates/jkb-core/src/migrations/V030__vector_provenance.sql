-- Vector freshness is derived from recorded provenance, never marked.
--
-- "Pending" used to mean only "has no vector", so an item kept the vector of whatever it said when
-- first embedded, forever: `jkb task edit`, a file-sync import, `undo` of either, an investigation's
-- unit edit — every content write after the first embed left vector search matching the old text
-- while FTS (kept current by the V002 triggers) matched the new. An edited document was worse: its
-- chunks were never re-cut, so they and their vectors kept the captured text.
--
-- Each link of the derivation now records its input, and nothing is ever flagged:
--
--   items.content_rev   counts content changes. Bumped by the trigger below, so `undo`'s generic
--                       column restore and every future writer count without remembering to.
--   items.chunked_rev   the content_rev the item's chunks were cut from. Written only by the
--                       chunker (`jkb-ingest`), beside the chunks it cuts. NULL for an item the
--                       chunker never cut — a synced document, a note — which is embedded from its
--                       own text. Backfilled for the documents that already have chunks: 0, as
--                       they were cut from the text it holds at rev 0 — or -1 if it was edited
--                       since its capture (below), so it is re-cut.
--   vector_sources      the content_rev each stored vector was computed from, and `seq`, a
--                       store-wide write counter. Written only by the one vector writer,
--                       `jkb_index::VectorIndexer::upsert_vector`, beside the vector and only if
--                       the item is still at that rev. A document's vector is the centroid of its
--                       chunks', so it is stale when any chunk's `seq` exceeds its own.
--
-- Pending is then a predicate (`jkb_index::pending_predicate`): no sidecar row, or a sidecar rev
-- other than the content rev. A vector written before this migration has no sidecar row, so the
-- upgrade reports the whole store pending and the next `jkb index` re-embeds it once, resumably —
-- the old vectors serve until each is replaced. Counting them fresh instead would keep every item
-- edited before the upgrade wrong for ever, silently, which is the defect being fixed.
--
-- `vector_sources` is derived index state (D9), like the vec tables: not changelogged, and
-- `ON DELETE CASCADE` drops a deleted item's row with the vector the GC trigger drops.

ALTER TABLE items ADD COLUMN content_rev INTEGER NOT NULL DEFAULT 0;
ALTER TABLE items ADD COLUMN chunked_rev INTEGER;

-- A chunked document whose text was edited before this migration still holds the chunks of its
-- captured text, and nothing counted the edit. Ingest stores a document as uid `b3:<hash>` with
-- `content_hash` `<hash>` (for source bytes and for text alike); an edit through the content repo
-- clears or replaces `content_hash`, so one that no longer matches its uid was edited. It is recorded
-- as cut at -1, a revision no text ever has, so the first embed pass re-cuts it from what it says now
-- rather than storing its old chunks' centroid under its new text for ever.
UPDATE items SET chunked_rev = CASE WHEN content_hash IS NOT substr(uid, 4) THEN -1 ELSE 0 END
 WHERE kind = 'document'
   AND EXISTS (SELECT 1 FROM edges e JOIN items c ON c.id = e.src_item_id
                WHERE e.dst_item_id = items.id AND e.type = 'derived_from' AND c.kind = 'chunk');

-- The FTS update trigger (V002, re-created by V006 and V010) fired on an UPDATE of any column. The
-- revision bump below is an UPDATE fired from inside a content UPDATE, and SQLite does not order
-- the triggers of one statement: when the bump ran first, the FTS trigger it fired un-indexed
-- `old.content` — by then the NEW text — while the shadow tables still held the old text's tokens,
-- and FTS5 reported "database disk image is malformed" (reproduced by `undo`'s tests). Narrowed to
-- `UPDATE OF content`, the only column FTS indexes, so the bump never touches FTS and the outer
-- statement's own firing does the one un-index/re-index, whatever the order. An update of any
-- other column no longer re-indexes an unchanged text either. The trigger, not the table: the
-- populated FTS5 table is untouched.
DROP TRIGGER items_after_update;
CREATE TRIGGER items_after_update AFTER UPDATE OF content ON items BEGIN
    INSERT INTO fts_items (fts_items, rowid, content) VALUES ('delete', old.id, old.content);
    INSERT INTO fts_items (rowid, content) VALUES (new.id, new.content);
END;

-- `IS NOT` rather than `<>`: NULL to text (and text to NULL) is a change. A write of identical text
-- is not, so a sync that rewrites an unchanged body causes no re-embed. From the larger of the
-- row's previous count and whatever the statement itself wrote to the column, so the bump is an
-- increase over both and the guard below never refuses it.
CREATE TRIGGER items_content_rev AFTER UPDATE OF content ON items
WHEN old.content IS NOT new.content
BEGIN
    UPDATE items SET content_rev = max(old.content_rev, new.content_rev) + 1 WHERE id = new.id;
END;

-- The count never goes backwards. A vector's sidecar names the rev it was computed from; a rev that
-- could return to an earlier value could make a vector of different text read as current again.
-- No writer lowers it — no changelog entry names the column outside an item delete's snapshot,
-- which is restored by INSERT — so this refuses only a write that would break the derivation.
CREATE TRIGGER items_content_rev_monotonic BEFORE UPDATE OF content_rev ON items
WHEN new.content_rev < old.content_rev
BEGIN
    SELECT RAISE(ABORT, 'items.content_rev never decreases: it names the text a stored vector was computed from');
END;

CREATE TABLE vector_sources (
    item_id     INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    content_rev INTEGER NOT NULL,
    seq         INTEGER NOT NULL
);

-- `upsert_vector` takes the next `seq` as `max(seq) + 1`.
CREATE INDEX idx_vector_sources_seq ON vector_sources (seq);
