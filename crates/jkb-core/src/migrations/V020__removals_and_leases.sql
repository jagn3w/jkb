-- The worktree-removal records and the leases, in the database (tasks S6.4 stage 3, decisions B and D;
-- openspec/changes/jkb-message-queue/design-s6-4.md).
--
-- A session a verb could not archive where it ran leaves a record the reap service finishes later
-- (design D49). Those records were files beside the database, which a process reaching the knowledge
-- base through `jkb serve` has no path to; here, every client reads and writes them through the same
-- ops. Paths are written home-relative (`~/repos/…`) wherever they lie under the writer's home, so the
-- host and the dev container — which see the same `~/repos` under different homes — resolve them to the
-- same checkout, and the host's reap service can finish what a container session deferred.
--
-- `written_via` is the backend that wrote the row (`serve` for a client of the daemon), stamped by the
-- server, never sent by the client.
--
-- A lease is a named, exclusive hold: the removal sweep's lock, and `jkb task land`'s per-repo lock.
-- `holder` is `<owner id> <nonce>`; whether a holder is gone is judged by the client that can probe it,
-- and a takeover is a compare-and-set on the holder it judged.
--
-- Deliberately NOT changelogged, like `notify_sessions`: operational state, which `jkb undo` must not
-- resurrect or erase.

CREATE TABLE worktree_removals (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    worktree      TEXT NOT NULL CHECK (length(worktree) > 0),
    repo_root     TEXT NOT NULL CHECK (length(repo_root) > 0),
    branch        TEXT NOT NULL,
    uid           TEXT NOT NULL,
    delete_branch INTEGER NOT NULL DEFAULT 0 CHECK (delete_branch IN (0, 1)),
    accept_dirty  INTEGER NOT NULL DEFAULT 0 CHECK (accept_dirty IN (0, 1)),
    recorded_at   INTEGER NOT NULL,
    head          TEXT,
    archive       TEXT,
    archived_at   INTEGER,
    written_via   TEXT NOT NULL,
    CHECK ((archive IS NULL) = (archived_at IS NULL))
);

CREATE TABLE leases (
    name     TEXT PRIMARY KEY CHECK (length(name) > 0),
    holder   TEXT NOT NULL CHECK (length(holder) > 0),
    taken_at INTEGER NOT NULL
);
