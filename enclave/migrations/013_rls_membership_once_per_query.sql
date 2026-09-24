-- =============================================================
-- Forward-fix migration. Do NOT edit 001-012 (see 005's header).
--
-- RLS policies that evaluate membership once per query (ADR-0021),
-- and removal of the dormant schema.sql policies (ADR-0011's deferred
-- cleanup, now covered by rls_validation in CI).
--
-- Measured on a 50 000-row spreadsheet: a full-text lookup of one word
-- over the department's chunks took 257 ms under the old policies,
-- 20 ms without any, 15 ms with a precomputed membership array. The
-- time was the policies, not the search: `is_member_of(department_id)`
-- ran a subquery per row, and in databases that once had db/schema.sql
-- applied, a dormant `app_user_in_department(department_id)` policy ran
-- alongside it on every row too. (The GIN index cannot help under RLS
-- at all: `@@` is not leakproof, so the planner may not evaluate it
-- before the policy.)
--
-- The rule does not change — a row is visible to members of its
-- department, decided in the database — only its form: the caller's
-- departments are computed once, as an InitPlan (the `(SELECT …)`
-- wrapper), and each row is checked against that array.
-- =============================================================

-- ---------------------------------------------------------------
-- 1. The caller's departments, once per query. Unset identity → empty
--    array → no rows (fails closed, like is_member_of).
-- ---------------------------------------------------------------
CREATE OR REPLACE FUNCTION current_user_department_ids()
RETURNS UUID[]
LANGUAGE sql
STABLE
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT COALESCE(array_agg(dm.department_id), '{}')
    FROM department_members dm
    WHERE dm.user_id = current_user_id();
$$;

REVOKE ALL ON FUNCTION current_user_department_ids() FROM PUBLIC;
-- Explicit: 012 removed default EXECUTE for app_user.
GRANT EXECUTE ON FUNCTION current_user_department_ids() TO app_user;

-- ---------------------------------------------------------------
-- 2. Dormant policies (ADR-0011): every policy built on the schema.sql
--    helpers, whatever its name — names differ between databases. With
--    FORCE RLS a table left without policies denies everything, which is
--    right for the unused schema.sql tables (conversations, messages,
--    query_logs, memberships, …). Absent in databases built from
--    migrations only; then this loop does nothing.
-- ---------------------------------------------------------------
DO $$
DECLARE
    p RECORD;
BEGIN
    FOR p IN
        SELECT schemaname, tablename, policyname
        FROM pg_policies
        WHERE schemaname = 'public'
          AND (COALESCE(qual, '') || ' ' || COALESCE(with_check, '')) ~ '(app_user_in_department|app_current_user_id)\('
    LOOP
        EXECUTE format('DROP POLICY %I ON %I.%I', p.policyname, p.schemaname, p.tablename);
    END LOOP;

    -- The helpers go too, unless something else still depends on them.
    BEGIN
        DROP FUNCTION IF EXISTS app_user_in_department(UUID);
        DROP FUNCTION IF EXISTS app_current_user_id();
    EXCEPTION WHEN dependent_objects_still_exist THEN
        RAISE NOTICE 'schema.sql helper functions still have dependents; left in place';
    END;
END$$;

-- ---------------------------------------------------------------
-- 3. The live policies, same rules, membership computed once.
-- ---------------------------------------------------------------
DROP POLICY IF EXISTS documents_dept_select ON documents;
CREATE POLICY documents_dept_select ON documents
    FOR SELECT USING (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS documents_dept_insert ON documents;
CREATE POLICY documents_dept_insert ON documents
    FOR INSERT WITH CHECK (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS documents_dept_update ON documents;
CREATE POLICY documents_dept_update ON documents
    FOR UPDATE USING (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS chunks_dept_select ON chunks;
CREATE POLICY chunks_dept_select ON chunks
    FOR SELECT USING (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS embeddings_dept_select ON chunk_embeddings;
CREATE POLICY embeddings_dept_select ON chunk_embeddings
    FOR SELECT USING (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS adapters_dept_select ON department_adapters;
CREATE POLICY adapters_dept_select ON department_adapters
    FOR SELECT USING (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS departments_member_select ON departments;
CREATE POLICY departments_member_select ON departments
    FOR SELECT USING (id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS department_members_own_select ON department_members;
CREATE POLICY department_members_own_select ON department_members
    FOR SELECT USING (user_id = (SELECT current_user_id()));

DROP POLICY IF EXISTS ingestion_jobs_dept_select ON ingestion_jobs;
CREATE POLICY ingestion_jobs_dept_select ON ingestion_jobs
    FOR SELECT USING (EXISTS (
        SELECT 1 FROM documents d
        WHERE d.id = document_id
          AND d.department_id = ANY ((SELECT current_user_department_ids())::uuid[])
    ));

DROP POLICY IF EXISTS ingestion_jobs_dept_insert ON ingestion_jobs;
CREATE POLICY ingestion_jobs_dept_insert ON ingestion_jobs
    FOR INSERT WITH CHECK (EXISTS (
        SELECT 1 FROM documents d
        WHERE d.id = document_id
          AND d.department_id = ANY ((SELECT current_user_department_ids())::uuid[])
    ));

DROP POLICY IF EXISTS audit_log_own_select ON audit_log;
CREATE POLICY audit_log_own_select ON audit_log
    FOR SELECT USING (user_id = (SELECT current_user_id()));

DROP POLICY IF EXISTS audit_log_own_insert ON audit_log;
CREATE POLICY audit_log_own_insert ON audit_log
    FOR INSERT WITH CHECK (user_id = (SELECT current_user_id()));
