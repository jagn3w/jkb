-- The Claude Code session registry (tasks S6.4, openspec/changes/jkb-message-queue/design-s6-4.md).
--
-- One row per Claude Code session the hook has seen, so a verb can ask whether the session that
-- holds a claim or a lock has ended. Fed by the hook: `SessionStart` makes a row live (a `resume`
-- revives an ended one), `SessionEnd` ends it, and a producer's sweep ends one it proved gone.
-- Measured: a killed session or a restarted container sends no `SessionEnd`, so a live row proves
-- nothing on its own — only an ended one is evidence.
--
-- `pid` and `instance` say which process holds the id right now (`claude --resume` keeps the id and
-- runs a new process): an end or a sweep verdict applies only while the row still names the process
-- it was about. `pid` is the `claude` process as the hook saw it, or '' when it had none it could
-- trust; `instance` is `host[#boot][/pidns]`, where that pid means something.
--
-- Deliberately NOT changelogged, like `notify_sessions`: this is observation, and `jkb undo` reaching
-- into it would revive or end a session nobody touched.

CREATE TABLE claude_sessions (
    session      TEXT PRIMARY KEY CHECK (length(session) > 0),
    pid          TEXT NOT NULL,
    instance     TEXT NOT NULL,
    cwd          TEXT NOT NULL,
    -- NULL when the first event seen was the end (a session started before the hook shipped).
    started_at   INTEGER,
    start_source TEXT,
    ended_at     INTEGER,
    end_reason   TEXT,
    CHECK ((ended_at IS NULL) = (end_reason IS NULL)),
    CHECK (started_at IS NOT NULL OR ended_at IS NOT NULL)
);
