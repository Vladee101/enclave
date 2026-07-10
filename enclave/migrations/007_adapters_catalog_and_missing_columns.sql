-- =============================================================
-- Forward-fix migration. Do NOT edit 001-006 (see 005's header).
--
-- Found by running rls_validation against a *truly fresh* database
-- (previously it only ever ran against the live, already-hybrid dev
-- database, so this gap was invisible): migrations 001-006 alone do not
-- reproduce the live schema. The live `enclave` database has an `adapters`
-- catalog table, `chunks.token_count`, and NOT NULL `documents.mime_type`/
-- `byte_size` — none of which any migration file creates. They exist only
-- because the original `db/schema.sql` reference design was hand-applied
-- via psql before migrations ever ran, and `CREATE TABLE IF NOT EXISTS`
-- then silently kept schema.sql's shape wherever the two designs collided.
-- migrations/005 already reconciled the *column-name* mismatches this
-- caused (pin_hash, filename, error_text, ...); this migration reconciles
-- the remaining gaps: whole objects/columns schema.sql provided that no
-- migration file ever created, which the app code (commands/admin.rs,
-- llm/mod.rs, llm/adapters.rs, ingest/mod.rs) already assumes exist.
--
-- Deliberately NOT recreating the rest of schema.sql's surface (memberships,
-- roles, conversations, messages, query_logs, query_retrieved_chunks,
-- audit_events) — those are dead weight no app code path uses, and ADR-0011
-- already covers removing that class of leftover, not expanding it further.
-- =============================================================

-- ---------------------------------------------------------------
-- 1. adapters catalog table (schema.sql's design — a shared, non
-- department-scoped registry of adapter files; department_adapters
-- references it via adapter_id below). No RLS: not department-scoped.
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS adapters (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL UNIQUE,
    file_path  TEXT NOT NULL,
    base_model TEXT NOT NULL,
    rank       INTEGER NOT NULL CHECK (rank > 0),
    alpha      INTEGER NOT NULL CHECK (alpha > 0),
    file_hash  TEXT NOT NULL UNIQUE,
    is_active  BOOLEAN NOT NULL DEFAULT true,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

GRANT SELECT ON adapters TO app_user;
GRANT SELECT ON adapters TO ingest_worker;

-- ---------------------------------------------------------------
-- 2. department_adapters: migrate from 002's flat shape (adapter_path/
-- description/is_active inline) to a pure junction against `adapters`.
-- Guarded on adapter_id's absence — on THIS database that's already true
-- (it has the schema.sql shape), so this only actually runs on a fresh
-- database where 002 just created the flat-shaped, still-empty table.
-- ---------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'department_adapters' AND column_name = 'adapter_id'
    ) THEN
        ALTER TABLE department_adapters ADD COLUMN adapter_id UUID REFERENCES adapters(id) ON DELETE CASCADE;
        ALTER TABLE department_adapters ADD COLUMN is_default BOOLEAN NOT NULL DEFAULT false;
        ALTER TABLE department_adapters DROP COLUMN IF EXISTS adapter_path;
        ALTER TABLE department_adapters DROP COLUMN IF EXISTS description;
        ALTER TABLE department_adapters DROP COLUMN IF EXISTS is_active;
        ALTER TABLE department_adapters DROP COLUMN IF EXISTS created_at;
        ALTER TABLE department_adapters ALTER COLUMN adapter_id SET NOT NULL;
        ALTER TABLE department_adapters
            ADD CONSTRAINT department_adapters_department_id_adapter_id_key UNIQUE (department_id, adapter_id);
        ALTER TABLE department_adapters
            ADD CONSTRAINT department_adapters_scale_check CHECK (scale >= 0 AND scale <= 2);
    END IF;
END$$;

CREATE UNIQUE INDEX IF NOT EXISTS uq_department_default_adapter
    ON department_adapters (department_id) WHERE is_default;

-- ---------------------------------------------------------------
-- 3. chunks.token_count (chars/4 heuristic — see ingest/mod.rs)
-- ---------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'chunks' AND column_name = 'token_count'
    ) THEN
        ALTER TABLE chunks ADD COLUMN token_count INTEGER;
        UPDATE chunks SET token_count = GREATEST(length(content) / 4, 1) WHERE token_count IS NULL;
        ALTER TABLE chunks ALTER COLUMN token_count SET NOT NULL;
    END IF;
END$$;

-- ---------------------------------------------------------------
-- 4. documents.mime_type / byte_size NOT NULL (documents.rs always
-- supplies both now, but the constraint should match what the live
-- database — and the code's assumptions — actually enforce).
-- ---------------------------------------------------------------
DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'documents' AND column_name = 'mime_type' AND is_nullable = 'YES'
    ) THEN
        UPDATE documents SET mime_type = 'application/octet-stream' WHERE mime_type IS NULL;
        ALTER TABLE documents ALTER COLUMN mime_type SET NOT NULL;
    END IF;
END$$;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'documents' AND column_name = 'byte_size' AND is_nullable = 'YES'
    ) THEN
        UPDATE documents SET byte_size = 0 WHERE byte_size IS NULL;
        ALTER TABLE documents ALTER COLUMN byte_size SET NOT NULL;
    END IF;
END$$;
