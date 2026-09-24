-- =============================================================
-- Forward-fix migration. Do NOT edit 001-013 (see 005's header).
--
-- Spreadsheets as typed tables, for calculations over them (ADR-0022).
--
-- RAG answers questions about a row but cannot count or sum: "общая
-- сумма договоров Петровой" is in no chunk. Each sheet of an ingested
-- spreadsheet is also stored here — its columns with inferred types,
-- its rows as JSONB arrays — and a question is answered by a
-- parameterized aggregate the core compiles from a validated plan.
--
-- Same rules as chunks (ADR-0008, ADR-0009, ADR-0018):
--   * department_id denormalized on both tables, stamped by the
--     ingestion worker (BYPASSRLS) in the same transaction as the chunks;
--   * app_user reads under RLS, in the once-per-query form (ADR-0021),
--     and writes nothing;
--   * a deleted document takes its tables with it (trigger below —
--     delete_document() and delete_department() both tombstone the
--     document row, and that is the one place to hook).
-- =============================================================

CREATE TABLE IF NOT EXISTS sheet_tables (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    document_id   UUID NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    department_id UUID NOT NULL REFERENCES departments(id),
    sheet         TEXT NOT NULL,
    -- Position among the workbook's sheets, for a stable order.
    sheet_index   INT  NOT NULL,
    row_count     INT  NOT NULL,
    -- [{"name", "type": number|date|text, "distinct", "top": [[value, rows]], "min", "max"}]
    columns       JSONB NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (document_id, sheet_index)
);

CREATE TABLE IF NOT EXISTS sheet_rows (
    table_id      UUID NOT NULL REFERENCES sheet_tables(id) ON DELETE CASCADE,
    department_id UUID NOT NULL REFERENCES departments(id),
    -- Row number as Excel shows it.
    row_number    INT  NOT NULL,
    -- One element per column, JSON null for an empty cell. Numbers are
    -- JSON numbers in number columns; everything else is a string.
    cells         JSONB NOT NULL,
    PRIMARY KEY (table_id, row_number)
);

CREATE INDEX IF NOT EXISTS idx_sheet_tables_document ON sheet_tables (document_id);

-- ---------------------------------------------------------------
-- RLS: members of the department read; nobody on app_pool writes.
-- ---------------------------------------------------------------
ALTER TABLE sheet_tables ENABLE ROW LEVEL SECURITY;
ALTER TABLE sheet_tables FORCE  ROW LEVEL SECURITY;
ALTER TABLE sheet_rows   ENABLE ROW LEVEL SECURITY;
ALTER TABLE sheet_rows   FORCE  ROW LEVEL SECURITY;

DROP POLICY IF EXISTS sheet_tables_dept_select ON sheet_tables;
CREATE POLICY sheet_tables_dept_select ON sheet_tables
    FOR SELECT USING (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

DROP POLICY IF EXISTS sheet_rows_dept_select ON sheet_rows;
CREATE POLICY sheet_rows_dept_select ON sheet_rows
    FOR SELECT USING (department_id = ANY ((SELECT current_user_department_ids())::uuid[]));

-- Explicit both ways: 012 removed the default privileges that used to
-- hand app_user writes on new tables, but a database may still carry
-- some for an owner 012 did not see.
REVOKE ALL ON sheet_tables, sheet_rows FROM app_user;
GRANT SELECT ON sheet_tables, sheet_rows TO app_user;

-- The worker writes them (BYPASSRLS — it stamps department_id itself).
GRANT SELECT, INSERT ON sheet_tables, sheet_rows TO ingest_worker;

-- ---------------------------------------------------------------
-- A tombstoned document loses its tables, like its chunks. Runs inside
-- delete_document() / delete_department() (SECURITY DEFINER), which are
-- the only paths that set deleted_at; the cascade removes the rows.
-- ---------------------------------------------------------------
CREATE OR REPLACE FUNCTION purge_sheet_tables()
RETURNS TRIGGER
LANGUAGE plpgsql
SET search_path = public, pg_temp
AS $$
BEGIN
    DELETE FROM sheet_tables t WHERE t.document_id = NEW.id;
    RETURN NEW;
END;
$$;

REVOKE ALL ON FUNCTION purge_sheet_tables() FROM PUBLIC;

DROP TRIGGER IF EXISTS documents_purge_sheet_tables ON documents;
CREATE TRIGGER documents_purge_sheet_tables
    AFTER UPDATE OF deleted_at ON documents
    FOR EACH ROW
    WHEN (OLD.deleted_at IS NULL AND NEW.deleted_at IS NOT NULL)
    EXECUTE FUNCTION purge_sheet_tables();
