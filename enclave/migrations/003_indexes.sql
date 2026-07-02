-- =============================================================
-- ADR-0006: HNSW on embeddings (dense leg), GIN on tsvector
--           (lexical leg), B-tree on department_id columns for
--           ANN pre-filtering (ADR-0009).
-- =============================================================

-- HNSW index for cosine ANN on the embedding column.
-- m=16, ef_construction=64 are sensible defaults for a per-org corpus.
CREATE INDEX IF NOT EXISTS idx_chunk_embeddings_hnsw
    ON chunk_embeddings
    USING hnsw (embedding vector_cosine_ops)
    WITH (m = 16, ef_construction = 64);

-- Partial HNSW per model (escape hatch from ADR-0007 if multiple
-- embedding dimensions are ever needed simultaneously).
-- Leave commented for now; uncomment and adapt if required.
-- CREATE INDEX idx_chunk_embeddings_hnsw_model
--     ON chunk_embeddings USING hnsw (embedding vector_cosine_ops)
--     WHERE embedding_model_id = '<model-uuid>';

-- GIN index for the lexical full-text search leg (ADR-0006).
CREATE INDEX IF NOT EXISTS idx_chunks_content_tsv
    ON chunks USING gin (content_tsv);

-- B-tree indexes on department_id for RLS predicate evaluation
-- and ANN pre-filter (ADR-0008, 0009).
CREATE INDEX IF NOT EXISTS idx_chunks_department_id
    ON chunks (department_id);

CREATE INDEX IF NOT EXISTS idx_chunk_embeddings_department_id
    ON chunk_embeddings (department_id);

CREATE INDEX IF NOT EXISTS idx_documents_department_id
    ON documents (department_id);

-- Ingestion job status lookup.
CREATE INDEX IF NOT EXISTS idx_ingestion_jobs_status
    ON ingestion_jobs (status)
    WHERE status IN ('queued', 'running');

CREATE INDEX IF NOT EXISTS idx_ingestion_jobs_document_id
    ON ingestion_jobs (document_id);

-- Audit log time-series.
CREATE INDEX IF NOT EXISTS idx_audit_log_created_at
    ON audit_log (created_at DESC);

CREATE INDEX IF NOT EXISTS idx_audit_log_user_id
    ON audit_log (user_id);
