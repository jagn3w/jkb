-- Containment is a placement, not a derived view (design D35).
--
-- A node that contains others -- a parent task over its subtasks, a document over its
-- chunks -- was previously simulated at read time: the child was placed in the SAME
-- namespace as its container, filtered out of that namespace's listing, and the hierarchy
-- re-derived from `parent_of` / `derived_from` edges. That made listing a special case per
-- relationship and needed a de-duplication rule to stop a child appearing twice.
--
-- Instead a placement says where a node lives: in namespace N, contained by item P.
-- `parent_item_id IS NULL` means it sits directly in the namespace. Listing is then one
-- query against one table, whatever the container is.
--
-- `namespace_id` is deliberately KEPT alongside it. Namespace scoping (`ns:tasks/**`,
-- which `task next` and every scoped query resolve through `placements.namespace_id`)
-- must still find a subtask; dropping the namespace would silently remove contained items
-- from every scoped read.
--
-- ON DELETE SET NULL, not CASCADE: deleting a container must return its children to the
-- namespace, not delete their placement rows. A cascade would make the children invisible
-- rather than merely un-parented -- data loss in the read path.

ALTER TABLE placements
    ADD COLUMN parent_item_id INTEGER
        REFERENCES items (id) ON DELETE SET NULL;

-- Listing a container is `WHERE parent_item_id = ?`, so it needs its own index; the
-- existing idx_placements_ns covers the namespace direction.
CREATE INDEX idx_placements_parent ON placements (parent_item_id, position);

-- Back-fill from the edges that carried containment until now.
--
-- Only the placement that shares the container's namespace is parented. A task mirrored
-- into `tasks/<repo>` keeps a flat row there on purpose: the mirror is an index of every
-- task, and nesting inside it would hide subtasks from the very view built to list them
-- all. Containment is a property of a location, which is why it lives on the placement.

-- Both back-fills are shaped so the OUTER scan is cheap and the INNER lookup is indexed.
-- The obvious formulations are not: `UPDATE ... FROM edges, placements pp` invites the
-- planner to consider a placements x edges product (57k x 60k) and a correlated
-- `SET ... WHERE EXISTS ...` evaluates the same join twice per row. Both failed to finish
-- in five minutes on a real 584 MB database.

-- Chunks: `derived_from` runs chunk -> document. No namespace guard is needed because
-- ingest always places a chunk in its document's namespace, and `role = 'chunk'` is what
-- it places them as -- so the outer scan is already restricted to exactly these rows.
-- Restricted by role rather than by `items.kind` to avoid a join: `derived_from` is a
-- general provenance edge (a note derived from a document is a relationship, not
-- containment) and only chunk placements carry that role.
UPDATE placements
   SET parent_item_id = (
       SELECT e.dst_item_id FROM edges e
        WHERE e.src_item_id = placements.item_id
          AND e.type = 'derived_from'
        LIMIT 1)
 WHERE role = 'chunk'
   AND EXISTS (SELECT 1 FROM edges e
                WHERE e.src_item_id = placements.item_id
                  AND e.type = 'derived_from');

-- Subtasks: `parent_of` runs parent -> child. Here the namespace guard IS needed: a task
-- mirrored into `tasks/<repo>` keeps a flat row there on purpose, because that mirror is an
-- index of every task and nesting inside it would hide subtasks from the view built to list
-- them all. Containment is a property of a location, which is why it lives on the placement.
UPDATE placements
   SET parent_item_id = (
       SELECT e.src_item_id FROM edges e
        WHERE e.dst_item_id = placements.item_id
          AND e.type = 'parent_of'
          AND EXISTS (SELECT 1 FROM placements pp
                       WHERE pp.item_id = e.src_item_id
                         AND pp.namespace_id = placements.namespace_id)
        LIMIT 1)
 WHERE EXISTS (SELECT 1 FROM edges e
                WHERE e.dst_item_id = placements.item_id
                  AND e.type = 'parent_of');
