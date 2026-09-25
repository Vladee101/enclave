-- =============================================================
-- Forward-fix migration. Do NOT edit 001-014 (see 005's header).
--
-- chunks (document_id): a question limited to chosen documents (the chat
-- panel) reads their chunks by document, and delete_document() purges by
-- document. Both scanned the department's chunks without it — 50 000
-- rows for one spreadsheet.
-- =============================================================

CREATE INDEX IF NOT EXISTS idx_chunks_document_id ON chunks (document_id);
