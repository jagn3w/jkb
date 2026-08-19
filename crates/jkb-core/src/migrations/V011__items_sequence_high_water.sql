-- Restore the `items` id high-water mark that V010 was supposed to set (design D42.1).
--
-- V010 made `items.id` AUTOINCREMENT so a freed id is never handed out again, and then seeded
-- `sqlite_sequence` in a way that does not work. Two independent defects, both reproduced:
--
--   1. `INSERT OR IGNORE INTO sqlite_sequence` cannot ignore. That table is
--      `CREATE TABLE sqlite_sequence(name, seq)` -- no primary key, no unique index, nothing to
--      conflict on -- so the statement always inserts, leaving TWO ('items', N) rows. SQLite
--      reads the first and silently ignores the rest.
--   2. It seeded from `MAX(id) FROM items` -- the maximum *surviving* id, not the maximum ever
--      used. Deleting the top of the range (which is exactly what `jkb undo` after an ingest
--      does) therefore reset the counter BELOW ids that had already been issued.
--
-- Observed consequence: after V010, ids freed at the top were reissued, and because a
-- `vec_items_<dim>` row outlives its item (a vec0 virtual table can carry no foreign key), the
-- new item silently inherited the deleted item's embedding -- vector search returned it for the
-- old text, and `index_pending` read it as already indexed so it could never be re-embedded.
-- `jkb doctor` reported `ok`, because an orphan whose id has been reused points at a live item.
--
-- The fix: recompute the high-water mark from a source that remembers ids the table no longer
-- holds, and REPLACE the row rather than inserting beside it.
--
-- `changelog` is that source. Every allocating insert into `items` records the new id
-- (`item::upsert`, `task::create`, `view::save` are the only three, and all changelog the id
-- they return), `undo` appends rather than deleting, and nothing in the tree ever deletes from
-- `changelog` or VACUUMs. So no vector table can hold an id the changelog has forgotten.
--
-- `CAST(entity_id AS INTEGER)` is safe on non-numeric text: SQLite yields 0 rather than erroring,
-- so a stray value can only understate the maximum, never overstate it.
--
-- The `MAX(id) FROM items` term is strictly redundant -- SQLite allocates
-- `max(sqlite_sequence.seq, MAX(rowid)) + 1`, so a seed below the table's own maximum can never
-- collide -- but it is kept because it states the intent, and because it is the correct answer
-- on a database whose changelog was truncated by some future tool.
--
-- DELETE first: this removes BOTH rows V010 left, and there is no unique index to upsert against.

DELETE FROM sqlite_sequence WHERE name = 'items';

INSERT INTO sqlite_sequence (name, seq)
SELECT 'items', MAX(high_water) FROM (
    SELECT COALESCE(MAX(id), 0) AS high_water FROM items
    UNION ALL
    SELECT COALESCE(MAX(CAST(entity_id AS INTEGER)), 0) AS high_water
      FROM changelog WHERE entity_type = 'items'
);
