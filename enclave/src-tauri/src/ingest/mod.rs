use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub mod jobs;

/// Split text content into overlapping chunks.
/// Strategy: fixed-size windows of `chunk_size` characters with
/// `overlap` characters of context carry-over.
pub fn split_into_chunks(text: &str, chunk_size: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut start = 0usize;

    while start < chars.len() {
        let end = (start + chunk_size).min(chars.len());
        let chunk: String = chars[start..end].iter().collect();
        chunks.push(chunk);
        if end == chars.len() {
            break;
        }
        start += chunk_size - overlap;
    }

    chunks
}

/// Ingest a single document: split → embed → insert chunks + embeddings.
///
/// Called by the job runner.  `user_id` must be set on the connection
/// before this is called so RLS permits the writes (ADR-0008).
pub async fn ingest_document(
    pool: &PgPool,
    llm: &crate::llm::LlmClient,
    blob_root: &Path,
    document_id: Uuid,
) -> Result<usize> {
    // ── Fetch document ────────────────────────────────────────────────────
    let doc_row = sqlx::query(
        "SELECT id, department_id, title AS filename, file_hash, mime_type FROM documents WHERE id = $1",
    )
    .bind(document_id)
    .fetch_one(pool)
    .await?;

    let doc_department_id: Uuid = doc_row.try_get("department_id")?;
    let doc_filename: String = doc_row.try_get("filename")?;
    let doc_file_hash: String = doc_row.try_get("file_hash")?;

    // ── Read raw bytes from the content-addressed blob store ──────────────
    // NOTE: this decodes bytes as lossy UTF-8 regardless of mime_type — a
    // deliberate simplification (like the token_count heuristic below).
    // Real format-aware extraction (PDF via pdfium, DOCX via docx-rs, etc.)
    // is future work; today this only produces sensible text for plain-text
    // uploads (.txt/.md).
    let blob_path = blob_root.join(&doc_file_hash);
    let raw_bytes = tokio::fs::read(&blob_path)
        .await
        .with_context(|| format!("Failed to read blob for document '{}' at {}", doc_filename, blob_path.display()))?;
    let raw_text = String::from_utf8_lossy(&raw_bytes).into_owned();

    // ── Chunk ─────────────────────────────────────────────────────────────
    let chunks = split_into_chunks(&raw_text, 512, 64);
    let chunk_count = chunks.len();

    // ── Active embedding model ─────────────────────────────────────────────
    let model_row = sqlx::query("SELECT id, dimension FROM embedding_models WHERE is_active = true LIMIT 1")
        .fetch_optional(pool)
        .await?;

    let Some(model_row) = model_row else {
        anyhow::bail!("No active embedding model configured");
    };

    let model_id: Uuid = model_row.try_get("id")?;
    let model_dimension: i32 = model_row.try_get("dimension")?;

    // ── Insert chunks + embeddings in a transaction ────────────────────────
    let mut tx = pool.begin().await?;

    for (idx, content) in chunks.iter().enumerate() {
        // token_count: chars/4 heuristic (deliberate simplification, same
        // spirit as the lossy-UTF-8 text extraction above — revisit if it
        // bites, per CLAUDE.md's own note on this heuristic). NOT NULL on
        // the live chunks table with no default, so it must be supplied.
        let token_count = ((content.chars().count() / 4).max(1)) as i32;

        // Insert chunk.
        let chunk_id: Uuid = sqlx::query_scalar(
            r#"
            INSERT INTO chunks (document_id, department_id, chunk_index, content, token_count)
            VALUES ($1, $2, $3, $4, $5)
            RETURNING id
            "#
        )
        .bind(document_id)
        .bind(doc_department_id)
        .bind(idx as i32)
        .bind(content)
        .bind(token_count)
        .fetch_one(&mut *tx)
        .await?;

        // Embed.
        let embedding = llm.embed(content).await?;
        anyhow::ensure!(
            embedding.len() == model_dimension as usize,
            "Embedding dimension mismatch: got {}, expected {}",
            embedding.len(),
            model_dimension
        );

        // Insert embedding.
        sqlx::query(
            r#"
            INSERT INTO chunk_embeddings
                (chunk_id, embedding_model_id, department_id, embedding)
            VALUES ($1, $2, $3, $4)
            "#
        )
        .bind(chunk_id)
        .bind(model_id)
        .bind(doc_department_id)
        .bind(embedding)
        .execute(&mut *tx)
        .await?;
    }

    // Mark document ready.
    sqlx::query("UPDATE documents SET status = 'ready', updated_at = now() WHERE id = $1")
        .bind(document_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;
    Ok(chunk_count)
}
