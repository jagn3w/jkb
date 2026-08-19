-- `items.id` is never reused (design D40).
--
-- `id INTEGER PRIMARY KEY` is a rowid alias, and SQLite hands the largest freed rowid to the
-- next insert. The vector index cannot carry a foreign key -- `vec_items_<dim>` is a virtual
-- table, so `ON DELETE CASCADE` is not available to it -- so a deleted item leaves its vector
-- behind, keyed on an id that is then reissued. The next item created *inherits the deleted
-- item's embedding*: vector search returns it for the old text, and `index_pending`'s
-- `id NOT IN (SELECT item_id FROM vec_items_<dim>)` considers it already indexed, so it can
-- never be re-embedded. Ingest also died on the resulting UNIQUE collision, permanently, for
-- every later ingest into that database.
--
-- That was fixed four times as "sweep the orphaned rows", once per call site that deletes:
-- `undo`, then `item rm`, then ingest's re-capture arm, then ingest's fresh-capture arm. Each
-- fix was correct and each was incomplete, because the enforcement was procedural -- every
-- present and future deleter had to remember. `AUTOINCREMENT` removes the mechanism instead:
-- a freed id is never handed out again, so a leftover vector row can no longer be *adopted*.
-- It becomes merely stale, which is a hygiene problem with a bounded cost, cleaned by the
-- explicit sweep in `jkb doctor --fix` / `jkb index --sweep`.
--
-- SQLite cannot `ALTER TABLE … ADD AUTOINCREMENT`, so the table is rebuilt -- the same
-- create-new / copy / DROP / rename that V006 used, and the harness (`migrate::run`) already
-- owns the `PRAGMA foreign_keys = OFF` toggle and the `foreign_key_check` afterwards, because
-- `PRAGMA foreign_keys` is a no-op inside the transaction refinery wraps each migration in.
--
-- Rowids are PRESERVED (`id` is copied explicitly), so the FTS5 external-content index
-- (`fts_items`, content='items') stays aligned and is not rebuilt; only its triggers, which
-- are dropped with the table, are recreated. Copying explicit ids into an AUTOINCREMENT table
-- also seeds `sqlite_sequence` to the maximum id, so the counter continues above every id this
-- database has ever used rather than restarting inside the existing range.
--
-- Columns mirror the CURRENT schema: V006's rebuild plus V007's `resolution`.

CREATE TABLE items_new (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    uid          TEXT NOT NULL UNIQUE,
    kind         TEXT NOT NULL,
    content      TEXT,
    content_hash TEXT UNIQUE,
    mime         TEXT,
    status       TEXT CHECK (
        status IS NULL
        OR status IN ('open', 'in_progress', 'needs_review', 'done', 'cancelled')
    ),
    priority     INTEGER,
    due          TEXT,
    metadata     TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    claimant_id  TEXT,
    claimed_at   TEXT,
    resolution   TEXT CHECK (
        resolution IS NULL
        OR resolution IN ('unresolved', 'success', 'dead_end', 'superseded', 'abandoned')
    )
);

INSERT INTO items_new (
    id, uid, kind, content, content_hash, mime, status, priority, due,
    metadata, created_at, updated_at, claimant_id, claimed_at, resolution
)
SELECT
    id, uid, kind, content, content_hash, mime, status, priority, due,
    metadata, created_at, updated_at, claimant_id, claimed_at, resolution
FROM items;

DROP TABLE items;
ALTER TABLE items_new RENAME TO items;

-- An empty table copies no rows, so nothing seeds `sqlite_sequence`. Seed it explicitly to
-- the highest id any vector table still remembers, so a fresh database that already holds
-- orphaned vector rows (undo ran before this migration) cannot hand those ids out either.
-- `INSERT OR IGNORE` because the copy above may already have created the row.
INSERT OR IGNORE INTO sqlite_sequence (name, seq)
VALUES ('items', (SELECT COALESCE(MAX(id), 0) FROM items));

-- Recreate the indexes dropped with the old table (V001, V007).
CREATE INDEX idx_items_kind_status ON items (kind, status);
CREATE INDEX idx_items_due ON items (due);
CREATE INDEX idx_items_resolution ON items (resolution);

-- Recreate the FTS5 external-content sync triggers dropped with the old table (V002/V006).
-- `fts_items` itself survives the rebuild; rowids are preserved, so its content is intact.
CREATE TRIGGER items_after_insert AFTER INSERT ON items BEGIN
    INSERT INTO fts_items (rowid, content) VALUES (new.id, new.content);
END;

CREATE TRIGGER items_after_delete AFTER DELETE ON items BEGIN
    INSERT INTO fts_items (fts_items, rowid, content) VALUES ('delete', old.id, old.content);
END;

CREATE TRIGGER items_after_update AFTER UPDATE ON items BEGIN
    INSERT INTO fts_items (fts_items, rowid, content) VALUES ('delete', old.id, old.content);
    INSERT INTO fts_items (rowid, content) VALUES (new.id, new.content);
END;
