-- Undo history begins here (D47).
--
-- The `changelog` is an **audit log**, and `undo` reads it as an **undo log**. Those are two
-- artifacts with different contracts: an audit entry has to say what happened, an undo entry has
-- to carry enough to put it back. Every entry written before the write-time guards in
-- `undo::check_restorable` met the first contract and was never held to the second, so the log
-- contains payloads that *look* like before-states and restore nothing:
--
--   * `item::set_content` logged `{"content_len": 12}` -- column-shaped, names no column
--   * `binding::mark_synced` logged NULL, on **every file sync**
--   * `ns::set_metadata` logged nothing at all
--   * `edge::unlink` and the two placement deletes logged their *arguments*, not their rows
--   * `ns::move_subtree` described one row of the subtree it moved
--
-- Each is now refused at its writer, which fixes every entry written from here on and **no entry
-- already written**. A write-time guard structurally cannot reach back. What was left was to
-- infer, per entry, whether a legacy payload happens to be restorable -- which is the same
-- mistake one level along, since the inference has to be right about payloads nobody designed to
-- be inverted.
--
-- So the honest boundary is a date line. Undo history begins at the transaction this migration
-- runs after: `undo_last` never selects below it, and an explicit `jkb undo <txn>` below it is
-- told the transaction predates undo history rather than dying part-way through applying it.
--
-- Additive and safe on a populated database: a new one-row table, no existing table altered, no
-- existing row rewritten. On a fresh database `changelog` is empty, so the mark is 0 and nothing
-- is excluded -- the seed is `MAX(txn_id)`, so the boundary is only ever as high as the history
-- that actually exists.

CREATE TABLE undo_watermark (
    -- One row, structurally: a second boundary would be two answers to one question.
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    from_txn INTEGER NOT NULL
);

INSERT INTO undo_watermark (id, from_txn)
SELECT 1, COALESCE(MAX(txn_id), 0) FROM changelog;
