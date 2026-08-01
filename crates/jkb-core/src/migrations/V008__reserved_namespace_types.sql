-- Typed namespaces, the retrofit (design D33.4): stamp the reserved roots with the
-- contract each carries, so `nstype::check_placement` has something to enforce on an
-- existing database. `ns::ensure` does the same for namespaces created from here on;
-- this back-fills the ones that already exist.
--
-- Guarded by `json_extract(...) IS NULL` so a hand-set type is never clobbered, and by
-- `json_valid` because `json_extract` raises on metadata that is not valid JSON (only
-- reachable if something outside jkb wrote it).
--
-- Mirrors `nstype::RESERVED_TYPES`; keep the two in step.

UPDATE namespaces
SET metadata = json_set(
        CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.type', 'tasks')
WHERE path = 'tasks'
  AND json_extract(CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.type')
      IS NULL;

UPDATE namespaces
SET metadata = json_set(
        CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.type', 'views')
WHERE path = '_sys/views'
  AND json_extract(CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.type')
      IS NULL;

UPDATE namespaces
SET metadata = json_set(
        CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.type', 'journal')
WHERE path IN ('_sys/sync', '_sys/transactions', '_sys/ingestions')
  AND json_extract(CASE WHEN json_valid(metadata) THEN metadata ELSE '{}' END, '$.type')
      IS NULL;
