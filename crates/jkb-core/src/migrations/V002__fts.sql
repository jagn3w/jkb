-- Full-text index over items.content using FTS5 external-content, kept in sync
-- with the items table by the standard AFTER INSERT/DELETE/UPDATE trigger triad
-- (design D10). `content='items'` + `content_rowid='id'` avoids duplicating text.

CREATE VIRTUAL TABLE fts_items USING fts5 (
    content,
    content='items',
    content_rowid='id',
    tokenize='porter unicode61'
);

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
