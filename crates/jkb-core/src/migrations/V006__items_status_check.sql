-- Typed `items.status` at the storage boundary (design D29.1). `status` was a free-form
-- nullable TEXT column; constrain it to NULL (non-task kinds) or one of the five canonical
-- `TaskStatus` strings. SQLite cannot `ALTER TABLE … ADD CONSTRAINT`, so we rebuild the
-- table.
--
-- The migration harness (`migrate::run`) runs migrations with `PRAGMA foreign_keys = OFF`
-- and a `foreign_key_check` afterward, because `items` has `ON DELETE CASCADE` children
-- (bindings, placements, edges, tag_applications) and `DROP TABLE` under FK enforcement
-- would cascade-delete them. `PRAGMA foreign_keys` is a no-op inside a transaction, so the
-- toggle cannot live here — it is owned by the harness (see `migrate.rs`).
--
-- `items_new` mirrors the CURRENT schema: the V001 columns plus the V005 claim columns
-- (`claimant_id`, `claimed_at`). Rowids are preserved (explicit `id`), so the FTS5
-- external-content index (`fts_items`, content='items') stays aligned and is NOT rebuilt;
-- only the triggers, which are dropped with the table, are recreated.

CREATE TABLE items_new (
    id           INTEGER PRIMARY KEY,
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
    claimed_at   TEXT
);

INSERT INTO items_new (
    id, uid, kind, content, content_hash, mime, status, priority, due,
    metadata, created_at, updated_at, claimant_id, claimed_at
)
SELECT
    id, uid, kind, content, content_hash, mime, status, priority, due,
    metadata, created_at, updated_at, claimant_id, claimed_at
FROM items;

DROP TABLE items;
ALTER TABLE items_new RENAME TO items;

-- Recreate the indexes that were dropped with the old table (from V001).
CREATE INDEX idx_items_kind_status ON items (kind, status);
CREATE INDEX idx_items_due ON items (due);

-- Recreate the FTS5 external-content sync triggers dropped with the old table (from V002).
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
