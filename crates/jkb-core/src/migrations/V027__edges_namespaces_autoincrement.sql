-- `edges.id` and `namespaces.id` are never reused, the treatment V010/V011 gave `items.id`.
--
-- Both were `INTEGER PRIMARY KEY` with no AUTOINCREMENT (V001), and SQLite hands the largest
-- freed rowid to the next insert. `undo` of a delete puts the row back under its logged id, so
-- deleting an edge (`jkb task undepend`) and then linking another gave the new edge the dead
-- one's id, and undoing the delete failed with `UNIQUE constraint failed: edges.id` for as long
-- as the live edge held it. `jkb ns rm` then `jkb ns mk` did the same to `namespaces`. These were
-- the only two tables in the class: every other table a delete writer touches is AUTOINCREMENT
-- already or has no id column for a restore to demand.
--
-- An exemption list in `undo` ("for these tables, restore under a fresh id") was rejected: it
-- is one more rule every future writer would have to remember, and an edge or a namespace
-- restored under a new id breaks every older changelog entry that names the old one.
-- `AUTOINCREMENT` removes the mechanism instead.
--
-- SQLite cannot add AUTOINCREMENT to an existing column, so both tables are rebuilt with the
-- create-new / copy / DROP / rename that V006 and V010 used. The harness (`migrate::run`) owns
-- the `PRAGMA foreign_keys = OFF` toggle and the `foreign_key_check` afterwards, because the
-- pragma is a no-op inside the transaction refinery wraps each migration in, and `mounts` and
-- `placements` reference `namespaces` with `ON DELETE CASCADE` -- a DROP under enforcement would
-- empty them. Ids are PRESERVED (copied explicitly), so every reference to them stays valid.
--
-- Columns, constraints and indexes mirror the CURRENT schema: V001 for both tables plus V007's
-- `edges.weight`, and V021's paged-walk indexes. Neither table has a trigger, and no view reads
-- either one.

CREATE TABLE namespaces_new (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    path       TEXT NOT NULL UNIQUE,
    parent_id  INTEGER REFERENCES namespaces (id),
    kind       TEXT NOT NULL CHECK (kind IN ('logical', 'mount', 'system')),
    metadata   TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

INSERT INTO namespaces_new (id, path, parent_id, kind, metadata, created_at)
SELECT id, path, parent_id, kind, metadata, created_at FROM namespaces;

DROP TABLE namespaces;
ALTER TABLE namespaces_new RENAME TO namespaces;

CREATE INDEX idx_namespaces_parent ON namespaces (parent_id);

CREATE TABLE edges_new (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    src_item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    dst_item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    type        TEXT NOT NULL,
    props       TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    weight      REAL,
    UNIQUE (src_item_id, dst_item_id, type)
);

INSERT INTO edges_new (id, src_item_id, dst_item_id, type, props, created_at, weight)
SELECT id, src_item_id, dst_item_id, type, props, created_at, weight FROM edges;

DROP TABLE edges;
ALTER TABLE edges_new RENAME TO edges;

CREATE INDEX idx_edges_src ON edges (src_item_id, type);
CREATE INDEX idx_edges_dst ON edges (dst_item_id, type);
CREATE INDEX idx_edges_src_id ON edges (src_item_id, id);
CREATE INDEX idx_edges_dst_id ON edges (dst_item_id, id);

-- The high-water mark, set the way V011 set it for `items` rather than the way V010 tried to.
--
-- The explicit-id copy above leaves `sqlite_sequence` at the maximum *surviving* id, which is
-- below an id that was issued and then deleted off the top of the range -- exactly the id a
-- pending `undo` of that delete would restore, and exactly the one a new row would otherwise be
-- handed. The changelog remembers it two ways. Each of the two deleters (`edge::unlink`,
-- `ns::remove`) logs the whole row, `id` included, as its before-state -- the id a pending undo
-- would write back. And an allocating edge insert (`edge::link_weighted`) logs the new id as
-- `entity_id`, which also covers edges that left by cascade with their item. (`ns::ensure` logs
-- no insert, so for namespaces the delete before-states are the record.) Only `op = 'insert'`
-- entity ids are read, because an edge delete's `entity_id` is `src->dst`, not an id. `CAST` of
-- non-numeric text yields 0, so a stray value can only understate the maximum.
--
-- DELETE then INSERT, because `sqlite_sequence` has no unique key to upsert against (V011's
-- finding): an `INSERT OR IGNORE` beside the copy's row would add a second, ignored one.

DELETE FROM sqlite_sequence WHERE name IN ('namespaces', 'edges');

INSERT INTO sqlite_sequence (name, seq)
SELECT 'namespaces', MAX(high_water) FROM (
    SELECT COALESCE(MAX(id), 0) AS high_water FROM namespaces
    UNION ALL
    SELECT COALESCE(MAX(CAST(entity_id AS INTEGER)), 0) FROM changelog
     WHERE entity_type = 'namespaces' AND op = 'insert'
    UNION ALL
    SELECT COALESCE(MAX(CAST(json_extract(before, '$.id') AS INTEGER)), 0) FROM changelog
     WHERE entity_type = 'namespaces' AND op = 'delete' AND json_valid(before)
);

INSERT INTO sqlite_sequence (name, seq)
SELECT 'edges', MAX(high_water) FROM (
    SELECT COALESCE(MAX(id), 0) AS high_water FROM edges
    UNION ALL
    SELECT COALESCE(MAX(CAST(entity_id AS INTEGER)), 0) FROM changelog
     WHERE entity_type = 'edges' AND op = 'insert'
    UNION ALL
    SELECT COALESCE(MAX(CAST(json_extract(before, '$.id') AS INTEGER)), 0) FROM changelog
     WHERE entity_type = 'edges' AND op = 'delete' AND json_valid(before)
);
