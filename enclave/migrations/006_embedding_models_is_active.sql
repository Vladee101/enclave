-- =============================================================
-- Forward-fix migration. Do NOT edit 001-005 (see 005's header).
--
-- ingest/mod.rs picks "the" embedding model with:
--   SELECT id, dimension FROM embedding_models WHERE is_active = true LIMIT 1
-- (ADR-0007's model registry). The live embedding_models table instead
-- matches the old hand-applied db/schema.sql shape — (id, name, dimension,
-- provider), no is_active — the same class of drift migration 005 fixed
-- for users/departments/documents/ingestion_jobs, just not caught for this
-- table at the time because nothing had tried to query it yet.
-- =============================================================

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'embedding_models' AND column_name = 'is_active'
    ) THEN
        ALTER TABLE embedding_models ADD COLUMN is_active BOOLEAN NOT NULL DEFAULT false;
    END IF;
END$$;
