-- Indexes a paged edge walk reads by (tasks S6.4 stage 5): `edge::walk_limited` reads a node's edges in
-- id order a page at a time, `WHERE src_item_id = ? AND id > ? ORDER BY id LIMIT ?` (and the same on
-- `dst_item_id`). The (endpoint, type) indexes V001 made serve that only by reading every edge of the
-- node for each page, so a client's walk past a hub cost its degree per page. Additive.
CREATE INDEX IF NOT EXISTS idx_edges_src_id ON edges (src_item_id, id);
CREATE INDEX IF NOT EXISTS idx_edges_dst_id ON edges (dst_item_id, id);
