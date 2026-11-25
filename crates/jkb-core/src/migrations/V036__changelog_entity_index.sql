-- `jkb undo <txn>` refuses an entry while a later transaction has written the same row
-- (`undo::later_writer`), which asks the changelog for every later entry on one
-- `(entity_type, entity_id)`. The only index was `(txn_id)`, so for an old transaction each of its
-- entries scanned most of the log, and an ingest transaction has hundreds of entries.
--
-- Additive: an index, no data change.
CREATE INDEX idx_changelog_entity ON changelog (entity_type, entity_id, txn_id);
