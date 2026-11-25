-- A session that files a run request hears its result (the "session that files a run request hears
-- its result" design): delivery state, the warning lines of a run's output, and each Claude Code
-- process's inbox.
--
-- `exec_delivery` is one row per request, made with it by `exec::request` in the same transaction.
-- Every move is one compare-and-set in `exec_delivery.rs`, and a row reaches `delivered` once: that is
-- the whole "never delivered twice" guarantee, and the CHECKs below tie each state to the columns it
-- implies. Like `exec_output` and `claude_sessions` this is observation, not content: it is not
-- changelogged, and `jkb undo` never reaches it.

CREATE TABLE exec_delivery (
    request_id      INTEGER PRIMARY KEY REFERENCES exec_requests(id) ON DELETE CASCADE,
    -- The filing in-process subagent; NULL for the main agent, or when a fold joined requests from
    -- two different agents (the result then goes to the session).
    agent_id        TEXT,
    -- Set when that subagent's `SubagentStop` is released.
    agent_gone_at   INTEGER,
    state           TEXT NOT NULL CHECK (state IN ('waiting', 'sending', 'delivered', 'failed')),
    -- 'sending' only: a deliverer's claim, which lapses.
    lease_until     INTEGER,
    via             TEXT CHECK (via IN ('socket', 'hook', 'backfill')),
    delivered_at    INTEGER,
    attempts        INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    last_attempt_at INTEGER,
    last_error      TEXT,
    CHECK ((state = 'sending') = (lease_until IS NOT NULL)),
    CHECK ((state = 'delivered') = (delivered_at IS NOT NULL AND via IS NOT NULL))
);
CREATE INDEX exec_delivery_open ON exec_delivery (state) WHERE state <> 'delivered';

-- Every request that ENDED before this migration counts as delivered, so the upgrade does not flood
-- sessions with old results. One still pending or running is waiting: its session hears it end.
INSERT INTO exec_delivery (request_id, state, via, delivered_at)
SELECT id, 'delivered', 'backfill', coalesce(ended_at, created_at) FROM exec_requests
WHERE state NOT IN ('pending', 'running');
INSERT INTO exec_delivery (request_id, state)
SELECT id, 'waiting' FROM exec_requests WHERE state IN ('pending', 'running');

-- The `warning:` lines of a run's output, collected by `exec::output` as each chunk arrives (the
-- tail keeps only the last 256 KiB, and a land's early warning can fall out of it). `exec_output` is
-- not changelogged, so adding columns to it is safe.
ALTER TABLE exec_output ADD COLUMN warnings TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(warnings) AND json_type(warnings) = 'array');
ALTER TABLE exec_output ADD COLUMN warnings_dropped INTEGER NOT NULL DEFAULT 0
    CHECK (warnings_dropped >= 0);
-- The unfinished last line of the chunks so far, at most 1 KiB (a longer line is cut).
ALTER TABLE exec_output ADD COLUMN warn_carry BLOB NOT NULL DEFAULT x''
    CHECK (length(warn_carry) <= 1024);
-- The bytes that reached jkb as chunks; less than `total_bytes` means the runner wrote the rest to
-- its log only.
ALTER TABLE exec_output ADD COLUMN streamed_bytes INTEGER NOT NULL DEFAULT 0
    CHECK (streamed_bytes >= 0);
-- An output that ended before this migration was not scanned, and nothing more will reach it: count
-- it as streamed whole, so its result is not reported as reaching jkb only in part. A run still going
-- keeps 0 — what it streamed before was not scanned, and saying so is the truth.
UPDATE exec_output SET streamed_bytes = total_bytes
WHERE request_id IN (SELECT id FROM exec_requests WHERE state NOT IN ('pending', 'running'));

-- Each `claude` process's inbox socket and its token, from the hook's environment. Per (session, pid,
-- instance) row, because each process has its own socket. The token never leaves the database
-- through a listing.
ALTER TABLE claude_sessions ADD COLUMN inbox_socket TEXT;
ALTER TABLE claude_sessions ADD COLUMN inbox_token TEXT;
