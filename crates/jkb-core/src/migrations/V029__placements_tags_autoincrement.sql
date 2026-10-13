-- `placements` and `tag_applications` get an `id INTEGER PRIMARY KEY AUTOINCREMENT`, so a row id
-- either table has issued is never issued again -- the treatment V010/V011 gave `items` and V027
-- gave `edges` and `namespaces`.
--
-- Both were V001 tables keyed only by their natural key (`(item_id, namespace_id, role)` and
-- `(item_id, facet, value)`), so their row identity was SQLite's implicit rowid, and SQLite hands
-- the largest freed rowid to the next insert. Every changelog entry for either table is keyed by
-- that rowid (`entity_id`), and `undo` addresses rows by it. So: place A (rowid 1), remove A, place
-- B (handed rowid 1), undo the removal (A back under rowid 2), then undo A's placement -- its
-- `DELETE ... WHERE rowid = 1` removed B's placement and reported success. The same with tags, by
-- the direct removals (`placement::unplace`, `tag::remove`) and by an item delete's cascade
-- snapshot, which captured no rowid at all.
--
-- Keying these two tables' changelog entries on the natural key instead was rejected: `undo`
-- addresses every row it restores or deletes by `rowid`, and a per-table key column list is what
-- `undo::restore_columns` records having tried and removed; `tag::rename_facet` also rewrites the
-- natural key itself, so an entry keyed on it would name a row by a key the row no longer has.
-- An explicit unreusable id keeps the one addressing rule and removes the mechanism, as V027 did.
--
-- SQLite cannot add a PRIMARY KEY column to an existing table, so both are rebuilt with the
-- create-new / copy / DROP / rename that V006, V010 and V027 used. The natural key becomes a
-- UNIQUE constraint, which `placement::place`'s and `tag::apply`'s `ON CONFLICT(...)` upserts
-- name. Each row's id is its old rowid (copied explicitly), so every changelog entry already
-- keyed by one still addresses the same row. Neither table is referenced by a foreign key, has a
-- trigger, or is read by a view; the harness (`migrate::run`) owns the `foreign_keys` toggle.
--
-- Columns, constraints and indexes mirror the CURRENT schema, which is V001's for both.

CREATE TABLE placements_new (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id      INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    namespace_id INTEGER NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    role         TEXT NOT NULL,
    position     INTEGER NOT NULL DEFAULT 0,
    metadata     TEXT NOT NULL DEFAULT '{}',
    UNIQUE (item_id, namespace_id, role)
);

INSERT INTO placements_new (id, item_id, namespace_id, role, position, metadata)
SELECT rowid, item_id, namespace_id, role, position, metadata FROM placements;

DROP TABLE placements;
ALTER TABLE placements_new RENAME TO placements;

CREATE INDEX idx_placements_ns ON placements (namespace_id, role, position);
CREATE INDEX idx_placements_item ON placements (item_id);

CREATE TABLE tag_applications_new (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    facet   TEXT NOT NULL,
    value   TEXT NOT NULL DEFAULT '',
    props   TEXT NOT NULL DEFAULT '{}',
    UNIQUE (item_id, facet, value)
);

INSERT INTO tag_applications_new (id, item_id, facet, value, props)
SELECT rowid, item_id, facet, value, props FROM tag_applications;

DROP TABLE tag_applications;
ALTER TABLE tag_applications_new RENAME TO tag_applications;

CREATE INDEX idx_tag_applications_facet ON tag_applications (facet, value, item_id);
CREATE INDEX idx_tag_applications_item ON tag_applications (item_id);

-- The high-water mark, set as V011 and V027 set theirs: above every rowid the changelog has
-- ever keyed an entry for either table by, not just above the surviving rows. A rowid deleted
-- off the top of the range is still named by older entries -- the `insert` whose inverse deletes
-- it, an `update` whose inverse rewrites it, a placement `delete` -- and a new row handed it would
-- be the row those entries then act on. Every op is read: `tag::remove` logged the item id rather
-- than the row id as its delete key, which can only overstate the maximum, and an overstated mark
-- skips ids, which is harmless. `CAST` of non-numeric text yields 0, so a stray value can only
-- understate it.
--
-- DELETE then INSERT, because `sqlite_sequence` has no unique key to upsert against (V011).

DELETE FROM sqlite_sequence WHERE name IN ('placements', 'tag_applications');

INSERT INTO sqlite_sequence (name, seq)
SELECT 'placements', MAX(high_water) FROM (
    SELECT COALESCE(MAX(id), 0) AS high_water FROM placements
    UNION ALL
    SELECT COALESCE(MAX(CAST(entity_id AS INTEGER)), 0) FROM changelog
     WHERE entity_type = 'placements'
);

INSERT INTO sqlite_sequence (name, seq)
SELECT 'tag_applications', MAX(high_water) FROM (
    SELECT COALESCE(MAX(id), 0) AS high_water FROM tag_applications
    UNION ALL
    SELECT COALESCE(MAX(CAST(entity_id AS INTEGER)), 0) FROM changelog
     WHERE entity_type = 'tag_applications'
);
