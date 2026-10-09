-- Where a design's export goes, and the files it was made from (design D55.5–6), each in a table of
-- its own rather than as keys of the design item's `metadata`.
--
-- They were metadata keys first, and the changelog logs `metadata` as one column: undoing an older
-- doc-target write put back the whole blob of its time, silently dropping sources recorded since,
-- and could restore a target another design had taken meanwhile — the one-design-per-file rule
-- lived only in the writer, which `jkb undo` does not go through. One row per fact makes each write
-- undo on its own, and the rule a UNIQUE constraint that an undo meets too (it is refused, by name,
-- rather than leaving two designs rendering one file).
--
-- `design_id` is the doc target's rowid, so the changelog keys a target by its design. A design
-- item is never deleted (`item::remove` refuses one), so the cascades below never fire in
-- practice; they keep the tables honest if that ever changes.
CREATE TABLE design_doc_targets (
    design_id INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    -- Relative to the repository root, under `docs/`.
    path      TEXT NOT NULL UNIQUE
);

CREATE TABLE design_sources (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    design_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    -- Relative to the repository root.
    path      TEXT NOT NULL,
    -- Lowercase hex blake3 of the content when it was recorded.
    blake3    TEXT NOT NULL,
    UNIQUE (design_id, path)
);

-- What the metadata keys held, moved over. Two designs naming one target could only arise through
-- the undo defect above; the lowest design id keeps it.
INSERT INTO design_doc_targets (design_id, path)
SELECT MIN(id), json_extract(metadata, '$.doc_target')
  FROM items
 WHERE kind = 'design' AND json_type(metadata, '$.doc_target') = 'text'
 GROUP BY json_extract(metadata, '$.doc_target');

INSERT OR IGNORE INTO design_sources (design_id, path, blake3)
SELECT i.id, json_extract(s.value, '$.path'), json_extract(s.value, '$.blake3')
  FROM items i, json_each(i.metadata, '$.sources') s
 WHERE i.kind = 'design' AND json_type(i.metadata, '$.sources') = 'array'
   AND json_type(s.value, '$.path') = 'text' AND json_type(s.value, '$.blake3') = 'text';

UPDATE items SET metadata = json_remove(metadata, '$.doc_target', '$.sources')
 WHERE kind = 'design'
   AND (json_type(metadata, '$.doc_target') IS NOT NULL
        OR json_type(metadata, '$.sources') IS NOT NULL);
