-- The message queue (design r3.2 Q1-Q4, openspec/changes/jkb-message-queue/design-r3.md).
--
-- Kafka's shape: a TOPIC is a stream with its own retention and size cap; a MESSAGE carries a
-- queue-assigned `seq`, an opaque `key` saying what it is about, a `kind` consumers dispatch on and
-- a JSON `payload`; a consumer GROUP has one committed `position`. There is no server-side filter —
-- a consumer reads the whole topic and skips what it does not want.
--
-- ORDER COMES FROM `seq`, NEVER FROM A CLOCK. `seq` is AUTOINCREMENT, allocated inside the inserting
-- transaction, and SQLite admits one write transaction at a time, holding its lock until commit,
-- across every process on one kernel: a later-committed message always has a higher `seq`. A
-- topic's seqs have gaps (one sequence serves every topic; reaping leaves holes; a rolled-back
-- insert's seq is reused, harmlessly); reordering never happens. Times are metadata for TTL/reaping.
--
-- TIMES ARE INTEGER MILLISECONDS since the Unix epoch, unlike the ISO text elsewhere in this
-- schema, because every use is arithmetic — `expires_at <= now`, idle for N days — and the caller
-- supplies `now`, so tests control the clock.
--
-- Deliberately NOT changelogged, following the `blobs` / `task_transitions` precedent: the queue is
-- transport, not knowledge, and `jkb undo` reaching into it would replay or erase deliveries.

CREATE TABLE mq_topics (
    id                 INTEGER PRIMARY KEY,
    name               TEXT NOT NULL UNIQUE,
    -- A closed set in code (`mq::QueueType`); `log` is the only v1 type.
    type               TEXT NOT NULL CHECK (type IN ('log')),
    max_bytes          INTEGER NOT NULL CHECK (max_bytes > 0),
    max_messages       INTEGER NOT NULL CHECK (max_messages > 0),
    default_ttl_ms     INTEGER CHECK (default_ttl_ms IS NULL OR default_ttl_ms > 0),
    group_idle_ms      INTEGER NOT NULL CHECK (group_idle_ms > 0),
    compact_every_ms   INTEGER NOT NULL CHECK (compact_every_ms > 0),
    created_at         INTEGER NOT NULL,
    compacted_at       INTEGER
);

CREATE TABLE mq_messages (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    topic_id     INTEGER NOT NULL REFERENCES mq_topics(id) ON DELETE CASCADE,
    key          TEXT NOT NULL CHECK (length(key) > 0),
    kind         TEXT NOT NULL CHECK (length(kind) > 0),
    payload      TEXT NOT NULL,
    -- Bytes counted against the topic's cap: key + kind + payload.
    size         INTEGER NOT NULL CHECK (size > 0),
    producer     TEXT NOT NULL,
    enqueued_at  INTEGER NOT NULL,
    expires_at   INTEGER
);

-- Covering for the cap's per-send `COUNT(*)` / `SUM(size)` and for oldest-first reaping.
CREATE INDEX mq_messages_topic_seq ON mq_messages (topic_id, seq, size);

CREATE TABLE mq_groups (
    topic_id      INTEGER NOT NULL REFERENCES mq_topics(id) ON DELETE CASCADE,
    name          TEXT NOT NULL CHECK (length(name) > 0),
    -- Everything with `seq <= position` has been consumed by this group.
    position      INTEGER NOT NULL CHECK (position >= 0),
    created_at    INTEGER NOT NULL,
    last_poll_at  INTEGER,
    last_ack_at   INTEGER,
    PRIMARY KEY (topic_id, name)
);
