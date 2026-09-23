-- =============================================================
-- Forward-fix migration. Do NOT edit 001-010 (see 005's header).
--
-- Department deletion together with its documents (ADR-0017).
-- Administrator only, never the default department, checked in the
-- database like document deletion (ADR-0015).
--
-- The department row itself becomes a tombstone, not a hard delete:
-- documents, chunks and chunk_embeddings reference it ON DELETE
-- RESTRICT, and audit_log rows and document tombstones must keep
-- pointing at something.
-- =============================================================

ALTER TABLE departments ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
ALTER TABLE departments ADD COLUMN IF NOT EXISTS deleted_by UUID REFERENCES users(id);

-- ---------------------------------------------------------------
-- Names unique among live departments only, so a deleted "Sales" does
-- not block a new one. Fresh databases carry departments_name_key from
-- 002; databases that once had db/schema.sql hand-applied have no name
-- constraint at all — so it is looked up by its column, not its name.
-- ---------------------------------------------------------------
DO $$
DECLARE
    c TEXT;
BEGIN
    FOR c IN
        SELECT con.conname
        FROM pg_constraint con
        WHERE con.conrelid = 'departments'::regclass
          AND con.contype  = 'u'
          AND (SELECT array_agg(att.attname::text)
               FROM unnest(con.conkey) k
               JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = k)
              = ARRAY['name']
    LOOP
        EXECUTE format('ALTER TABLE departments DROP CONSTRAINT %I', c);
    END LOOP;
END$$;

CREATE UNIQUE INDEX IF NOT EXISTS uq_departments_live_name
    ON departments (name)
    WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------
-- delete_department(): the one way to delete a department.
--
-- Same shape as delete_document() (009): SECURITY DEFINER, identity from
-- app.current_user_id of the caller's transaction, refusal with 42501
-- for anyone who is not an administrator. Documents are removed the way
-- delete_document removes one — chunks and embeddings purged, jobs
-- failed, rows kept as tombstones — under the same row locks, so the
-- ingestion worker's deleted_at check (ingest/mod.rs) covers this too.
--
-- Returns the ids of the documents it deleted (for the audit row) and
-- the blob hashes no live document uses any more (for the app to delete
-- after COMMIT).
-- ---------------------------------------------------------------
CREATE OR REPLACE FUNCTION delete_department(dept_id UUID)
RETURNS TABLE (document_ids UUID[], orphaned_blobs TEXT[])
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
DECLARE
    uid      UUID := current_user_id();
    dept     departments%ROWTYPE;
    is_admin BOOLEAN;
    doc_ids  UUID[];
    hashes   TEXT[];
BEGIN
    IF uid IS NULL THEN
        RAISE EXCEPTION 'no user identity in this transaction' USING ERRCODE = 'insufficient_privilege';
    END IF;

    SELECT u.is_admin INTO is_admin FROM users u WHERE u.id = uid;
    IF NOT COALESCE(is_admin, false) THEN
        RAISE EXCEPTION 'only an administrator can delete a department' USING ERRCODE = 'insufficient_privilege';
    END IF;

    SELECT * INTO dept FROM departments d WHERE d.id = dept_id AND d.deleted_at IS NULL FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'department not found' USING ERRCODE = 'no_data_found';
    END IF;
    IF dept.is_default THEN
        -- Every new profile joins it (ADR-0016); without it profile
        -- creation fails.
        RAISE EXCEPTION 'the default department cannot be deleted' USING ERRCODE = 'restrict_violation';
    END IF;

    -- Lock the live documents first (serializes with the ingestion
    -- worker and with delete_document), then collect them.
    PERFORM 1 FROM documents d WHERE d.department_id = dept_id AND d.deleted_at IS NULL FOR UPDATE;
    SELECT COALESCE(array_agg(d.id), '{}'), COALESCE(array_agg(DISTINCT d.file_hash), '{}')
      INTO doc_ids, hashes
      FROM documents d
     WHERE d.department_id = dept_id AND d.deleted_at IS NULL;

    DELETE FROM chunks c WHERE c.department_id = dept_id;       -- cascades to chunk_embeddings

    UPDATE ingestion_jobs j
    SET status = 'failed', error = 'department deleted', finished_at = now()
    WHERE j.document_id = ANY (doc_ids) AND j.status IN ('queued', 'running');

    UPDATE documents d
    SET deleted_at = now(), deleted_by = uid, updated_at = now()
    WHERE d.id = ANY (doc_ids);

    DELETE FROM department_adapters da WHERE da.department_id = dept_id;
    DELETE FROM department_members  dm WHERE dm.department_id = dept_id;

    UPDATE departments d SET deleted_at = now(), deleted_by = uid WHERE d.id = dept_id;

    RETURN QUERY
    SELECT doc_ids,
           COALESCE(ARRAY(SELECT h FROM unnest(hashes) h
                          WHERE NOT EXISTS (SELECT 1 FROM documents o
                                            WHERE o.file_hash = h AND o.deleted_at IS NULL)),
                    '{}');
END;
$$;

REVOKE ALL ON FUNCTION delete_department(UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION delete_department(UUID) TO app_user;
