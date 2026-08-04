-- Containment: which item a node lives inside (design D35).
--
-- A node that contains others -- a parent task over its subtasks, a document over its
-- chunks -- was previously simulated at read time: the child was placed in the SAME
-- namespace as its container, filtered out of that namespace's listing, and the hierarchy
-- re-derived from `parent_of` / `derived_from` edges. Listing was then a special case per
-- relationship, plus a de-duplication rule to stop a child appearing twice.
--
-- The fact "X is contained by Y" is a property of X, not of one of X's locations. An item
-- has several placements -- a home plus the `tasks/<repo>` mirror -- so putting the parent
-- on the placement stores one fact N times, free to disagree. It lives here instead, once.
--
-- Its own table rather than an `items` column: `child_item_id` as PRIMARY KEY makes
-- "at most one container" a structural guarantee rather than a convention, the rows are
-- sparse (most items contain nothing), and it keeps a hot, shared table narrow.

CREATE TABLE containment (
    child_item_id  INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    parent_item_id INTEGER NOT NULL    REFERENCES items (id) ON DELETE CASCADE,
    -- Order among siblings. Chunks carry the fragment index so a document reads in order;
    -- subtasks default to 0 and fall back to priority/uid ordering.
    position       INTEGER NOT NULL DEFAULT 0,
    -- A node cannot contain itself. Deeper cycles are refused by `edge::link` when the
    -- relationship is recorded.
    CHECK (child_item_id != parent_item_id)
);

-- Listing a container is `WHERE parent_item_id = ?`, ordered.
CREATE INDEX idx_containment_parent ON containment (parent_item_id, position);

-- Back-fill from the edges that carried containment until now. INSERT ... SELECT over an
-- indexed edge scan, so this stays a single pass.

-- Subtasks: `parent_of` runs parent -> child.
INSERT OR IGNORE INTO containment (child_item_id, parent_item_id, position)
SELECT e.dst_item_id, e.src_item_id, 0
  FROM edges e
 WHERE e.type = 'parent_of'
   AND e.dst_item_id != e.src_item_id;

-- Chunks: `derived_from` runs chunk -> document, and the fragment index is the `chunk`
-- placement's position. Restricted to chunks because `derived_from` is a general
-- provenance edge -- a note derived from a document is a relationship, not containment.
INSERT OR IGNORE INTO containment (child_item_id, parent_item_id, position)
SELECT e.src_item_id,
       e.dst_item_id,
       COALESCE((SELECT p.position FROM placements p
                  WHERE p.item_id = e.src_item_id AND p.role = 'chunk' LIMIT 1), 0)
  FROM edges e
  JOIN items i ON i.id = e.src_item_id
 WHERE e.type = 'derived_from'
   AND i.kind = 'chunk'
   AND e.src_item_id != e.dst_item_id;
