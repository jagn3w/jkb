-- Shared-core schema for investigation-coordination namespaces (design Dmem.3/Dmem.4).
--
-- Two additive columns. Nothing is back-filled: all-NULL means every existing row
-- behaves exactly as before, so tasks, sync, and search are untouched.
--
-- 1. `items.resolution` — the OUTCOME axis, orthogonal to `status`. `status` answers
--    "how far along?"; `resolution` answers "how did it end?". It is a real indexed
--    column (not a tag) because the frontier and anti-retread queries filter on it hot.
--    NULL is read as `unresolved`, so a memory node needs no back-fill and a task never
--    has to carry one. `dead_end`/`superseded` rows are RETAINED forever (never deleted)
--    and linked to whatever killed them — the graveyard is the memory.
--
-- 2. `edges.weight` — an optional signed magnitude for evidence edges
--    (`supports`/`contradicts`). NULL means "unweighted"; readers treat it as 1.0.
--
-- `ALTER TABLE … ADD COLUMN` with a CHECK constraint is permitted by SQLite (only
-- PRIMARY KEY / UNIQUE / non-constant defaults are not) and the constraint applies to
-- every subsequent insert and update. Neither column is touched by the FTS5
-- external-content triggers (they key on `content`), so `fts_items` is unaffected.
ALTER TABLE items ADD COLUMN resolution TEXT CHECK (
    resolution IS NULL
    OR resolution IN ('unresolved', 'success', 'dead_end', 'superseded', 'abandoned')
);
CREATE INDEX idx_items_resolution ON items (resolution);

ALTER TABLE edges ADD COLUMN weight REAL;
