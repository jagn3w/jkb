-- The task lifecycle's history, as an append-only log (design S-series, D48).
--
-- Two facts about a piece of work outlive git's memory of it: **where a branch was cut**, and
-- **that it landed**. Git cannot supply either -- after a squash or rebase merge the commits are
-- rewritten, so containment cannot be tested, and a branch that never started is indistinguishable
-- from one whose work was squashed away. `branch_records` (V013) stored both as *properties of a
-- branch*, and that shape is what hurt: a mutable projection of the past, keyed by a name that git
-- lets you delete, recreate and reuse, so it had to be kept in agreement with a moving world. Every
-- piece of machinery around it existed for that reconciliation -- the supersede clause, `landed_head`,
-- the reflog instance anchor, `--forget` -- and each produced its own must-fix findings.
--
-- An append-only log makes **no claim about the present**, so there is nothing to reconcile. "Branch
-- X was cut from C at T" stays true whatever happens to X afterwards; a name that changes hands
-- appends a new row rather than corrupting an old one; and superseding stops being an operation.
--
-- What replaced the *inference* is separate and simpler: a merged pull request. A PR number is
-- minted by GitHub and never reused, so "did this land?" became a lookup on a stable id instead of
-- a question about the commit graph that needed a stored cut point to be answerable at all.
--
-- Deliberately **not** changelogged, and so not undoable. It follows the `blobs` precedent: this
-- table *is* an audit record, and changelogging it would be recording that we recorded something.
-- A transition reverted by `jkb undo` stays here, which is the honest reading of a history -- it did
-- happen, and the undo is itself in the changelog.

CREATE TABLE task_transitions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    -- The transaction the move was applied in, so a reader can line a transition up with the
    -- changelog entries it produced (and with the `undo` marker, if one was later written).
    txn_id       TEXT NOT NULL,
    -- History of a deleted item is not meaningful, and orphan rows would accumulate unbounded.
    -- The cost, stated: `jkb undo` of an item delete restores the item without its history.
    item_id      INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
    at           TEXT NOT NULL,
    -- `jkb_core::lifecycle::TaskEvent::name`. A closed set in code; stored as text so a log
    -- written by an older binary still reads.
    event        TEXT NOT NULL,
    -- NULL only for the very first transition of an item created before this table existed.
    from_status  TEXT,
    to_status    TEXT NOT NULL,
    -- Who acted. `items.claimant_id`'s vocabulary (`jkb_types::AgentId`) -- a process, a session
    -- worktree, or an externally-minted agent id. An attribute of the event, which is the one
    -- place an agent identity is unarguably correct: it is a fact about a moment, not a key.
    agent_id     TEXT,
    -- Descriptive labels, never keys. Nothing looks a transition up by branch name, which is the
    -- whole point of the change: a recycled name cannot make an old row describe new work.
    branch       TEXT,
    onto         TEXT,
    ref_commit   TEXT,
    -- The pull request that proved an external landing, where one did.
    pr_number    INTEGER,
    -- The `TaskFacts` the guard fired on, as JSON. This is what makes `jkb task why` able to say
    -- *why* rather than only *what* -- fourteen must-fix findings in this repository's history are
    -- "held for ever with no way to see the reason".
    evidence     TEXT
);

CREATE INDEX idx_task_transitions_item ON task_transitions (item_id, id);
CREATE INDEX idx_task_transitions_branch ON task_transitions (branch);
