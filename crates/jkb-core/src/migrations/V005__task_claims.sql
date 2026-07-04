-- Agent-claim state (design D27.1): a claim is a property of the task row, not a
-- side table, so acquisition is a compare-and-swap and the ready frontier is a plain
-- column predicate. Two nullable columns on `items`:
--
--   claimant_id  -- NULL = unclaimed; else a LIVENESS-CHECKABLE owner id (host:pid+run)
--   claimed_at   -- when the claim was taken; NULL when unclaimed
--
-- Additive `ALTER TABLE items ADD COLUMN` on a regular table (not a virtual one), so
-- it is safe. No back-fill: all-NULL means every existing task is unclaimed and
-- behaves exactly as before. There is deliberately NO expiry/TTL column — claim
-- liveness is by owner-existence, never by age (a paused-but-alive agent is never
-- reclaimed). The FTS triggers key on `content`, which claims never touch, so the
-- external-content index is unaffected.
ALTER TABLE items ADD COLUMN claimant_id TEXT;
ALTER TABLE items ADD COLUMN claimed_at  TEXT;
