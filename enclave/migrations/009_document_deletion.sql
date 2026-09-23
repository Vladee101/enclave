-- =============================================================
-- Forward-fix migration. Do NOT edit 001-008 (see 005's header).
--
-- Document deletion (ADR-0015): the uploader or an administrator can
-- delete a document. Deletion purges content — chunks and their
-- embeddings are physically removed — and leaves a tombstone row in
-- `documents` (deleted_at / deleted_by, title, hash), because audit_log
-- query rows refer to documents by id.
--
-- The rule is enforced in the database, like every other access rule
-- here (ADR-0008): app_user loses every direct way to remove or hide
-- content, and the only way left is delete_document(), which checks
-- "uploader or admin" itself.
-- =============================================================

-- ---------------------------------------------------------------
-- 1. Tombstone columns
-- ---------------------------------------------------------------
ALTER TABLE documents ADD COLUMN IF NOT EXISTS deleted_at TIMESTAMPTZ;
ALTER TABLE documents ADD COLUMN IF NOT EXISTS deleted_by UUID REFERENCES users(id);

-- ---------------------------------------------------------------
-- 2. Uniqueness only among live documents, so the same file can be
--    uploaded again after its document was deleted. The existing
--    constraint's name differs between databases (005 creates
--    documents_department_file_hash_key; databases that once had
--    db/schema.sql hand-applied carry documents_department_id_file_hash_key),
--    so it is found by its columns.
-- ---------------------------------------------------------------
DO $$
DECLARE
    c TEXT;
BEGIN
    FOR c IN
        SELECT con.conname
        FROM pg_constraint con
        WHERE con.conrelid = 'documents'::regclass
          AND con.contype  = 'u'
          AND (SELECT array_agg(att.attname::text ORDER BY att.attname)
               FROM unnest(con.conkey) k
               JOIN pg_attribute att ON att.attrelid = con.conrelid AND att.attnum = k)
              = ARRAY['department_id', 'file_hash']
    LOOP
        EXECUTE format('ALTER TABLE documents DROP CONSTRAINT %I', c);
    END LOOP;
END$$;

CREATE UNIQUE INDEX IF NOT EXISTS uq_documents_live_department_file_hash
    ON documents (department_id, file_hash)
    WHERE deleted_at IS NULL;

-- ---------------------------------------------------------------
-- 3. Close the direct paths. Privileges, not policies: a revoked
--    privilege holds no matter how many permissive policies a table
--    carries (the dormant ones from ADR-0011 included).
--
--    documents: no DELETE at all; UPDATE only of the columns the app
--    legitimately changes (re-queue on re-upload), so a member cannot
--    set deleted_at on someone else's document and hide it.
--    chunks / chunk_embeddings: written only by the ingestion worker
--    (its own role). A member able to UPDATE or DELETE them could erase
--    a document's content without deleting the document — the same
--    act the uploader-or-admin rule exists to restrict.
-- ---------------------------------------------------------------
REVOKE DELETE ON documents FROM app_user;
REVOKE UPDATE ON documents FROM app_user;
GRANT  UPDATE (status, updated_at) ON documents TO app_user;

REVOKE INSERT, UPDATE, DELETE ON chunks, chunk_embeddings FROM app_user;

DROP POLICY IF EXISTS documents_dept_delete  ON documents;
DROP POLICY IF EXISTS chunks_dept_insert     ON chunks;
DROP POLICY IF EXISTS chunks_dept_update     ON chunks;
DROP POLICY IF EXISTS chunks_dept_delete     ON chunks;
DROP POLICY IF EXISTS embeddings_dept_insert ON chunk_embeddings;
DROP POLICY IF EXISTS embeddings_dept_update ON chunk_embeddings;
DROP POLICY IF EXISTS embeddings_dept_delete ON chunk_embeddings;

-- ---------------------------------------------------------------
-- 4. delete_document(): the one way to delete.
--
--    SECURITY DEFINER so it can purge chunks app_user can no longer
--    touch; the caller's identity still comes from app.current_user_id
--    (current_user_id() reads the session setting, not the role), so it
--    must run inside the caller's transaction after set_current_user —
--    unset identity fails closed.
--
--    Allowed: an administrator (any document — including one misfiled
--    into a department they are not in), or the uploader while still a
--    member of the document's department.
--
--    Returns the document's file_hash and whether any other live
--    document still uses that blob; the app deletes the blob file only
--    when none does (same bytes may live in another department as a
--    separate document with its own permissions).
-- ---------------------------------------------------------------
CREATE OR REPLACE FUNCTION delete_document(doc_id UUID)
RETURNS TABLE (file_hash TEXT, department_id UUID, blob_still_used BOOLEAN)
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
DECLARE
    uid      UUID := current_user_id();
    doc      documents%ROWTYPE;
    is_admin BOOLEAN;
BEGIN
    IF uid IS NULL THEN
        RAISE EXCEPTION 'no user identity in this transaction' USING ERRCODE = 'insufficient_privilege';
    END IF;

    -- Row lock: serializes with the ingestion worker, which locks the
    -- same row before writing chunks (ingest/mod.rs).
    SELECT * INTO doc FROM documents d WHERE d.id = doc_id AND d.deleted_at IS NULL FOR UPDATE;
    IF NOT FOUND THEN
        RAISE EXCEPTION 'document not found' USING ERRCODE = 'no_data_found';
    END IF;

    SELECT u.is_admin INTO is_admin FROM users u WHERE u.id = uid;

    IF NOT (COALESCE(is_admin, false)
            OR (doc.uploaded_by = uid AND is_member_of(doc.department_id))) THEN
        -- Same message whether the document is someone else's or in a
        -- department the caller cannot see: no existence oracle.
        RAISE EXCEPTION 'only the uploader or an administrator can delete this document'
            USING ERRCODE = 'insufficient_privilege';
    END IF;

    DELETE FROM chunks c WHERE c.document_id = doc_id;          -- cascades to chunk_embeddings

    UPDATE ingestion_jobs j
    SET status = 'failed', error = 'document deleted', finished_at = now()
    WHERE j.document_id = doc_id AND j.status IN ('queued', 'running');

    UPDATE documents d
    SET deleted_at = now(), deleted_by = uid, updated_at = now()
    WHERE d.id = doc_id;

    RETURN QUERY
    SELECT doc.file_hash,
           doc.department_id,
           EXISTS (SELECT 1 FROM documents o
                   WHERE o.file_hash = doc.file_hash AND o.id <> doc_id AND o.deleted_at IS NULL);
END;
$$;

REVOKE ALL ON FUNCTION delete_document(UUID) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION delete_document(UUID) TO app_user;
