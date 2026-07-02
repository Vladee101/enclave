-- =============================================================
-- Forward-fix migration. Do NOT edit 001-004: sqlx tracks applied
-- migrations by checksum, so already-migrated databases must only
-- ever receive new files.
--
-- REVISED: this file originally assumed a "fresh" database built
-- only from 001-004. In practice the dev database had already been
-- hand-patched (email/password_hash/slug/title/file_hash/error all
-- already existed, pin_hash was already renamed) before this
-- migration was written, so the original version failed on a
-- database that had drifted ahead of what it expected. Every
-- column/rename step below is now guarded so this migration is safe
-- to run whether the target column already exists or not.
--
-- This migration closes the gaps between the schema 001-004 create
-- and what the Rust code / CLAUDE.md invariants actually require:
--
--   1. Column drift: guarded below, see comments per statement.
--   2. No dedicated ingestion role: CLAUDE.md invariant #3 requires a
--      BYPASSRLS `ingest_worker` role used only by the ingestion
--      worker, distinct from `app_user`/`enclave_app` and from any
--      superuser used for provisioning. That role never existed.
--   3. No RLS on `departments` / `department_members` at all, so any
--      app_user query saw every department in the database.
--   4. NEW — found during verification: `departments`, `documents`,
--      `chunks`, `chunk_embeddings`, `ingestion_jobs`, `conversations`
--      and `query_logs` each carry a leftover permissive policy built
--      on `app_user_in_department()` / `app_current_user_id()`, which
--      read from `memberships` / a table from the original schema.sql
--      reference design that was never supposed to go live and is not
--      written to by any application code path. `memberships` is
--      confirmed empty, so this is not an active bypass, but it's a
--      live landmine (a stray future row would silently widen access
--      on all seven tables, since Postgres OR's permissive policies
--      together). Deliberately NOT touched by this migration — the
--      function has more dependents than fit in a column-alignment
--      migration's blast radius. See ADR-0011 for the deferred,
--      table-by-table cleanup plan.
-- =============================================================

-- ---------------------------------------------------------------
-- 1. Column alignment (idempotent — guards added against drift)
-- ---------------------------------------------------------------

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'users' AND column_name = 'pin_hash'
    ) THEN
        ALTER TABLE users RENAME COLUMN pin_hash TO password_hash;
    END IF;
END$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'users' AND column_name = 'email'
    ) THEN
        ALTER TABLE users ADD COLUMN email TEXT;
        UPDATE users SET email = username || '@local' WHERE email IS NULL;
        ALTER TABLE users ALTER COLUMN email SET NOT NULL;
        ALTER TABLE users ADD CONSTRAINT users_email_key UNIQUE (email);
    END IF;
END$$;

-- Bootstrap admin flag: no roles table exists in this schema family,
-- so admin-only commands (cmd_create_department, cmd_add_adapter)
-- gate on this instead. The first user ever created is promoted to
-- admin by application code at creation time.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'users' AND column_name = 'is_admin'
    ) THEN
        ALTER TABLE users ADD COLUMN is_admin BOOLEAN NOT NULL DEFAULT false;
    END IF;
END$$;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'departments' AND column_name = 'slug'
    ) THEN
        ALTER TABLE departments ADD COLUMN slug TEXT;
        UPDATE departments
        SET slug = trim(both '-' from regexp_replace(lower(name), '[^a-z0-9]+', '-', 'g'))
                   || '-' || substr(id::text, 1, 8)
        WHERE slug IS NULL;
        ALTER TABLE departments ALTER COLUMN slug SET NOT NULL;
        ALTER TABLE departments ADD CONSTRAINT departments_slug_key UNIQUE (slug);
    END IF;
END$$;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'documents' AND column_name = 'filename'
    ) THEN
        ALTER TABLE documents RENAME COLUMN filename TO title;
    END IF;
END$$;

-- Pre-existing rows predate the content-addressed blob store and have
-- no source bytes to hash; synthesize a unique placeholder so the
-- NOT NULL + UNIQUE constraints below can be applied. Real uploads
-- from here on always supply a genuine sha-256 (documents.rs).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'documents' AND column_name = 'file_hash'
    ) THEN
        ALTER TABLE documents ADD COLUMN file_hash TEXT;
        UPDATE documents SET file_hash = 'legacy:' || md5(id::text) WHERE file_hash IS NULL;
        ALTER TABLE documents ALTER COLUMN file_hash SET NOT NULL;
        ALTER TABLE documents ADD CONSTRAINT documents_department_file_hash_key
            UNIQUE (department_id, file_hash);
    END IF;
END$$;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'ingestion_jobs' AND column_name = 'error_text'
    ) THEN
        ALTER TABLE ingestion_jobs RENAME COLUMN error_text TO error;
    END IF;
END$$;

DO $$
BEGIN
    IF EXISTS (
        SELECT 1 FROM information_schema.columns
        WHERE table_name = 'ingestion_jobs' AND column_name = 'queued_at'
    ) THEN
        ALTER TABLE ingestion_jobs RENAME COLUMN queued_at TO created_at;
    END IF;
END$$;

-- ---------------------------------------------------------------
-- 2. Dedicated ingestion role (ADR-0008/0009, CLAUDE.md invariant #3)
--
-- BYPASSRLS so the worker can stamp department_id onto chunks /
-- chunk_embeddings without a session identity (ADR-0009). This is
-- NOT the superuser used for migrations/provisioning: it can bypass
-- RLS and nothing else — no CREATEDB, no CREATEROLE, no DDL rights
-- beyond what's explicitly granted below.
-- ---------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'ingest_worker') THEN
        CREATE ROLE ingest_worker LOGIN PASSWORD 'change_me_in_production'
            NOSUPERUSER BYPASSRLS NOCREATEDB NOCREATEROLE NOINHERIT;
    END IF;
END$$;

GRANT USAGE ON SCHEMA public TO ingest_worker;
GRANT SELECT, INSERT, UPDATE ON documents, chunks, chunk_embeddings, ingestion_jobs
    TO ingest_worker;
GRANT SELECT ON embedding_models, departments TO ingest_worker;

-- ---------------------------------------------------------------
-- 3. RLS on departments / department_members
--
-- 004_rls.sql enabled RLS on the department-scoped knowledge tables
-- but never on departments/department_members themselves, so any
-- app_user query (e.g. a department picker) saw every department in
-- the org regardless of membership.
-- ---------------------------------------------------------------
ALTER TABLE departments        ENABLE ROW LEVEL SECURITY;
ALTER TABLE departments        FORCE  ROW LEVEL SECURITY;
ALTER TABLE department_members ENABLE ROW LEVEL SECURITY;
ALTER TABLE department_members FORCE  ROW LEVEL SECURITY;

-- Members can see the departments they belong to.
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_policies
        WHERE tablename = 'departments' AND policyname = 'departments_member_select'
    ) THEN
        CREATE POLICY departments_member_select ON departments
            FOR SELECT USING (is_member_of(id));
    END IF;
END$$;

-- Members can see their own membership rows only. is_member_of() is
-- SECURITY DEFINER so it keeps working even though this table is now
-- RLS-protected (it reads department_members directly as the
-- function owner, not as app_user).
DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1 FROM pg_policies
        WHERE tablename = 'department_members' AND policyname = 'department_members_own_select'
    ) THEN
        CREATE POLICY department_members_own_select ON department_members
            FOR SELECT USING (user_id = current_user_id());
    END IF;
END$$;

-- NOTE: this migration does NOT touch the dormant memberships-based
-- policies/function (app_user_in_department(), dept_visible,
-- doc_dept_isolation, chunk_dept_isolation, emb_dept_isolation,
-- ingestion_dept_isolation, conv_owner, qlog_owner — 11 policies
-- across 6 tables). They're dead (memberships is empty, nothing
-- writes to it) but still depended on by other policies, so removing
-- them safely means dropping 6 tables' worth of policies one at a
-- time and confirming nothing else breaks — too large to bundle into
-- a column-alignment migration. See the ADR for the deferred plan.