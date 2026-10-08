-- Design documents as CRDTs (design D53.4, docs/code-factory.md).
--
-- A design is an item (`kind = 'design'`); its body is a Yjs document, and the document IS the
-- merge of the updates below. Nothing stores the text: `jkb design cat` renders it from them.
--
-- `design_updates` is append-only. A row is never edited, and `jkb undo` of one appends a NEW row
-- that reverts it rather than deleting the old one: every peer that merged an update has it for
-- good, so removing it from the table would make the table disagree with every editor holding the
-- document. `update_v1` is a Yjs v1 update (`Y.encodeStateAsUpdate` / `Y.applyUpdate`'s format);
-- the design record calls the column `update`, which is an SQL keyword.
--
-- `seq` orders a design's updates. The order is not needed for the merge (Yjs updates commute) but
-- it is what a version is: a design at version `n` is its snapshot plus every update with
-- `seq <= n`, which is how an edit is resolved against the text its author read.
--
-- `id` is AUTOINCREMENT because the changelog names an update by it, and a compaction deletes
-- rows: with a plain rowid the next update would be handed a compacted row's id, and `jkb undo` of
-- the compacted update would revert that newer one instead (measured: the compaction test did
-- exactly this before the column existed). The same reason `items.id` is AUTOINCREMENT (V010).
CREATE TABLE design_updates (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    design_id  INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL CHECK (seq > 0),
    update_v1  BLOB NOT NULL,
    actor      TEXT NOT NULL,
    txn_id     INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (design_id, seq)
);

-- The compaction: the whole document through `seq`, as one update. The updates it covers are
-- deleted when it is written, so a version older than `seq` can no longer be rebuilt and an edit
-- against one is refused (re-read and retry). One row per design: only the newest matters.
--
-- `state_v1` is encoded from a document with garbage collection OFF, so deleted text keeps its
-- content — a revert puts it back from there, and span state compares against it.
CREATE TABLE design_snapshots (
    design_id  INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    seq        INTEGER NOT NULL CHECK (seq > 0),
    state_v1   BLOB NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
