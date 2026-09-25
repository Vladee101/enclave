-- =============================================================
-- Forward-fix migration. Do NOT edit 001-015 (see 005's header).
--
-- embedding_models.provider: the embedding sidecar registers itself with
-- a provider ('llama.cpp'), but no migration ever created the column. It
-- existed only in development databases that once had db/schema.sql
-- hand-applied (the drift 005/006 fixed for other columns), so every
-- database built from migrations alone — every new install, first seen
-- with the embedded PostgreSQL of ADR-0014 — failed the registration, and
-- ingestion had no active embedding model.
-- =============================================================

ALTER TABLE embedding_models ADD COLUMN IF NOT EXISTS provider TEXT;
