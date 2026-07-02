-- =============================================================
-- ADR-0008: Department-scoped Row-Level Security
--
-- Design invariants:
--   • The application NEVER connects as superuser or BYPASSRLS.
--   • The app role is app_user (NOLOGIN; the actual connection
--     uses a login role that inherits from app_user).
--   • Per-transaction session variable: app.current_user_id
--     set via set_config('app.current_user_id', $1, true)
--     (transaction-local = true, safe under connection pooling).
--   • Policies fail CLOSED: if the session variable is unset,
--     no rows are returned.
--   • SECURITY = FORCE so even the table owner is subject.
-- =============================================================

-- ---------------------------------------------------------------
-- Application role
-- ---------------------------------------------------------------
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_user') THEN
        CREATE ROLE app_user NOLOGIN NOINHERIT;
    END IF;
END$$;

-- The actual login role that the pool uses; inherits app_user.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'enclave_app') THEN
        CREATE ROLE enclave_app LOGIN PASSWORD 'change_me_in_production' INHERIT;
        GRANT app_user TO enclave_app;
    END IF;
END$$;

-- ---------------------------------------------------------------
-- Helper function: the current session's user id, or NULL if unset,
-- empty, or not a valid UUID. Never throws — every policy that needs
-- the caller's identity should go through this rather than casting
-- current_setting() directly, so a missing/blank session variable
-- fails closed (no matching rows) instead of raising a hard error.
-- LANGUAGE plpgsql (not sql): a SQL-language CASE here would let the
-- planner hoist an uncorrelated EXISTS(...) into an InitPlan evaluated
-- before branch selection (see is_member_of below), bypassing the
-- short-circuit and reaching the ::UUID cast anyway. plpgsql's real
-- procedural IF/RETURN control flow has no such hazard.
-- ---------------------------------------------------------------
CREATE OR REPLACE FUNCTION current_user_id()
RETURNS UUID
LANGUAGE plpgsql STABLE
AS $$
DECLARE
    uid TEXT := current_setting('app.current_user_id', true);
BEGIN
    IF uid IS NULL OR uid = '' THEN
        RETURN NULL;
    END IF;
    RETURN uid::UUID;
EXCEPTION WHEN invalid_text_representation THEN
    RETURN NULL;
END;
$$;

GRANT EXECUTE ON FUNCTION current_user_id() TO app_user;

-- ---------------------------------------------------------------
-- Helper function: is the session user a member of a department?
-- Returns TRUE only when app.current_user_id is set and the user
-- belongs to the given department.
-- ---------------------------------------------------------------
CREATE OR REPLACE FUNCTION is_member_of(dept_id UUID)
RETURNS BOOLEAN
LANGUAGE sql STABLE SECURITY DEFINER
AS $$
    SELECT current_user_id() IS NOT NULL
       AND EXISTS (
            SELECT 1
            FROM department_members dm
            WHERE dm.user_id      = current_user_id()
              AND dm.department_id = dept_id
        );
$$;

-- Grant execute to app_user so the policy can call it.
GRANT EXECUTE ON FUNCTION is_member_of(UUID) TO app_user;

-- ---------------------------------------------------------------
-- Grant table access to app_user
-- ---------------------------------------------------------------
GRANT SELECT, INSERT, UPDATE, DELETE ON
    users, departments, department_members,
    documents, chunks, chunk_embeddings,
    embedding_models, department_adapters,
    ingestion_jobs, audit_log
TO app_user;

-- ---------------------------------------------------------------
-- Enable & force RLS on all department-scoped tables
-- ---------------------------------------------------------------
ALTER TABLE documents         ENABLE ROW LEVEL SECURITY;
ALTER TABLE documents         FORCE  ROW LEVEL SECURITY;

ALTER TABLE chunks            ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunks            FORCE  ROW LEVEL SECURITY;

ALTER TABLE chunk_embeddings  ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunk_embeddings  FORCE  ROW LEVEL SECURITY;

ALTER TABLE department_adapters ENABLE ROW LEVEL SECURITY;
ALTER TABLE department_adapters FORCE  ROW LEVEL SECURITY;

ALTER TABLE ingestion_jobs    ENABLE ROW LEVEL SECURITY;
ALTER TABLE ingestion_jobs    FORCE  ROW LEVEL SECURITY;

ALTER TABLE audit_log         ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_log         FORCE  ROW LEVEL SECURITY;

-- ---------------------------------------------------------------
-- Policies  (SELECT, INSERT, UPDATE, DELETE)
-- ---------------------------------------------------------------

-- documents
CREATE POLICY documents_dept_select ON documents
    FOR SELECT USING (is_member_of(department_id));

CREATE POLICY documents_dept_insert ON documents
    FOR INSERT WITH CHECK (is_member_of(department_id));

CREATE POLICY documents_dept_update ON documents
    FOR UPDATE USING (is_member_of(department_id));

CREATE POLICY documents_dept_delete ON documents
    FOR DELETE USING (is_member_of(department_id));

-- chunks
CREATE POLICY chunks_dept_select ON chunks
    FOR SELECT USING (is_member_of(department_id));

CREATE POLICY chunks_dept_insert ON chunks
    FOR INSERT WITH CHECK (is_member_of(department_id));

CREATE POLICY chunks_dept_update ON chunks
    FOR UPDATE USING (is_member_of(department_id));

CREATE POLICY chunks_dept_delete ON chunks
    FOR DELETE USING (is_member_of(department_id));

-- chunk_embeddings
CREATE POLICY embeddings_dept_select ON chunk_embeddings
    FOR SELECT USING (is_member_of(department_id));

CREATE POLICY embeddings_dept_insert ON chunk_embeddings
    FOR INSERT WITH CHECK (is_member_of(department_id));

CREATE POLICY embeddings_dept_update ON chunk_embeddings
    FOR UPDATE USING (is_member_of(department_id));

CREATE POLICY embeddings_dept_delete ON chunk_embeddings
    FOR DELETE USING (is_member_of(department_id));

-- department_adapters
CREATE POLICY adapters_dept_select ON department_adapters
    FOR SELECT USING (is_member_of(department_id));

CREATE POLICY adapters_dept_insert ON department_adapters
    FOR INSERT WITH CHECK (is_member_of(department_id));

CREATE POLICY adapters_dept_update ON department_adapters
    FOR UPDATE USING (is_member_of(department_id));

CREATE POLICY adapters_dept_delete ON department_adapters
    FOR DELETE USING (is_member_of(department_id));

-- ingestion_jobs: scoped via the parent document
CREATE POLICY ingestion_jobs_dept_select ON ingestion_jobs
    FOR SELECT USING (
        EXISTS (
            SELECT 1 FROM documents d
            WHERE d.id = document_id
              AND is_member_of(d.department_id)
        )
    );

CREATE POLICY ingestion_jobs_dept_insert ON ingestion_jobs
    FOR INSERT WITH CHECK (
        EXISTS (
            SELECT 1 FROM documents d
            WHERE d.id = document_id
              AND is_member_of(d.department_id)
        )
    );

CREATE POLICY ingestion_jobs_dept_update ON ingestion_jobs
    FOR UPDATE USING (
        EXISTS (
            SELECT 1 FROM documents d
            WHERE d.id = document_id
              AND is_member_of(d.department_id)
        )
    );

-- audit_log: users can only see their own rows
CREATE POLICY audit_log_own_select ON audit_log
    FOR SELECT USING (
        user_id = current_user_id()
    );

CREATE POLICY audit_log_own_insert ON audit_log
    FOR INSERT WITH CHECK (
        user_id = current_user_id()
    );
