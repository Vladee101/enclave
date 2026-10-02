-- =============================================================
-- Forward-fix migration. Do NOT edit 001-017 (see 005's header).
--
-- documents.ocr_language: the recognizer language when the document's
-- text came from OCR of a scan (ADR-0028), NULL when the file had text
-- of its own. Shown next to the document, so text read by a machine — and
-- read without Russian, on a Windows without it — is visible as such.
--
-- Written by the ingestion worker (table-level UPDATE on documents,
-- migration 005); app_user only reads it — its UPDATE stays limited to
-- status / updated_at (migration 012).
-- =============================================================

ALTER TABLE documents ADD COLUMN IF NOT EXISTS ocr_language TEXT;
