-- The Claude Code session registry (tasks S6.4, openspec/changes/jkb-message-queue/design-s6-4.md).
--
-- So a verb can ask whether the session that holds a claim or a lock has ended. One row per PROCESS
-- holding a session: `claude --resume` runs an id in a new process, possibly while another still
-- runs it, so a session is live while any of its rows is, and one process's end or death ends only
-- its own row. Fed by the hook: `SessionStart` makes a row live (reviving an ended one), any other hook
-- event from a process marks it seen (repairing a start that was lost), `SessionEnd` ends it, and a
-- producer's sweep ends one it proved gone. Measured: a killed session or a restarted container sends no
-- `SessionEnd`, so a live row proves nothing on its own — only a session whose rows have all ended
-- is evidence.
--
-- `pid` is the `claude` process as the hook saw it, or '' when it had none it could trust; `instance`
-- is `host[#boot][/pidns]`, where that pid means something.
--
-- Deliberately NOT changelogged, like `notify_sessions`: this is observation, and `jkb undo` reaching
-- into it would revive or end a session nobody touched.

CREATE TABLE claude_sessions (
    session      TEXT NOT NULL CHECK (length(session) > 0),
    pid          TEXT NOT NULL,
    instance     TEXT NOT NULL,
    cwd          TEXT NOT NULL,
    -- NULL when this process was first seen by another event than its start.
    started_at   INTEGER,
    start_source TEXT,
    -- The last event from this process (a start, an end, or any hook event, refreshed at most hourly):
    -- a session's newest row is what the prune ages it by.
    seen_at      INTEGER NOT NULL,
    ended_at     INTEGER,
    end_reason   TEXT,
    PRIMARY KEY (session, pid, instance),
    CHECK ((ended_at IS NULL) = (end_reason IS NULL))
);
