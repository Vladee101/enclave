-- =============================================================
-- Forward-fix migration. Do NOT edit 001-016 (see 005's header).
--
-- The department a row is isolated by must be its parent's (ADR-0012).
-- chunks, chunk_embeddings, sheet_tables and sheet_rows carry a copy of
-- department_id (ADR-0009) and are written by ingest_worker, which
-- bypasses RLS: nothing checked the copy, so a worker bug stamping the
-- wrong department would have published a document's text to another
-- department, silently. Composite foreign keys make the database refuse
-- such a row, whoever writes it — superuser included.
--
-- A key (id, department_id) on each parent is unique because id alone is;
-- the extra unique index is what a composite foreign key must point at.
-- ON UPDATE is left as NO ACTION: moving a document with content to
-- another department is refused, not cascaded.
-- =============================================================

DO $$
DECLARE
    k RECORD;
BEGIN
    FOR k IN
        SELECT * FROM (VALUES
            ('documents',    'documents_id_department_key'),
            ('chunks',       'chunks_id_department_key'),
            ('sheet_tables', 'sheet_tables_id_department_key')
        ) AS t(tbl, name)
    LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = k.name) THEN
            EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I UNIQUE (id, department_id)', k.tbl, k.name);
        END IF;
    END LOOP;

    FOR k IN
        SELECT * FROM (VALUES
            ('chunks',           'chunks_department_matches_document',       'document_id', 'documents'),
            ('chunk_embeddings', 'chunk_embeddings_department_matches_chunk', 'chunk_id',    'chunks'),
            ('sheet_tables',     'sheet_tables_department_matches_document', 'document_id', 'documents'),
            ('sheet_rows',       'sheet_rows_department_matches_table',      'table_id',    'sheet_tables')
        ) AS t(tbl, name, parent_col, parent)
    LOOP
        IF NOT EXISTS (SELECT 1 FROM pg_constraint WHERE conname = k.name) THEN
            EXECUTE format(
                'ALTER TABLE %I ADD CONSTRAINT %I FOREIGN KEY (%I, department_id) '
                'REFERENCES %I (id, department_id) ON DELETE CASCADE',
                k.tbl, k.name, k.parent_col, k.parent
            );
        END IF;
    END LOOP;
END$$;
