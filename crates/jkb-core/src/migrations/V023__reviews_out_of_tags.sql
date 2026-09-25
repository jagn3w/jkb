-- A task's review record moves out of tags (design D52.7, openspec/changes/jkb-rbac-workflows/).
--
-- `reviewed=`, `review=` and `review-waived=` decided the land gate, and a tag is content any writer
-- may set -- the sync engine included, applying a `tasks.md` line an agent in the dev container
-- edited: `#review-waived=x` waived the gate (hole H3). `tag.rs` records why the answer is not a
-- reserved facet. The facts move here, the gate reads only this table, and the tags are deleted so
-- nothing displays a stale copy. A synced file that still carries one re-imports it as ordinary
-- content that nothing trusts.
--
-- Append-only and not changelogged, like `task_transitions`: a review happened.

CREATE TABLE reviews (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id  INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    -- `recorded` a review round; `waived` a `--no-review` landing; `gate_on_host` a landing whose gate
    -- the operator ran on the host rather than in the dev container (`--gate-on-host`, D52.12).
    kind     TEXT NOT NULL CHECK (kind IN ('recorded', 'waived', 'gate_on_host')),
    -- The round's findings namespace; NULL only for a migrated `reviewed=` with no `review=` beside it.
    ns       TEXT,
    sha      TEXT NOT NULL,
    -- The principal that recorded it (`migrated` for rows carried over from tags).
    actor    TEXT NOT NULL,
    txn_id   INTEGER,
    at       TEXT NOT NULL
);
CREATE INDEX idx_reviews_item ON reviews (item_id, id);

-- Every recorded round, in namespace order (the reviewer names them by date and time), at the head
-- the task's `reviewed=` named.
INSERT INTO reviews (item_id, kind, ns, sha, actor, at)
SELECT r.item_id, 'recorded', r.value,
       coalesce((SELECT min(h.value) FROM tag_applications h
                 WHERE h.item_id = r.item_id AND h.facet = 'reviewed'), 'unknown'),
       'migrated', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM tag_applications r WHERE r.facet = 'review'
ORDER BY r.item_id, r.value;

-- A `reviewed=` with no round beside it still says a review was recorded.
INSERT INTO reviews (item_id, kind, ns, sha, actor, at)
SELECT h.item_id, 'recorded', NULL, h.value, 'migrated', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM tag_applications h
WHERE h.facet = 'reviewed'
  AND NOT EXISTS (SELECT 1 FROM tag_applications r WHERE r.item_id = h.item_id AND r.facet = 'review');

INSERT INTO reviews (item_id, kind, ns, sha, actor, at)
SELECT w.item_id, 'waived', NULL, w.value, 'migrated', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM tag_applications w WHERE w.facet = 'review-waived'
ORDER BY w.item_id, w.value;

DELETE FROM tag_applications WHERE facet IN ('reviewed', 'review', 'review-waived');
