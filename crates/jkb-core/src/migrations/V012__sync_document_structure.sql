-- A synced file's document structure lives on its journal row, not in the namespace tree (D45.2).
--
-- The `tasks` serializer stored a file's structure -- its `##` headers, their order, and its
-- prose -- inside `namespaces.metadata`, on namespaces derived from the file's path. The
-- namespace tree is a shared, globally addressable, user-mutable hierarchy; a file's structure is
-- private to one file, file-owned, and has to round-trip exactly. Every sync data-loss incident
-- in this project came out of that mismatch:
--
--   * the `openspec` collapse -- two files shared a namespace, so they shared one `layout`
--   * prose orphaning -- prose stored as items placed in namespaces
--   * layout ownership -- seven guards over eight review passes, all asking "whose layout is this?"
--   * `retire_undeclared_sections` retiring a *neighbouring* file's sections
--   * `jkb ns mv` (and the VS Code Rename button) making a file's structure unreachable, after
--     which the export arm wrote a structureless render over the file
--
-- `sync_state` is keyed `uri TEXT PRIMARY KEY`, so it holds at most one row per file and two files
-- cannot share one whatever the namespace derivation does. `reconcile` already loads that row as
-- its first act, so reading structure from it costs nothing extra -- and the byte fast path in
-- `decide_direction` and `Outcome::Normalized` both survive, which a base-blob-derived design
-- would have had to give up.
--
-- `document` holds the whole structure as one JSON object:
--
--     {"layout": [ {"section": "backend"} | {"item": "<local_id>"} | {"prose": "..."} ],
--      "sections": [ {"path": "backend", "header_line": "## Backend"} ]}
--
-- NULL means "not yet populated": the engine fills it once, from the file's own base blob (or the
-- file on disk), immediately after reading the journal row. Section namespaces survive as a
-- derived, rebuildable view for browsing and `ns:` scoping -- nothing reads them to decide what a
-- document looks like.
--
-- Being a migration is itself load-bearing. Refinery's runner defaults to `abort_missing: true`
-- and verifies every applied migration before running any, so a binary older than this one fails
-- at `Db::open` rather than silently reading namespace metadata that nothing refreshes any more
-- and exporting from it.

ALTER TABLE sync_state ADD COLUMN document TEXT;

-- `_sys/sync` is the only read surface for the journal, so the view has to carry the column that
-- now decides what a file looks like -- otherwise the most important field in the table is the
-- one nobody can see. SQLite cannot ALTER a view; it is dropped and recreated.
DROP VIEW IF EXISTS sys_sync;

CREATE VIEW sys_sync AS
SELECT uri, serializer, status, last_synced_hash, base_blob_hash,
       parse_error, quarantine_blob_hash, document, updated_at
FROM sync_state;
