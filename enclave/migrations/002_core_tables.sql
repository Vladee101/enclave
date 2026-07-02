-- =============================================================
-- ADR-0002, 0004, 0005, 0007, 0008, 0009, 0010
-- Core domain tables.  All department-scoped tables carry
-- department_id (denormalized per ADR-0009) so RLS policies
-- (ADR-0008) and ANN pre-filters (ADR-0006) can evaluate
-- against a local column without joins.
-- =============================================================

-- ---------------------------------------------------------------
-- Identity / access
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS users (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    username    TEXT NOT NULL UNIQUE,
    pin_hash    TEXT NOT NULL,          -- bcrypt / argon2 hash of local PIN
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS departments (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name        TEXT NOT NULL UNIQUE,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS department_members (
    user_id        UUID NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
    department_id  UUID NOT NULL REFERENCES departments(id) ON DELETE CASCADE,
    PRIMARY KEY (user_id, department_id)
);

-- ---------------------------------------------------------------
-- Document lifecycle  (ADR-0010)
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS documents (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    department_id  UUID NOT NULL REFERENCES departments(id) ON DELETE RESTRICT,
    filename       TEXT NOT NULL,
    mime_type      TEXT,
    byte_size      BIGINT,
    status         TEXT NOT NULL DEFAULT 'pending'
                       CHECK (status IN ('pending','ready','failed')),
    uploaded_by    UUID REFERENCES users(id),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------
-- Chunking  (ADR-0006, 0009)
-- department_id is copied from the parent document (ADR-0009):
-- a deliberate 3NF violation for hot-path filter performance.
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS chunks (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    document_id    UUID NOT NULL REFERENCES documents(id)   ON DELETE CASCADE,
    department_id  UUID NOT NULL REFERENCES departments(id) ON DELETE RESTRICT,
    chunk_index    INTEGER NOT NULL,
    content        TEXT NOT NULL,
    -- tsvector for the lexical FTS leg (ADR-0006)
    content_tsv    TSVECTOR GENERATED ALWAYS AS (
                       to_tsvector('english', content)
                   ) STORED,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (document_id, chunk_index)
);

-- ---------------------------------------------------------------
-- Embedding model registry  (ADR-0007)
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS embedding_models (
    id         UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name       TEXT NOT NULL UNIQUE,   -- e.g. 'nomic-embed-text-v1.5'
    dimension  INTEGER NOT NULL,       -- pinned to 768 per ADR-0007
    is_active  BOOLEAN NOT NULL DEFAULT false,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------
-- Embeddings  (ADR-0007, 0009)
-- Keyed by (chunk_id, embedding_model_id) so re-embedding is
-- additive, not destructive.  department_id denormalized (ADR-0009)
-- so the ANN pre-filter column is local to the indexed table.
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS chunk_embeddings (
    id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    chunk_id            UUID NOT NULL REFERENCES chunks(id)           ON DELETE CASCADE,
    embedding_model_id  UUID NOT NULL REFERENCES embedding_models(id) ON DELETE RESTRICT,
    department_id       UUID NOT NULL REFERENCES departments(id)       ON DELETE RESTRICT,
    embedding           VECTOR(768) NOT NULL,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (chunk_id, embedding_model_id)
);

-- ---------------------------------------------------------------
-- LoRA adapter registry  (ADR-0003, 0004)
-- scale maps directly onto the llama-server lora: [{id, scale}]
-- payload.
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS department_adapters (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    department_id  UUID NOT NULL REFERENCES departments(id) ON DELETE CASCADE,
    adapter_path   TEXT NOT NULL,        -- relative path inside binaries/
    scale          REAL NOT NULL DEFAULT 1.0,
    description    TEXT,
    is_active      BOOLEAN NOT NULL DEFAULT true,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- ---------------------------------------------------------------
-- Async ingestion jobs  (ADR-0010)
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS ingestion_jobs (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    document_id  UUID NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
    status       TEXT NOT NULL DEFAULT 'queued'
                     CHECK (status IN ('queued','running','succeeded','failed')),
    attempts     INTEGER NOT NULL DEFAULT 0,
    error_text   TEXT,
    queued_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at   TIMESTAMPTZ,
    finished_at  TIMESTAMPTZ
);

-- ---------------------------------------------------------------
-- Audit log
-- ---------------------------------------------------------------
CREATE TABLE IF NOT EXISTS audit_log (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id        UUID REFERENCES users(id),
    department_id  UUID REFERENCES departments(id),
    event_type     TEXT NOT NULL,   -- 'query' | 'upload' | 'login' | ...
    payload        JSONB,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
