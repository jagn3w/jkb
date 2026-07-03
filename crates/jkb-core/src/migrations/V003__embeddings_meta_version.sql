-- Record the *resolved* model fingerprint alongside the configured model name.
--
-- `embeddings_meta.model` stores the configured name (e.g. `nomic-embed-text`),
-- which for ollama is really the mutable `:latest` tag. `model_version` records the
-- resolved identity — ollama's content digest, or a stable id for fastembed — so
-- silent drift (a `:latest` tag re-pointed at new weights) is detectable by `doctor`
-- via `jkb_embed::check_version_drift`. Nullable: some backends expose no version
-- handle. `embeddings_meta` is a regular table, so ADD COLUMN is safe (unlike the
-- vec0/FTS5 virtual tables, which are never ALTERed — design D13).
ALTER TABLE embeddings_meta ADD COLUMN model_version TEXT;
