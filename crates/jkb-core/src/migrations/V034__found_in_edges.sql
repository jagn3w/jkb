-- Every review finding names the task it was found in with a `found_in` edge (finding -> task),
-- for display only: the Tasks tab lists a task's findings under it. `jkb_core::reviews::link_findings`
-- writes one from here on, from a filing (`task.review_file`) and from a recording
-- (`task.review_record`); this writes them for the rounds recorded before.
--
-- A finding of a recorded round is linked to every task that recorded the round -- what a recording
-- now links, so live data and this backfill agree. A filing that was never recorded has no task to
-- link (`review_filings` names none), so it gets no edge here. The `JOIN items` lines skip a deleted
-- finding or task: `reviews` and `review_round_findings` do not cascade. A finding that is the
-- recording task itself is skipped (no self-loop), and an edge already present is left as it is.
--
-- No gate reads the edge: the frontier follows `depends_on`, subtasks and step counts follow
-- containment, and the land gate reads the round snapshot.
--
-- Not changelogged, like V033's finding tags: there is no transaction to undo.

INSERT OR IGNORE INTO edges (src_item_id, dst_item_id, type)
SELECT DISTINCT f.item_id, v.item_id, 'found_in'
  FROM reviews v
  JOIN review_rounds r ON r.ns = v.ns
  JOIN review_round_findings f ON f.round_id = r.id
  JOIN items fi ON fi.id = f.item_id
  JOIN items ti ON ti.id = v.item_id
 WHERE v.kind = 'recorded' AND f.item_id <> v.item_id;
