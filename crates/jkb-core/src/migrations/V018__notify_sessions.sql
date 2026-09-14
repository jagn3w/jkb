-- The permission-notification record, moved from a marker file in the hook's `$TMPDIR` into the
-- database the daemon owns (design r3.2 N1, openspec/changes/jkb-message-queue/design-r3.md).
--
-- One row per session with a notification on screen — the state `jkb_core::notify`'s machine reads.
-- A row exists only in the two posted states; `absent` is the absence of a row. The machine's
-- effects on the SCREEN become `claude/notify` messages written in the same transaction as this row,
-- so the record and what was sent can no longer disagree the way a marker file and a subprocess could.
--
-- `owner` and `instance` are for the `SessionStart` sweep, which runs in the PRODUCER's process
-- (only it can probe the pid): `owner` is the `claude` process's pid as the hook saw it, `instance`
-- names where that number means something: `host[#boot][/pidns]`, built and compared by the hook
-- (`instance_from` / `verdict` in crates/jkb-cli/src/notify.rs; docs/notifications.md). Opaque here.
--
-- Deliberately NOT changelogged, like the queue itself: this is transport state, and `jkb undo`
-- reaching into it would resurrect or erase a notification.

CREATE TABLE notify_sessions (
    session     TEXT PRIMARY KEY CHECK (length(session) > 0),
    -- The tool the prompt named, or '' when it named none (the `awaiting_user` state).
    tool        TEXT NOT NULL,
    owner       TEXT NOT NULL,
    instance    TEXT NOT NULL,
    updated_at  INTEGER NOT NULL
);
