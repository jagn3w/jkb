-- A strategy definition's description: one sentence on why to choose it, required by
-- `workflow::store::define` (through `workflow::validate_describe`) for every version written from
-- here on and stored per version, so a redefinition keeps its own.
--
-- Nullable because the versions already defined have none; `workflow.strategies` lists such a
-- version with a fallback naming its graph. `workflow_strategies` is append-only and not
-- changelogged, so no before-state ever names this column.
ALTER TABLE workflow_strategies ADD COLUMN describe TEXT;
