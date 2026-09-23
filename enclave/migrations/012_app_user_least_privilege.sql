-- =============================================================
-- Forward-fix migration. Do NOT edit 001-011 (see 005's header).
--
-- Least privilege for app_user (ADR-0018).
--
-- The query role could write almost every table: 004 granted it
-- SELECT/INSERT/UPDATE/DELETE across the schema, databases that once had
-- db/schema.sql hand-applied carry even broader grants (down to
-- _sqlx_migrations), and default privileges there hand it write access
-- to every future table. Most of that was harmless only because RLS
-- policies happened to be missing — but `users` has no RLS at all, and
-- `UPDATE users SET is_admin = true` as app_user was verified to work:
-- privilege escalation straight past every admin check that
-- delete_document() / delete_department() rely on. Members could also
-- rewrite their department's LoRA assignments (department_adapters
-- policies from 004) and register embedding models.
--
-- The rule from here on: app_user may write exactly what the app writes
-- on app_pool, and nothing else. Everything privileged goes through
-- admin_pool or a SECURITY DEFINER function with its own check.
--   documents       INSERT (upload), UPDATE of status/updated_at (re-queue)
--   ingestion_jobs  INSERT (upload)
--   audit_log       INSERT (own rows; audit_log_own_insert)
-- =============================================================

-- ---------------------------------------------------------------
-- 1. Strip every write, on every table that exists now — including the
--    dormant schema.sql tables in databases that have them.
-- ---------------------------------------------------------------
REVOKE INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER
    ON ALL TABLES IN SCHEMA public FROM app_user;
REVOKE ALL ON _sqlx_migrations FROM app_user;

-- ---------------------------------------------------------------
-- 2. Grant back exactly what app_pool code writes. (Revoking the
--    table-level UPDATE above also dropped 009's column grant.)
-- ---------------------------------------------------------------
GRANT INSERT ON documents, ingestion_jobs, audit_log TO app_user;
GRANT UPDATE (status, updated_at) ON documents TO app_user;

-- Policies for writes app_user can no longer make: dead weight that would
-- quietly come back to life if a privilege were ever re-granted.
DROP POLICY IF EXISTS adapters_dept_insert       ON department_adapters;
DROP POLICY IF EXISTS adapters_dept_update       ON department_adapters;
DROP POLICY IF EXISTS adapters_dept_delete       ON department_adapters;
DROP POLICY IF EXISTS ingestion_jobs_dept_update ON ingestion_jobs;

-- ---------------------------------------------------------------
-- 3. Future objects: no automatic grants to app_user. Some databases
--    carry default privileges giving it write on every new table and
--    EXECUTE on every new function — which would reopen all of the
--    above with the next migration, and make every future SECURITY
--    DEFINER function callable before anyone decided it should be.
--    SELECT on new tables stays automatic (reads are still RLS-bound);
--    writes and EXECUTE must be granted explicitly, next to the object.
--    Looked up per owning role and per schema: the admin role is not
--    always called postgres, and default privileges may be global or
--    scoped IN SCHEMA (the drifted databases have them on public — a
--    global REVOKE leaves those untouched).
-- ---------------------------------------------------------------
DO $$
DECLARE
    acl RECORD;
    scope TEXT;
BEGIN
    FOR acl IN
        SELECT DISTINCT pg_get_userbyid(d.defaclrole) AS owner_role, d.defaclnamespace AS ns
        FROM pg_default_acl d, aclexplode(d.defaclacl) a
        WHERE a.grantee = 'app_user'::regrole
    LOOP
        scope := CASE WHEN acl.ns = 0 THEN '' ELSE format(' IN SCHEMA %I', acl.ns::regnamespace::text) END;
        EXECUTE format(
            'ALTER DEFAULT PRIVILEGES FOR ROLE %I%s REVOKE INSERT, UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON TABLES FROM app_user',
            acl.owner_role, scope);
        EXECUTE format(
            'ALTER DEFAULT PRIVILEGES FOR ROLE %I%s REVOKE EXECUTE ON FUNCTIONS FROM app_user',
            acl.owner_role, scope);
    END LOOP;
END$$;
