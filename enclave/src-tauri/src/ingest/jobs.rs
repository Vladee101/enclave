use sqlx::{PgPool, Row};
use tauri::{AppHandle, Manager};
use tracing::{error, info};
use uuid::Uuid;

/// Background job runner that polls `ingestion_jobs` for queued work
/// and drives the ingestion pipeline (ADR-0010).
pub async fn run_job_loop(pool: PgPool, app: AppHandle) {
    info!("Ingestion job runner started.");

    let blob_root = match app.path().app_data_dir() {
        Ok(dir) => dir.join("blobs"),
        Err(e) => {
            error!("Could not resolve app data dir for blob store; ingestion cannot run: {e:#}");
            return;
        }
    };

    // Reap orphaned jobs: any row still 'running' at startup belongs to a
    // previous process that died mid-flight (nothing survives a restart while
    // running). Reset them to 'queued' so they get retried instead of stranding.
    match sqlx::query("UPDATE ingestion_jobs SET status = 'queued', started_at = NULL WHERE status = 'running'")
        .execute(&pool)
        .await
    {
        Ok(r) if r.rows_affected() > 0 => info!("Reaped {} orphaned job(s) at startup.", r.rows_affected()),
        Ok(_) => {}
        Err(e) => error!("Startup job reaper failed: {e:#}"),
    }

    loop {
        match tick(&pool, &app, &blob_root).await {
            Ok(true)  => {} // processed a job, check immediately for more
            Ok(false) => {
                // No queued jobs; sleep before polling again.
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
            Err(e) => {
                error!("Job runner error: {e:#}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }
    }
}

/// Claim and process one queued job. Returns `true` if a job was found.
async fn tick(pool: &PgPool, app: &AppHandle, blob_root: &std::path::Path) -> anyhow::Result<bool> {
    // Atomically claim the oldest queued job.
    let row = sqlx::query(
        r#"
        WITH claimed AS (
            SELECT id, document_id
            FROM   ingestion_jobs
            WHERE  status = 'queued'
            ORDER BY created_at
            LIMIT  1
            FOR UPDATE SKIP LOCKED
        )
        UPDATE ingestion_jobs j
        SET    status     = 'running',
               started_at = now(),
               attempts   = attempts + 1
        FROM   claimed
        WHERE  j.id = claimed.id
        RETURNING j.id, j.document_id
        "#
    )
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else {
        return Ok(false);
    };

    let job_id: Uuid = row.try_get(0)?;
    let document_id: Uuid = row.try_get(1)?;

    info!("Processing ingestion job {} for document {}", job_id, document_id);

    let llm_state = app.state::<Option<crate::llm::LlmClient>>();
    let Some(llm) = llm_state.inner().as_ref() else {
        let msg = "llama-server sidecar unavailable; cannot embed document";
        error!("Job {} failed: {}", job_id, msg);
        sqlx::query(
            r#"
            UPDATE ingestion_jobs
            SET status = 'failed', finished_at = now(), error = $2
            WHERE id = $1
            "#
        )
        .bind(job_id)
        .bind(msg)
        .execute(pool)
        .await?;
        return Ok(true);
    };
    let result = crate::ingest::ingest_document(pool, llm, blob_root, document_id).await;

    match result {
        Ok(chunk_count) => {
            info!("Job {} succeeded: {} chunks", job_id, chunk_count);
            sqlx::query(
                r#"
                UPDATE ingestion_jobs
                SET status = 'succeeded', finished_at = now()
                WHERE id = $1
                "#
            )
            .bind(job_id)
            .execute(pool)
            .await?;
        }
        Err(e) => {
            let err_text = format!("{e:#}");
            error!("Job {} failed: {}", job_id, err_text);

            // Mark document failed too.
            sqlx::query("UPDATE documents SET status = 'failed', updated_at = now() WHERE id = $1")
                .bind(document_id)
                .execute(pool)
                .await?;

            sqlx::query(
                r#"
                UPDATE ingestion_jobs
                SET status = 'failed', finished_at = now(), error = $2
                WHERE id = $1
                "#
            )
            .bind(job_id)
            .bind(err_text)
            .execute(pool)
            .await?;
        }
    }

    Ok(true)
}
