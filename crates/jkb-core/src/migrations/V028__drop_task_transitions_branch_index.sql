-- Drop `idx_task_transitions_branch` (V015): no query reads it.
--
-- V015 created it beside its own comment that "nothing looks a transition up by branch name, which
-- is the whole point of the change". That stayed true: the one `SELECT` on `task_transitions`
-- (`transition::history`) filters on `item_id` and orders by `id`, served by
-- `idx_task_transitions_item`, and every other statement on the table is an `INSERT`. So the index
-- only cost a write per transition and a few pages per branch name. Checked by grepping every
-- statement that names the table, and pinned by a test that `EXPLAIN QUERY PLAN`s the history read.

DROP INDEX IF EXISTS idx_task_transitions_branch;
