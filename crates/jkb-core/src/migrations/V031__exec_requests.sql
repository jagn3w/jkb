-- Run requests: a session blocked by its own permissions asks the operator to run one command for
-- it, on the host or in the dev container outside the agent's sandbox (the Executions design).
--
-- A request is a typed record, never a shell string: `argv` is a JSON array run without a shell,
-- `dir` the requester's realpath, and the tree pin (`pin_head`, `pin_changes`) the repository the
-- operator reviews — the run refuses if it changed. Everything here is decided by `exec.rs`, the one
-- writer; the CHECKs below are what make its rules schema facts rather than conventions:
--
--   * every state move is one compare-and-set on `state`, so a request runs at most once;
--   * the columns a state implies are tied to it (`running`/`finished` have a start, `finished` an
--     outcome, `refused` a reason);
--   * an identical pending request from the same session folds into the existing one through the
--     partial unique index, never queues twice.
--
-- `exec_requests` is changelogged (it is the audit record of what ran and who asked), and `jkb undo`
-- refuses every entry of it by name: undo cannot un-run a command or un-ask a request. `exec_output`
-- is not changelogged, like `blobs` and `task_transitions`, so a 256 KiB tail never enters a
-- changelog before/after state.
--
-- `id` is AUTOINCREMENT so a request id is never reused: the wire id is `exec-<id>`, and the request's
-- message-queue topic is `exec/<id>`.

CREATE TABLE exec_requests (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at    INTEGER NOT NULL,
    expires_at    INTEGER NOT NULL,
    session       TEXT NOT NULL CHECK (length(session) > 0),
    agent_type    TEXT,
    role          TEXT NOT NULL CHECK (length(role) > 0),
    task_id       INTEGER,
    target        TEXT NOT NULL CHECK (target IN ('host', 'container')),
    dir           TEXT NOT NULL CHECK (substr(dir, 1, 1) = '/'),
    dir_side      TEXT NOT NULL CHECK (dir_side IN ('host', 'container')),
    argv          TEXT NOT NULL CHECK (json_valid(argv) AND json_type(argv) = 'array'
                                       AND json_array_length(argv) >= 1),
    reason        TEXT NOT NULL,
    pin_head      TEXT,
    pin_changes   TEXT,
    timeout_ms    INTEGER NOT NULL CHECK (timeout_ms > 0),
    state         TEXT NOT NULL CHECK (state IN ('pending', 'running', 'finished', 'declined',
                                                 'refused', 'expired')),
    refuse_reason TEXT CHECK (refuse_reason IN ('tree_changed', 'audit_refused', 'dir_gone')),
    note          TEXT,
    runner        TEXT,
    started_at    INTEGER,
    ended_at      INTEGER,
    exit_status   INTEGER,
    outcome       TEXT CHECK (outcome IN ('exited', 'signalled', 'timed_out', 'interrupted')),
    -- A pin is both halves or neither: a directory outside any repository has none.
    CHECK ((pin_head IS NULL) = (pin_changes IS NULL)),
    CHECK (state NOT IN ('running', 'finished') OR (started_at IS NOT NULL AND runner IS NOT NULL)),
    CHECK (state <> 'finished' OR (outcome IS NOT NULL AND ended_at IS NOT NULL)),
    CHECK (outcome IS NULL OR state = 'finished'),
    CHECK (outcome NOT IN ('exited', 'signalled') OR exit_status IS NOT NULL),
    CHECK ((state = 'refused') = (refuse_reason IS NOT NULL)),
    CHECK (state <> 'pending' OR (started_at IS NULL AND ended_at IS NULL))
);

-- The fold: at most one pending request per (session, argv, dir, target, pin).
CREATE UNIQUE INDEX exec_requests_pending_fold ON exec_requests (
    session, argv, dir, target, ifnull(pin_head, ''), ifnull(pin_changes, '')
) WHERE state = 'pending';

-- Lazy expiry and the per-session sweep read pending rows only.
CREATE INDEX exec_requests_pending ON exec_requests (expires_at) WHERE state = 'pending';
CREATE INDEX exec_requests_session ON exec_requests (session, state);

CREATE TABLE exec_output (
    request_id  INTEGER PRIMARY KEY REFERENCES exec_requests(id) ON DELETE CASCADE,
    tail        BLOB NOT NULL DEFAULT x'' CHECK (length(tail) <= 262144),
    total_bytes INTEGER NOT NULL DEFAULT 0 CHECK (total_bytes >= 0),
    truncated   INTEGER NOT NULL DEFAULT 0 CHECK (truncated IN (0, 1)),
    chunks      INTEGER NOT NULL DEFAULT 0 CHECK (chunks >= 0),
    log_path    TEXT
);
