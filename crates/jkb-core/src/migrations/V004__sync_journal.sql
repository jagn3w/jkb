-- Sync journal (design D25): per-file sync state, keyed by the file's `file://` uri.
--
-- Additive only: a new table, a view, and one seeded namespace. No ALTER of an
-- existing table, no virtual tables touched. `document` sync worked with only
-- `bindings.last_synced_hash`; multi-item serializers need file-level state (one row
-- per file, many item bindings), plus a persisted base for three-way merge and a
-- quarantine slot so a parse failure never destroys the last-good items.
CREATE TABLE sync_state (
    uri                  TEXT PRIMARY KEY,             -- 'file://<path>' (bare, no #fragment)
    serializer           TEXT NOT NULL,
    status               TEXT NOT NULL DEFAULT 'ok'
        CHECK (status IN ('ok', 'conflict', 'needs_attention')),
    last_synced_hash     TEXT,                         -- blake3 of the last-synced (rendered) bytes
    base_blob_hash       TEXT,                         -- blobs.hash of those bytes (three-way base)
    parse_error          TEXT,                         -- actionable message when needs_attention
    quarantine_blob_hash TEXT,                         -- blobs.hash of the failing bytes
    updated_at           TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- Surface the journal under the _sys/sync namespace as a view (mirrors _sys/transactions).
CREATE VIEW sys_sync AS
SELECT uri, serializer, status, last_synced_hash, base_blob_hash,
       parse_error, quarantine_blob_hash, updated_at
FROM sync_state;

INSERT INTO namespaces (path, parent_id, kind)
SELECT '_sys/sync', id, 'system' FROM namespaces WHERE path = '_sys';
