-- =============================================================
-- Forward-fix migration. Do NOT edit 001-007 (see 005's header).
--
-- Retry with backoff for ingestion jobs (ADR-0010). Until now a job
-- that hit a transient error (embedding sidecar timeout, 5xx) went
-- straight to 'failed' and `attempts` counted nothing — there was no
-- second attempt. The worker now requeues transient failures with
-- `run_after` pushed into the future and only claims jobs whose
-- `run_after` has passed.
-- =============================================================

ALTER TABLE ingestion_jobs
    ADD COLUMN IF NOT EXISTS run_after TIMESTAMPTZ NOT NULL DEFAULT now();

CREATE INDEX IF NOT EXISTS idx_ingestion_jobs_queued_run_after
    ON ingestion_jobs (run_after)
    WHERE status = 'queued';
