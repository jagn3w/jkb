-- Core relational schema for the jkb virtual filesystem (design D3, D4, D11).

-- Namespaces: the logical tree (adjacency + display path). Paths are logical
-- addresses, never filesystem paths.
CREATE TABLE namespaces (
    id         INTEGER PRIMARY KEY,
    path       TEXT NOT NULL UNIQUE,
    parent_id  INTEGER REFERENCES namespaces (id),
    kind       TEXT NOT NULL CHECK (kind IN ('logical', 'mount', 'system')),
    metadata   TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_namespaces_parent ON namespaces (parent_id);

-- Items: the atomic knowledge/graph node. `id` is the rowid (required by FTS5
-- external-content and sqlite-vec); `uid` is the stable string identity;
-- `content_hash` is globally unique so identical content dedups to one item.
-- status/priority/due are real columns because tasks sort and filter on them.
CREATE TABLE items (
    id           INTEGER PRIMARY KEY,
    uid          TEXT NOT NULL UNIQUE,
    kind         TEXT NOT NULL,
    content      TEXT,
    content_hash TEXT UNIQUE,
    mime         TEXT,
    status       TEXT,
    priority     INTEGER,
    due          TEXT,
    metadata     TEXT NOT NULL DEFAULT '{}',
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    updated_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);
CREATE INDEX idx_items_kind_status ON items (kind, status);
CREATE INDEX idx_items_due ON items (due);

-- Bindings: where an item's bytes live + sync bookkeeping (design D3, D24, D25).
CREATE TABLE bindings (
    item_id          INTEGER PRIMARY KEY REFERENCES items (id) ON DELETE CASCADE,
    uri              TEXT NOT NULL DEFAULT 'managed:',
    sync_mode        TEXT,
    serializer       TEXT,
    last_synced_hash TEXT,
    last_synced_at   TEXT
);

-- Mounts: bind a namespace subtree to an external backing root + serializer.
CREATE TABLE mounts (
    namespace_id    INTEGER PRIMARY KEY REFERENCES namespaces (id) ON DELETE CASCADE,
    backing_uri     TEXT NOT NULL,
    sync_mode       TEXT NOT NULL,
    serializer      TEXT NOT NULL DEFAULT 'document',
    include_glob    TEXT,
    exclude_glob    TEXT,
    conflict_policy TEXT NOT NULL DEFAULT 'manual'
);

-- Placements: item <-> namespace (many-to-many) — "indexed under multiple paths".
CREATE TABLE placements (
    item_id      INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    namespace_id INTEGER NOT NULL REFERENCES namespaces (id) ON DELETE CASCADE,
    role         TEXT NOT NULL,
    position     INTEGER NOT NULL DEFAULT 0,
    metadata     TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (item_id, namespace_id, role)
);
CREATE INDEX idx_placements_ns ON placements (namespace_id, role, position);
CREATE INDEX idx_placements_item ON placements (item_id);

-- Edges: typed directed graph. Indexed in both directions for traversal.
CREATE TABLE edges (
    id          INTEGER PRIMARY KEY,
    src_item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    dst_item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    type        TEXT NOT NULL,
    props       TEXT NOT NULL DEFAULT '{}',
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    UNIQUE (src_item_id, dst_item_id, type)
);
CREATE INDEX idx_edges_src ON edges (src_item_id, type);
CREATE INDEX idx_edges_dst ON edges (dst_item_id, type);

-- Tags: namespaced facets with per-application properties.
CREATE TABLE tag_defs (
    id         INTEGER PRIMARY KEY,
    facet      TEXT NOT NULL UNIQUE,
    value_kind TEXT NOT NULL DEFAULT 'string'
);
CREATE TABLE tag_applications (
    item_id INTEGER NOT NULL REFERENCES items (id) ON DELETE CASCADE,
    facet   TEXT NOT NULL,
    value   TEXT NOT NULL DEFAULT '',
    props   TEXT NOT NULL DEFAULT '{}',
    PRIMARY KEY (item_id, facet, value)
);
CREATE INDEX idx_tag_applications_facet ON tag_applications (facet, value, item_id);
CREATE INDEX idx_tag_applications_item ON tag_applications (item_id);

-- Blobs: content-addressed raw storage (blake3 hex key).
CREATE TABLE blobs (
    hash       TEXT PRIMARY KEY,
    bytes      BLOB NOT NULL,
    mime       TEXT,
    size       INTEGER NOT NULL,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
);

-- Ingestions: idempotency key + per-stage resume state.
CREATE TABLE ingestions (
    id               INTEGER PRIMARY KEY,
    source_hash      TEXT NOT NULL,
    pipeline_version INTEGER NOT NULL,
    strategy         TEXT NOT NULL,
    embedder_model   TEXT NOT NULL,
    stage            TEXT NOT NULL,
    status           TEXT NOT NULL,
    blob_hash        TEXT,
    started_at       TEXT,
    completed_at     TEXT,
    UNIQUE (source_hash, pipeline_version, strategy, embedder_model)
);

-- Embeddings catalog: which vec tables exist (populated in Section 6).
CREATE TABLE embeddings_meta (
    model        TEXT NOT NULL,
    dim          INTEGER NOT NULL,
    table_name   TEXT NOT NULL,
    populated_at TEXT,
    PRIMARY KEY (model, dim)
);

-- Changelog: append-only audit; the source of truth for `undo` (design D11).
CREATE TABLE changelog (
    id          INTEGER PRIMARY KEY,
    txn_id      INTEGER NOT NULL,
    ts          TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    op          TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id   TEXT NOT NULL,
    before      TEXT,
    after       TEXT,
    actor       TEXT
);
CREATE INDEX idx_changelog_txn ON changelog (txn_id);

-- Surface the changelog under the _sys/transactions namespace as a view.
CREATE VIEW sys_transactions AS
SELECT id, txn_id, ts, op, entity_type, entity_id, before, after, actor
FROM changelog;

-- Seed the system namespaces.
INSERT INTO namespaces (path, parent_id, kind) VALUES ('_sys', NULL, 'system');
INSERT INTO namespaces (path, parent_id, kind)
SELECT '_sys/transactions', id, 'system' FROM namespaces WHERE path = '_sys';
INSERT INTO namespaces (path, parent_id, kind)
SELECT '_sys/ingestions', id, 'system' FROM namespaces WHERE path = '_sys';
