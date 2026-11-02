-- Every review finding carries `finding=review` (`reviews::FINDING_TAG`), so a work frontier can
-- leave findings out in its query (`-tag:finding=review`) rather than each reader re-deriving it
-- from a namespace. `task.review_file` tags what it files from here on; this tags what was filed
-- before.
--
-- A finding is: an item `task.review_file` filed (`review_filings`), or a task placed under a
-- repository's `repos/<repo>/codereviews/` -- where `/review-log` mounted a round before filings
-- existed, and the namespace rule the swarm coordinator read until now. The clean-review item a
-- filing makes is tagged too, as `task.review_file` now tags it: it is done, so no frontier offers
-- it either way.
--
-- Not changelogged, like V023's moves: there is no transaction to undo, and the tag is ordinary
-- content an operator may remove.

-- The facet is declared as `tag::apply` declares one (`define_facet`, a string), so the tag rows
-- below have the definition every applied facet has.
INSERT INTO tag_defs (facet, value_kind) VALUES ('finding', 'string')
ON CONFLICT(facet) DO NOTHING;

INSERT OR IGNORE INTO tag_applications (item_id, facet, value)
SELECT f.item_id, 'finding', 'review'
FROM review_filings f
WHERE EXISTS (SELECT 1 FROM items i WHERE i.id = f.item_id);

INSERT OR IGNORE INTO tag_applications (item_id, facet, value)
SELECT DISTINCT p.item_id, 'finding', 'review'
FROM placements p
JOIN namespaces n ON n.id = p.namespace_id
JOIN items i ON i.id = p.item_id AND i.kind = 'task'
WHERE substr(n.path, 1, 6) = 'repos/'
  AND instr(substr(n.path, 7), '/') > 1
  AND substr(n.path, 7 + instr(substr(n.path, 7), '/'), 12) = 'codereviews/';
