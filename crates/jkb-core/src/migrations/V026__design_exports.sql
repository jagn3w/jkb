-- Where a design's export goes, and the files it was made from (design D55.5–6), each in a table of
-- its own rather than as keys of the design item's `metadata`.
--
-- They were metadata keys first, and the changelog logs `metadata` as one column: undoing an older
-- doc-target write put back the whole blob of its time, silently dropping sources recorded since.
-- One row per fact makes each write undo on its own.
--
-- `design_id` is the doc target's rowid, so the changelog keys a target by its design. A target is a
-- path relative to the checkout of the design's repo, and one file of a repo is one design's alone.
-- That rule is NOT a constraint here: a design's repo is derived from its namespace, which
-- `jkb ns mv` can change, so a stored copy would go stale. It is checked where a target is set
-- (`design::export::set_doc_target`), and `design::export::exports` refuses a repo in which two
-- designs name one file (which an undo restoring an older target can still produce).
--
-- The cascades fire on one path: `jkb undo` of a design's create, which deletes the item
-- (`item::remove` refuses a design). Both tables are in `design::DESIGN_OWNED`, so that undo is
-- refused while either holds a row a later transaction wrote (the rule is D47's, in
-- docs/namespaces-and-sync.md). `txn_id` is the transaction that last wrote the row, which is how
-- the guard tells a later row; an undo restoring an older value restores its `txn_id` too. Rows
-- moved over below predate undo history and carry 0.
CREATE TABLE design_doc_targets (
    design_id INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    -- Relative to the repository root, under `docs/`.
    path      TEXT NOT NULL,
    txn_id    INTEGER NOT NULL
);

CREATE TABLE design_sources (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    design_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    -- Relative to the repository root.
    path      TEXT NOT NULL,
    -- Lowercase hex blake3 of the content when it was recorded.
    blake3    TEXT NOT NULL,
    txn_id    INTEGER NOT NULL,
    UNIQUE (design_id, path)
);

-- What the metadata keys held, moved over.
INSERT INTO design_doc_targets (design_id, path, txn_id)
SELECT id, json_extract(metadata, '$.doc_target'), 0
  FROM items
 WHERE kind = 'design' AND json_type(metadata, '$.doc_target') = 'text';

INSERT OR IGNORE INTO design_sources (design_id, path, blake3, txn_id)
SELECT i.id, json_extract(s.value, '$.path'), json_extract(s.value, '$.blake3'), 0
  FROM items i, json_each(i.metadata, '$.sources') s
 WHERE i.kind = 'design' AND json_type(i.metadata, '$.sources') = 'array'
   AND json_type(s.value, '$.path') = 'text' AND json_type(s.value, '$.blake3') = 'text';

UPDATE items SET metadata = json_remove(metadata, '$.doc_target', '$.sources')
 WHERE kind = 'design'
   AND (json_type(metadata, '$.doc_target') IS NOT NULL
        OR json_type(metadata, '$.sources') IS NOT NULL);

-- Undo history ends where the keys were written. A changelog entry from before this migration that
-- wrote `doc_target` or `sources` into a design's metadata would, undone, restore a blob nothing reads
-- any more while the rows above stayed — and report the transaction reverted. So the watermark (V014)
-- rises to the newest transaction holding such an entry; later, unrelated work stays undoable. A
-- database that never ran the metadata-key build has no such entry and keeps its watermark.
UPDATE undo_watermark
   SET from_txn = (
       SELECT MAX(c.txn_id) FROM changelog c JOIN items i ON i.id = CAST(c.entity_id AS INTEGER)
        WHERE c.entity_type = 'items' AND i.kind = 'design'
          AND EXISTS (
              SELECT 1 FROM (SELECT CASE WHEN json_valid(c.before)
                                         THEN json_extract(c.before, '$.metadata') END AS m
                             UNION ALL
                             SELECT CASE WHEN json_valid(c.after)
                                         THEN json_extract(c.after, '$.metadata') END) x
               WHERE json_valid(x.m)
                 AND (json_type(x.m, '$.doc_target') IS NOT NULL
                      OR json_type(x.m, '$.sources') IS NOT NULL)))
 WHERE id = 1
   AND from_txn < (
       SELECT COALESCE(MAX(c.txn_id), 0) FROM changelog c
         JOIN items i ON i.id = CAST(c.entity_id AS INTEGER)
        WHERE c.entity_type = 'items' AND i.kind = 'design'
          AND EXISTS (
              SELECT 1 FROM (SELECT CASE WHEN json_valid(c.before)
                                         THEN json_extract(c.before, '$.metadata') END AS m
                             UNION ALL
                             SELECT CASE WHEN json_valid(c.after)
                                         THEN json_extract(c.after, '$.metadata') END) x
               WHERE json_valid(x.m)
                 AND (json_type(x.m, '$.doc_target') IS NOT NULL
                      OR json_type(x.m, '$.sources') IS NOT NULL)));
