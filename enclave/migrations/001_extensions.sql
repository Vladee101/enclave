-- ADR-0005: pgvector for ANN similarity search
-- ADR-0006: pg_trgm supports trigram indexes (used by FTS leg)
CREATE EXTENSION IF NOT EXISTS vector;
CREATE EXTENSION IF NOT EXISTS pg_trgm;
