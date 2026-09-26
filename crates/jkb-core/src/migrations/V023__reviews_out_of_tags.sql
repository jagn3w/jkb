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
    -- No cascade from `items` (see V022): a review happened, and `item rm` + `undo` must not give
    -- back a task that reads as never reviewed.
    item_id  INTEGER NOT NULL,
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

-- What `task.review_file` filed into each findings namespace. A round a non-operator records must be
-- one of these, and holds exactly these items: a namespace a caller merely NAMED (`tasks`) would
-- otherwise put every task under it into the recording task's scope (D52.4).
CREATE TABLE review_filings (
    ns       TEXT NOT NULL,
    item_id  INTEGER NOT NULL,
    PRIMARY KEY (ns, item_id)
);

-- Each review round as it stood when it was FIRST recorded. The land gate's last-round clause and the
-- workflow's guards read this, never the findings' live priority, placement or `area=` tags: those
-- are ordinary task content, and the implementer under review could lower its own last round's
-- must-fix, or file a line into an older clean round so that round sorted newest. `id` is the
-- recording order, which is what "the last round" means.
CREATE TABLE review_rounds (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    ns           TEXT NOT NULL UNIQUE,
    actor        TEXT NOT NULL,
    recorded_at  TEXT NOT NULL
);
CREATE TABLE review_round_findings (
    round_id  INTEGER NOT NULL REFERENCES review_rounds(id),
    item_id   INTEGER NOT NULL,
    must_fix  INTEGER NOT NULL CHECK (must_fix IN (0, 1)),
    -- The file the finding names, for the repeated-area rule. NULL for a migrated round.
    area      TEXT,
    PRIMARY KEY (round_id, item_id)
);

-- Every recorded round, in namespace order (the reviewer names them by date and time), at the head
-- the task's `reviewed=` named. ONLY where a `reviewed=` stands beside it: /review-log also tags a
-- backlog finding `review=<ns>` as a trail, with no `reviewed=`, and so does a forged `#review=` line
-- synced from a tasks.md -- migrating those made a task the old gate called never-reviewed pass as
-- reviewed. Those tags stay, as the ordinary content they always were.
INSERT INTO reviews (item_id, kind, ns, sha, actor, at)
SELECT r.item_id, 'recorded', r.value,
       (SELECT min(h.value) FROM tag_applications h
        WHERE h.item_id = r.item_id AND h.facet = 'reviewed'),
       'migrated', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM tag_applications r
WHERE r.facet = 'review'
  AND EXISTS (SELECT 1 FROM tag_applications h WHERE h.item_id = r.item_id AND h.facet = 'reviewed')
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

-- Snapshot every migrated round as it stands now, oldest filing first (the highest finding id, the
-- order the gate used before). A round's findings are the tasks placed in its namespace's subtree.
CREATE TEMP TABLE v023_round_members AS
SELECT r.ns AS ns, i.id AS item_id, i.priority AS priority
FROM (SELECT DISTINCT ns FROM reviews WHERE kind = 'recorded' AND ns IS NOT NULL) r
JOIN namespaces n ON n.path = r.ns OR substr(n.path, 1, length(r.ns) + 1) = r.ns || '/'
JOIN placements p ON p.namespace_id = n.id
JOIN items i ON i.id = p.item_id AND i.kind = 'task';

INSERT INTO review_rounds (ns, actor, recorded_at)
SELECT ns, 'migrated', strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
FROM v023_round_members GROUP BY ns ORDER BY max(item_id), ns;

INSERT OR IGNORE INTO review_round_findings (round_id, item_id, must_fix, area)
SELECT rr.id, m.item_id, (m.priority IS NOT NULL AND m.priority <= 1), NULL
FROM v023_round_members m JOIN review_rounds rr ON rr.ns = m.ns;

DROP TABLE v023_round_members;

-- The migrated tags go; a `review=` that was not migrated stays.
DELETE FROM tag_applications WHERE facet IN ('reviewed', 'review-waived');
DELETE FROM tag_applications
WHERE facet = 'review'
  AND item_id IN (SELECT item_id FROM reviews WHERE kind = 'recorded' AND actor = 'migrated');
