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
-- `design_id` is the doc target's rowid, so the changelog keys a target by its design. `repo` is the
-- design's repo (the segment after `designs/` in its primary namespace) when the target was set: a
-- target is a path relative to that repo's checkout, so two repos may each have `docs/a.md`, and
-- uniqueness is per repo.
--
-- The cascades fire on one path: `jkb undo` of a design's create, which deletes the item
-- (`item::remove` refuses a design). The rows go with it unlogged, so that undo must refuse while
-- the design holds rows a later transaction wrote — these tables count as such work for the design
-- guard in `undo.rs`, as the design's own later updates do.
CREATE TABLE design_doc_targets (
    design_id INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    repo      TEXT NOT NULL,
    -- Relative to the repository root, under `docs/`.
    path      TEXT NOT NULL,
    UNIQUE (repo, path)
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

-- What the metadata keys held, moved over. Two designs of one repo naming one target could only
-- arise through the undo defect above; the lowest design id keeps it.
INSERT INTO design_doc_targets (design_id, repo, path)
SELECT MIN(i.id), r.repo, json_extract(i.metadata, '$.doc_target')
  FROM items i
  JOIN (SELECT p.item_id,
               substr(n.path, 9, CASE WHEN instr(substr(n.path, 9), '/') > 0
                                      THEN instr(substr(n.path, 9), '/') - 1
                                      ELSE length(n.path) END) AS repo
          FROM placements p JOIN namespaces n ON n.id = p.namespace_id
         WHERE p.role = 'primary' AND n.path LIKE 'designs/_%') r ON r.item_id = i.id
 WHERE i.kind = 'design' AND json_type(i.metadata, '$.doc_target') = 'text'
 GROUP BY r.repo, json_extract(i.metadata, '$.doc_target');

INSERT OR IGNORE INTO design_sources (design_id, path, blake3)
SELECT i.id, json_extract(s.value, '$.path'), json_extract(s.value, '$.blake3')
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
