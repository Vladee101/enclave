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

/// Invariant #6: the embedder's output must match the active
/// `embedding_models.dimension` (and `vector(768)`). Checked before any
/// write so a mismatch fails the job with a clear message.
fn check_dimension(embedding: &[f32], expected: i32) -> Result<()> {
    anyhow::ensure!(
        embedding.len() == expected as usize,
        "Embedding dimension mismatch: got {}, expected {}",
        embedding.len(),
        expected
    );
    Ok(())
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

    // ── Embed every chunk first, with no transaction open ─────────────────
    // CLAUDE.md invariant #4: never hold a DB transaction across an
    // LLM/embedding HTTP call. Embeddings for a whole document live in
    // memory (~2 300 × 768 f32 ≈ 7 MB for a 1 MB text file), then one short
    // transaction writes everything — still all-or-nothing per document.
    let mut embeddings = Vec::with_capacity(chunk_count);
    for content in &chunks {
        let embedding = llm.embed(content, crate::llm::EmbedKind::Document).await?;
        check_dimension(&embedding, model_dimension)?;
        embeddings.push(embedding);
    }

    // ── Insert chunks + embeddings in one short transaction ───────────────
    let mut tx = pool.begin().await?;

    // The document may have been deleted while its chunks were being
    // embedded (ADR-0015). Lock its row — delete_document() takes the same
    // lock — and write nothing if it is gone: purged content must not come
    // back. A deletion after this commit finds the chunks and purges them.
    let deleted: Option<bool> = sqlx::query_scalar(
        "SELECT deleted_at IS NOT NULL FROM documents WHERE id = $1 FOR UPDATE",
    )
    .bind(document_id)
    .fetch_optional(&mut *tx)
    .await?;
    anyhow::ensure!(deleted == Some(false), "document deleted during ingestion; nothing written");

    for (idx, (content, embedding)) in chunks.iter().zip(embeddings).enumerate() {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_dimension_accepts_match_rejects_mismatch() {
        assert!(check_dimension(&vec![0.0; 768], 768).is_ok());
        let err = check_dimension(&vec![0.0; 384], 768).unwrap_err().to_string();
        assert!(err.contains("got 384, expected 768"), "{err}");
    }

    #[test]
    fn split_into_chunks_overlaps_and_covers_text() {
        let text: String = ('a'..='z').cycle().take(1000).collect();
        let chunks = split_into_chunks(&text, 512, 64);
        assert_eq!(chunks.len(), 3); // starts at 0, 448, 896
        assert_eq!(chunks[0].chars().count(), 512);
        assert_eq!(&chunks[0][448..], &chunks[1][..64]);
        assert!(text.ends_with(chunks.last().unwrap().as_str()));
    }

    #[test]
    fn split_into_chunks_handles_multibyte_and_empty() {
        assert!(split_into_chunks("", 512, 64).is_empty());
        let text = "привет".repeat(200); // 1200 chars, 2 bytes each
        let chunks = split_into_chunks(&text, 512, 64);
        assert!(chunks.iter().all(|c| c.chars().count() <= 512));
    }
}
