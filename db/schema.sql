-- ============================================================================
-- Corporate RAG + LoRA desktop app — PostgreSQL schema
-- Target: PostgreSQL 16+ with the pgvector extension.
-- Security model: department-scoped Row-Level Security, fail-closed.
-- This is a fresh-database script; run top to bottom.
--
-- ⚠️ NOT THE LIVE SCHEMA. This was the originally-provided reference design;
-- the actual application evolved a different (and now reconciled) schema in
-- `enclave/migrations/001-005*.sql`, applied automatically via
-- `sqlx::migrate!` on every app startup. Table/column names differ in
-- several places (e.g. `memberships`+`roles` here vs. `department_members`
-- there; this file was the origin of `password_hash`/`title`/`file_hash`,
-- which migration 005 backported into the real schema). Nothing in the app
-- reads or applies this file. Keep it only as historical reference for the
-- original design intent; treat `enclave/migrations/` as the source of truth.
-- ============================================================================

-- ----------------------------------------------------------------------------
-- 0. Extensions
-- ----------------------------------------------------------------------------
CREATE EXTENSION IF NOT EXISTS vector;   -- pgvector: ANN search over embeddings
-- gen_random_uuid() is built into PostgreSQL 13+, so no pgcrypto is required.

-- ----------------------------------------------------------------------------
-- 1. Application role
-- The app MUST connect as this non-superuser role. Superusers and any role with
-- BYPASSRLS skip every policy below, which would make the access-control story
-- decorative. Run this block once as a superuser.
-- ----------------------------------------------------------------------------
DO $$
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_user') THEN
    CREATE ROLE app_user LOGIN PASSWORD 'change-me'
      NOSUPERUSER NOBYPASSRLS NOCREATEDB NOCREATEROLE;
  END IF;
END $$;

-- ----------------------------------------------------------------------------
-- 2. Identity & RBAC
-- A role is held *within* a department, so the grant is the join of all three.
-- ----------------------------------------------------------------------------
CREATE TABLE users (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  username      text NOT NULL UNIQUE,
  email         text NOT NULL UNIQUE,
  password_hash text NOT NULL,
  is_active     boolean NOT NULL DEFAULT true,
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE roles (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name        text NOT NULL UNIQUE,
  description text
);

CREATE TABLE departments (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name       text NOT NULL,
  slug       text NOT NULL UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- The grant table. UNIQUE(user_id, department_id) lets one person be dept_admin
-- of Legal and viewer of Finance, but only one role per department.
CREATE TABLE memberships (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id       uuid NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
  department_id uuid NOT NULL REFERENCES departments(id) ON DELETE CASCADE,
  role_id       uuid NOT NULL REFERENCES roles(id)       ON DELETE RESTRICT,
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (user_id, department_id)
);

-- ----------------------------------------------------------------------------
-- 3. Knowledge store
-- ----------------------------------------------------------------------------
CREATE TABLE documents (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  department_id uuid NOT NULL REFERENCES departments(id) ON DELETE RESTRICT,
  uploaded_by   uuid REFERENCES users(id) ON DELETE SET NULL,
  title         text NOT NULL,
  file_hash     text NOT NULL,            -- sha-256 of source bytes, for dedup
  mime_type     text NOT NULL,
  byte_size     bigint NOT NULL,
  status        text NOT NULL DEFAULT 'pending'
                  CHECK (status IN ('pending','ingesting','ready','failed')),
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (department_id, file_hash)        -- same file may exist in two depts
);

-- department_id here is denormalized from documents. This is a deliberate 3NF
-- violation: it lets RLS and the full-text filter run on chunks without joining
-- back to documents on every retrieval. The ingestion pipeline copies it from
-- the parent document; a trigger could enforce it (see note at the end).
CREATE TABLE chunks (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  document_id   uuid NOT NULL REFERENCES documents(id)   ON DELETE CASCADE,
  department_id uuid NOT NULL REFERENCES departments(id) ON DELETE RESTRICT,
  chunk_index   int  NOT NULL,
  content       text NOT NULL,
  token_count   int  NOT NULL,
  -- 2-arg to_tsvector(regconfig, text) is IMMUTABLE, so it is valid in a
  -- generated column; the 1-arg form is only STABLE and would be rejected.
  content_tsv   tsvector GENERATED ALWAYS AS (to_tsvector('english', content)) STORED,
  UNIQUE (document_id, chunk_index)
);

CREATE TABLE embedding_models (
  id        uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name      text NOT NULL UNIQUE,         -- e.g. 'nomic-embed-text'
  dimension int  NOT NULL,
  provider  text NOT NULL DEFAULT 'ollama'
);

-- The vector column is pinned to 768 dims (nomic-embed-text / bge-base class),
-- because a single HNSW index requires a fixed dimension. The embedding_models
-- registry still records each model's dimension for validation. To serve models
-- of *different* dimensions at once, change the type to bare `vector`, drop the
-- shared HNSW index, and build one partial index per model:
--   CREATE INDEX ... USING hnsw (embedding vector_cosine_ops)
--   WHERE embedding_model_id = '<uuid>';
-- department_id is denormalized again so the ANN search can pre-filter by dept.
CREATE TABLE chunk_embeddings (
  id                 uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  chunk_id           uuid NOT NULL REFERENCES chunks(id)            ON DELETE CASCADE,
  embedding_model_id uuid NOT NULL REFERENCES embedding_models(id) ON DELETE RESTRICT,
  department_id      uuid NOT NULL REFERENCES departments(id)      ON DELETE RESTRICT,
  embedding          vector(768) NOT NULL,
  UNIQUE (chunk_id, embedding_model_id)
);

-- ----------------------------------------------------------------------------
-- 4. LoRA adapters
-- ----------------------------------------------------------------------------
CREATE TABLE adapters (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  name       text NOT NULL UNIQUE,
  file_path  text NOT NULL,              -- path to the GGUF adapter on disk
  base_model text NOT NULL,              -- base the adapter was trained against
  rank       int  NOT NULL CHECK (rank  > 0),
  alpha      int  NOT NULL CHECK (alpha > 0),
  file_hash  text NOT NULL UNIQUE,
  is_active  boolean NOT NULL DEFAULT true,
  created_at timestamptz NOT NULL DEFAULT now()
);

-- Junction (not a single FK on adapters) so one "formal-tone" adapter can serve
-- several departments. The per-assignment `scale` maps straight onto the
-- llama-server runtime payload: lora: [{ id, scale }, ...].
CREATE TABLE department_adapters (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  department_id uuid NOT NULL REFERENCES departments(id) ON DELETE CASCADE,
  adapter_id    uuid NOT NULL REFERENCES adapters(id)    ON DELETE CASCADE,
  scale         real NOT NULL DEFAULT 1.0 CHECK (scale >= 0 AND scale <= 2),
  is_default    boolean NOT NULL DEFAULT false,
  UNIQUE (department_id, adapter_id)
);

-- At most one default adapter per department (partial unique index).
CREATE UNIQUE INDEX uq_department_default_adapter
  ON department_adapters (department_id) WHERE is_default;

-- ----------------------------------------------------------------------------
-- 5. Async ingestion
-- ----------------------------------------------------------------------------
CREATE TABLE ingestion_jobs (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  document_id uuid NOT NULL REFERENCES documents(id) ON DELETE CASCADE,
  status      text NOT NULL DEFAULT 'queued'
                CHECK (status IN ('queued','running','succeeded','failed')),
  error       text,
  attempts    int NOT NULL DEFAULT 0,
  started_at  timestamptz,
  finished_at timestamptz,
  created_at  timestamptz NOT NULL DEFAULT now()
);

-- ----------------------------------------------------------------------------
-- 6. Conversations & observability
-- ----------------------------------------------------------------------------
CREATE TABLE conversations (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id       uuid NOT NULL REFERENCES users(id)       ON DELETE CASCADE,
  department_id uuid NOT NULL REFERENCES departments(id) ON DELETE RESTRICT,
  title         text,
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE messages (
  id              uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  conversation_id uuid NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
  role            text NOT NULL CHECK (role IN ('user','assistant','system')),
  content         text NOT NULL,
  created_at      timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE query_logs (
  id               uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id          uuid NOT NULL REFERENCES users(id)         ON DELETE CASCADE,
  department_id    uuid NOT NULL REFERENCES departments(id)   ON DELETE RESTRICT,
  conversation_id  uuid REFERENCES conversations(id)          ON DELETE SET NULL,
  query_text       text NOT NULL,
  adapters_applied jsonb NOT NULL DEFAULT '[]'::jsonb,  -- the [{id,scale}] sent to llama-server
  latency_ms       int,
  created_at       timestamptz NOT NULL DEFAULT now()
);

-- Retrieval provenance: which passages, at what rank/score, fed which answer.
-- This is the "show me the sources behind this response" feature, for free.
CREATE TABLE query_retrieved_chunks (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  query_log_id uuid NOT NULL REFERENCES query_logs(id) ON DELETE CASCADE,
  chunk_id     uuid NOT NULL REFERENCES chunks(id)     ON DELETE CASCADE,
  rank         int  NOT NULL,
  score        real NOT NULL,
  UNIQUE (query_log_id, chunk_id)
);

CREATE TABLE audit_events (
  id          uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  user_id     uuid REFERENCES users(id) ON DELETE SET NULL,
  action      text NOT NULL,            -- 'login', 'document.upload', 'membership.grant', ...
  entity_type text NOT NULL,
  entity_id   uuid,
  metadata    jsonb NOT NULL DEFAULT '{}'::jsonb,
  created_at  timestamptz NOT NULL DEFAULT now()
);

-- ----------------------------------------------------------------------------
-- 7. Indexes
-- PostgreSQL does not auto-index foreign keys; add them for joins and filters.
-- ----------------------------------------------------------------------------
CREATE INDEX idx_memberships_user       ON memberships (user_id);
CREATE INDEX idx_memberships_department ON memberships (department_id);
CREATE INDEX idx_documents_department   ON documents (department_id);
CREATE INDEX idx_chunks_document        ON chunks (document_id);
CREATE INDEX idx_chunks_department      ON chunks (department_id);
CREATE INDEX idx_chunk_emb_chunk        ON chunk_embeddings (chunk_id);
CREATE INDEX idx_chunk_emb_department   ON chunk_embeddings (department_id);
CREATE INDEX idx_ingestion_document     ON ingestion_jobs (document_id);
CREATE INDEX idx_conversations_user     ON conversations (user_id);
CREATE INDEX idx_messages_conversation  ON messages (conversation_id);
CREATE INDEX idx_query_logs_user        ON query_logs (user_id);
CREATE INDEX idx_qrc_query_log          ON query_retrieved_chunks (query_log_id);
CREATE INDEX idx_dept_adapters_dept     ON department_adapters (department_id);

-- Hybrid search: ANN over embeddings (cosine) + full-text over content.
CREATE INDEX idx_chunk_emb_hnsw ON chunk_embeddings
  USING hnsw (embedding vector_cosine_ops) WITH (m = 16, ef_construction = 64);
CREATE INDEX idx_chunks_tsv ON chunks USING gin (content_tsv);

-- ----------------------------------------------------------------------------
-- 8. RLS helper functions
-- ----------------------------------------------------------------------------
-- The authenticated user id, read from a per-transaction session variable.
-- The app sets it right after authenticating, scoped to the transaction:
--   SELECT set_config('app.current_user_id', $user_id::text, true);  -- true = tx-local
-- Tx-local matters under pooling: a value set with `false` leaks to the next
-- request that reuses the connection. NULL when unset -> policies fail closed.
CREATE OR REPLACE FUNCTION app_current_user_id() RETURNS uuid
  LANGUAGE sql STABLE AS $$
    SELECT nullif(current_setting('app.current_user_id', true), '')::uuid
$$;

-- True if the current user has a membership in the given department. Safe inside
-- policies: it only reads the caller's own membership rows, which the
-- memberships SELECT policy already permits — so no SECURITY DEFINER is needed
-- and there is no policy recursion (memberships' own policy uses only
-- app_current_user_id(), which touches no RLS table).
CREATE OR REPLACE FUNCTION app_user_in_department(dept uuid) RETURNS boolean
  LANGUAGE sql STABLE AS $$
    SELECT EXISTS (
      SELECT 1 FROM memberships m
      WHERE m.user_id = app_current_user_id()
        AND m.department_id = dept
    )
$$;

-- ----------------------------------------------------------------------------
-- 9. Enable + FORCE RLS, then policies
-- FORCE makes the table owner subject to policies too; combined with connecting
-- as app_user this closes the "owner sees everything" hole.
-- ----------------------------------------------------------------------------
ALTER TABLE departments            ENABLE ROW LEVEL SECURITY;
ALTER TABLE departments            FORCE  ROW LEVEL SECURITY;
ALTER TABLE memberships            ENABLE ROW LEVEL SECURITY;
ALTER TABLE memberships            FORCE  ROW LEVEL SECURITY;
ALTER TABLE documents              ENABLE ROW LEVEL SECURITY;
ALTER TABLE documents              FORCE  ROW LEVEL SECURITY;
ALTER TABLE chunks                 ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunks                 FORCE  ROW LEVEL SECURITY;
ALTER TABLE chunk_embeddings       ENABLE ROW LEVEL SECURITY;
ALTER TABLE chunk_embeddings       FORCE  ROW LEVEL SECURITY;
ALTER TABLE ingestion_jobs         ENABLE ROW LEVEL SECURITY;
ALTER TABLE ingestion_jobs         FORCE  ROW LEVEL SECURITY;
ALTER TABLE conversations          ENABLE ROW LEVEL SECURITY;
ALTER TABLE conversations          FORCE  ROW LEVEL SECURITY;
ALTER TABLE messages               ENABLE ROW LEVEL SECURITY;
ALTER TABLE messages               FORCE  ROW LEVEL SECURITY;
ALTER TABLE query_logs             ENABLE ROW LEVEL SECURITY;
ALTER TABLE query_logs             FORCE  ROW LEVEL SECURITY;
ALTER TABLE query_retrieved_chunks ENABLE ROW LEVEL SECURITY;
ALTER TABLE query_retrieved_chunks FORCE  ROW LEVEL SECURITY;

-- Departments and memberships are read-filtered; provisioning them (creating
-- departments, granting memberships) is an admin path that runs as a privileged
-- role outside these policies.
CREATE POLICY dept_visible ON departments
  FOR SELECT USING (app_user_in_department(id));

CREATE POLICY membership_own ON memberships
  FOR SELECT USING (user_id = app_current_user_id());

-- Department-scoped knowledge: visible and writable only within your departments.
CREATE POLICY doc_dept_isolation ON documents
  FOR ALL USING (app_user_in_department(department_id))
          WITH CHECK (app_user_in_department(department_id));

CREATE POLICY chunk_dept_isolation ON chunks
  FOR ALL USING (app_user_in_department(department_id))
          WITH CHECK (app_user_in_department(department_id));

CREATE POLICY emb_dept_isolation ON chunk_embeddings
  FOR ALL USING (app_user_in_department(department_id))
          WITH CHECK (app_user_in_department(department_id));

CREATE POLICY ingestion_dept_isolation ON ingestion_jobs
  FOR ALL USING (EXISTS (
            SELECT 1 FROM documents d
            WHERE d.id = ingestion_jobs.document_id
              AND app_user_in_department(d.department_id)))
          WITH CHECK (EXISTS (
            SELECT 1 FROM documents d
            WHERE d.id = ingestion_jobs.document_id
              AND app_user_in_department(d.department_id)));

-- Conversations and logs are personal: you see your own, and may only create
-- them inside a department you belong to.
CREATE POLICY conv_owner ON conversations
  FOR ALL USING (user_id = app_current_user_id())
          WITH CHECK (user_id = app_current_user_id()
                      AND app_user_in_department(department_id));

CREATE POLICY msg_owner ON messages
  FOR ALL USING (EXISTS (
            SELECT 1 FROM conversations c
            WHERE c.id = messages.conversation_id
              AND c.user_id = app_current_user_id()))
          WITH CHECK (EXISTS (
            SELECT 1 FROM conversations c
            WHERE c.id = messages.conversation_id
              AND c.user_id = app_current_user_id()));

CREATE POLICY qlog_owner ON query_logs
  FOR ALL USING (user_id = app_current_user_id())
          WITH CHECK (user_id = app_current_user_id()
                      AND app_user_in_department(department_id));

CREATE POLICY qrc_owner ON query_retrieved_chunks
  FOR ALL USING (EXISTS (
            SELECT 1 FROM query_logs q
            WHERE q.id = query_retrieved_chunks.query_log_id
              AND q.user_id = app_current_user_id()))
          WITH CHECK (EXISTS (
            SELECT 1 FROM query_logs q
            WHERE q.id = query_retrieved_chunks.query_log_id
              AND q.user_id = app_current_user_id()));

-- ----------------------------------------------------------------------------
-- 10. Grants
-- Table privileges are necessary but not sufficient: RLS still constrains which
-- rows app_user can touch. This is defense in depth.
-- ----------------------------------------------------------------------------
GRANT USAGE ON SCHEMA public TO app_user;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO app_user;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO app_user;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO app_user;
ALTER DEFAULT PRIVILEGES IN SCHEMA public
  GRANT EXECUTE ON FUNCTIONS TO app_user;

-- ----------------------------------------------------------------------------
-- 11. Triggers & seed data
-- ----------------------------------------------------------------------------
CREATE OR REPLACE FUNCTION set_updated_at() RETURNS trigger
  LANGUAGE plpgsql AS $$
BEGIN
  NEW.updated_at := now();
  RETURN NEW;
END $$;

CREATE TRIGGER trg_documents_updated_at
  BEFORE UPDATE ON documents
  FOR EACH ROW EXECUTE FUNCTION set_updated_at();

INSERT INTO roles (name, description) VALUES
  ('admin',      'Global administrator'),
  ('dept_admin', 'Manages a single department'),
  ('member',     'Read and write within a department'),
  ('viewer',     'Read-only within a department')
ON CONFLICT (name) DO NOTHING;

-- ----------------------------------------------------------------------------
-- Note on the denormalized department_id on chunks / chunk_embeddings:
-- it is set by the ingestion pipeline from the parent document. If you would
-- rather have the database guarantee it, add a BEFORE INSERT/UPDATE trigger
-- that copies documents.department_id, instead of trusting application code.
-- It is left out here to keep the ingest write path cheap.
-- ============================================================================
