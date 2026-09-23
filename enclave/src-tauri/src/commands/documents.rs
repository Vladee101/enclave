use serde::{Deserialize, Serialize};
use sha2::{Sha256, Digest};
use sqlx::FromRow;
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

use crate::{
    AppState,
    audit::{self, event},
    db::rls::set_current_user,
    session::Session,
};

#[derive(Serialize, FromRow, Debug)]
pub struct DocumentInfo {
    pub id:            Uuid,
    pub filename:      String,
    pub status:        String,
    pub department_id: Uuid,
    /// Whether the delete button should be offered. A UI hint only — the
    /// rule itself is enforced by `delete_document()` in the database.
    pub can_delete:    bool,
}

#[derive(Serialize, FromRow, Debug)]
pub struct JobStatus {
    pub job_id:      Uuid,
    pub document_id: Uuid,
    pub status:      String,
    pub attempts:    i32,
    pub error_text:  Option<String>,
}

/// Upload a document: create the `documents` row, queue an ingestion job,
/// and return immediately (ADR-0010 — 202-style).
///
/// `byte_size` isn't taken from the client: `documents.byte_size` is
/// NOT NULL on the live schema, and the true size is always known
/// server-side from the bytes actually received, so there's nothing to
/// trust the client for here. `mime_type` (also NOT NULL) falls back to
/// a generic default since browsers don't always report one.
#[derive(Deserialize)]
pub struct UploadArgs {
    pub department_id:  Uuid,
    pub filename:       String,
    pub mime_type:      Option<String>,
    pub file_contents:  Vec<u8>,
}

#[tauri::command]
pub async fn cmd_upload_document(
    app:     AppHandle,
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    args:    UploadArgs,
) -> Result<JobStatus, String> {
    let user_id = session.require()?.id;
    let digest = Sha256::digest(&args.file_contents);
    let file_hash: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    let byte_size = args.file_contents.len() as i64;
    let mime_type = args.mime_type.filter(|m| !m.is_empty()).unwrap_or_else(|| "application/octet-stream".to_string());

    // Content-addressed blob store (CLAUDE.md): {data_dir}/blobs/{file_hash}.
    // Write before the DB insert so the ingestion worker never sees a
    // documents row pointing at bytes that aren't on disk yet.
    let blob_dir = app.path().app_data_dir().map_err(|e| e.to_string())?.join("blobs");
    tokio::fs::create_dir_all(&blob_dir).await.map_err(|e| e.to_string())?;
    tokio::fs::write(blob_dir.join(&file_hash), &args.file_contents)
        .await
        .map_err(|e| e.to_string())?;

    let mut tx = state.app_pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, user_id).await.map_err(|e| e.to_string())?;

    // ON CONFLICT rather than a pre-check: two concurrent uploads of the same
    // file must not race each other into the unique index. It covers live
    // documents only (migrations/009), so a file whose document was deleted
    // uploads as a new document. RLS still applies — a non-member's INSERT
    // is rejected by documents_dept_insert before the conflict clause matters.
    let inserted: Option<Uuid> = sqlx::query_scalar(
        r#"
        INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, uploaded_by)
        VALUES ($1, $2, $3, $4, $5, $6)
        ON CONFLICT (department_id, file_hash) WHERE deleted_at IS NULL DO NOTHING
        RETURNING id
        "#,
    )
    .bind(args.department_id)
    .bind(&args.filename)
    .bind(&file_hash)
    .bind(mime_type)
    .bind(byte_size)
    .bind(user_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let doc_id = match inserted {
        Some(id) => id,
        None => {
            // Same bytes already uploaded to this department: hand back the
            // existing document instead of a duplicate (FR15). A failed one
            // gets a fresh job below — re-uploading is the retry path.
            let (id, status): (Uuid, String) = sqlx::query_as(
                "SELECT id, status FROM documents WHERE department_id = $1 AND file_hash = $2 AND deleted_at IS NULL",
            )
            .bind(args.department_id)
            .bind(&file_hash)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

            let latest_job = sqlx::query_as::<_, JobStatus>(
                r#"
                SELECT id AS job_id, document_id, status, attempts, error AS error_text
                FROM ingestion_jobs
                WHERE document_id = $1
                ORDER BY created_at DESC
                LIMIT 1
                "#,
            )
            .bind(id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|e| e.to_string())?;

            // Requeue when either side says it failed: documents stranded in
            // 'pending' behind a failed job exist in databases written by
            // older builds, whose sidecar-unavailable path failed only the job.
            let job_failed = latest_job.as_ref().is_some_and(|j| j.status == "failed");
            if status != "failed" && !job_failed {
                if let Some(job) = latest_job {
                    // Nothing changed, so nothing to audit.
                    tx.commit().await.map_err(|e| e.to_string())?;
                    return Ok(job);
                }
            }

            sqlx::query("UPDATE documents SET status = 'pending', updated_at = now() WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
            id
        }
    };

    let job_id: Uuid = sqlx::query_scalar(
        "INSERT INTO ingestion_jobs (document_id, status) VALUES ($1, 'queued') RETURNING id",
    )
    .bind(doc_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    audit::record(
        &mut tx,
        Some(user_id),
        Some(args.department_id),
        event::DOCUMENT_UPLOADED,
        serde_json::json!({ "document_id": doc_id, "job_id": job_id, "file_hash": file_hash, "byte_size": byte_size, "reupload": inserted.is_none() }),
    )
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    Ok(JobStatus {
        job_id,
        document_id: doc_id,
        status: "queued".into(),
        attempts: 0,
        error_text: None,
    })
}

/// List documents visible to the user (RLS enforces department scope).
///
/// `set_config(..., true)` is transaction-local: on a bare acquired
/// connection with no explicit `BEGIN`, it reverts as soon as that one
/// statement's implicit autocommit transaction ends, so a later query on the
/// same connection would run with the session variable already unset (RLS
/// then fails closed — zero rows). An explicit transaction is required so
/// the setting is still in effect for the SELECT below (CLAUDE.md invariant #2).
#[tauri::command]
pub async fn cmd_list_documents(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
) -> Result<Vec<DocumentInfo>, String> {
    let user_id = session.require()?.id;
    let mut tx = state.app_pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, user_id).await.map_err(|e| e.to_string())?;

    let docs = sqlx::query_as::<_, DocumentInfo>(
        r#"
        SELECT d.id, d.title AS filename, d.status, d.department_id,
               (d.uploaded_by = current_user_id()
                OR EXISTS (SELECT 1 FROM users u WHERE u.id = current_user_id() AND u.is_admin)) AS can_delete
        FROM documents d
        WHERE d.deleted_at IS NULL
        ORDER BY d.created_at DESC
        "#,
    )
    .fetch_all(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(docs)
}

/// Delete a document (ADR-0015): uploader or administrator only.
///
/// Everything that decides and does the deletion is `delete_document()` in
/// the database — the permission check, purging chunks + embeddings,
/// failing any queued job, writing the tombstone. This command supplies the
/// identity (session → `set_current_user`), audits in the same
/// transaction, and afterwards removes the blob file if no other live
/// document shares its bytes. The file goes only after COMMIT: a rolled-back
/// deletion must not leave a document whose bytes are gone.
#[tauri::command]
pub async fn cmd_delete_document(
    app:         AppHandle,
    state:       State<'_, AppState>,
    session:     State<'_, Session>,
    document_id: Uuid,
) -> Result<(), String> {
    let user_id = session.require()?.id;
    let mut tx = state.app_pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, user_id).await.map_err(|e| e.to_string())?;

    let (file_hash, department_id, blob_still_used): (String, Uuid, bool) =
        sqlx::query_as("SELECT file_hash, department_id, blob_still_used FROM delete_document($1)")
            .bind(document_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
                Some(code) if code == "42501" => {
                    "Only the uploader or an administrator can delete this document.".to_string()
                }
                Some(code) if code == "P0002" => "Document not found.".to_string(),
                _ => e.to_string(),
            })?;

    audit::record(
        &mut tx,
        Some(user_id),
        Some(department_id),
        event::DOCUMENT_DELETED,
        serde_json::json!({ "document_id": document_id, "file_hash": file_hash, "blob_removed": !blob_still_used }),
    )
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    if !blob_still_used {
        let blob = app.path().app_data_dir().map_err(|e| e.to_string())?.join("blobs").join(&file_hash);
        match tokio::fs::remove_file(&blob).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            // The document is already deleted; a leftover file is an orphan
            // (ADR-0013's GC), not a reason to report the deletion as failed.
            Err(e) => tracing::warn!("Document {document_id} deleted, but its blob stays: {e}"),
        }
    }
    Ok(())
}

/// Poll ingestion job status (the "202 polling" path from ADR-0010).
///
/// `ingestion_jobs` is RLS-scoped through its parent document's department,
/// so this needs the same transaction + `set_current_user` as
/// `cmd_list_documents` — without it the policy fails closed, the query
/// always returns `None`, and the frontend poller never terminates.
#[tauri::command]
pub async fn cmd_get_job_status(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    job_id:  Uuid,
) -> Result<Option<JobStatus>, String> {
    let user_id = session.require()?.id;
    let mut tx = state.app_pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, user_id).await.map_err(|e| e.to_string())?;

    let job = sqlx::query_as::<_, JobStatus>(
        r#"
        SELECT id AS job_id, document_id, status, attempts, error AS error_text
        FROM ingestion_jobs
        WHERE id = $1
        "#,
    )
    .bind(job_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(job)
}
