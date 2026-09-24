use anyhow::{Context, Result};
use sqlx::{PgPool, Row};
use std::path::Path;
use uuid::Uuid;

pub mod extract;
pub mod jobs;

/// Chunks per embedding request. Measured with the app's embedding server
/// settings: 1 → 49 chunks/s, 16 → 138, 32 → 148, 64 → no better.
const EMBED_BATCH: usize = 32;

/// Chunks per INSERT statement in the write transaction.
const WRITE_BATCH: usize = 1000;

/// A document bigger than this is refused instead of occupying the worker
/// for most of an hour. At the measured ~150 chunks/s that is ~11 minutes of
/// embedding; a spreadsheet makes about one chunk per row, so ~100 000 rows.
/// Embeddings are held in memory until the single write transaction
/// (invariant #4): 100 000 × 768 × 4 bytes ≈ 300 MB at the limit.
const MAX_CHUNKS_PER_DOCUMENT: usize = 100_000;

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

/// Chunks for one-record-per-line text (spreadsheet rows, `Layout::Rows`):
/// whole lines packed up to `max_chars`, so a row is never cut between two
/// chunks — each carries its sheet, row number and headers, and a half row
/// would lose them. No overlap: rows are self-contained. A single line
/// longer than `max_chars` (a very wide row) falls back to fixed windows.
pub fn split_rows(text: &str, max_chars: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut current_len = 0usize;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let len = line.chars().count();
        if len > max_chars {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
                current_len = 0;
            }
            chunks.extend(split_into_chunks(line, max_chars, 64));
            continue;
        }
        let needed = if current.is_empty() { len } else { current_len + 1 + len };
        if needed > max_chars {
            chunks.push(std::mem::take(&mut current));
            current_len = 0;
        }
        if !current.is_empty() {
            current.push('\n');
            current_len += 1;
        }
        current.push_str(line);
        current_len += len;
    }
    if !current.is_empty() {
        chunks.push(current);
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
    let blob_path = blob_root.join(&doc_file_hash);
    let raw_bytes = tokio::fs::read(&blob_path)
        .await
        .with_context(|| format!("Failed to read blob for document '{}' at {}", doc_filename, blob_path.display()))?;

    // ── Extract text (PDF / DOCX / spreadsheets / TXT / MD, ADR-0019/0020) ─
    // CPU-bound — parsing a large PDF or workbook takes seconds — so it runs off the
    // async runtime. Its errors are permanent (not reqwest errors), so the
    // job fails at once instead of being retried (jobs.rs).
    let name = doc_filename.clone();
    let (extracted, tables) = tokio::task::spawn_blocking(move || {
        let extracted = extract::extract(&raw_bytes, &name)?;
        // Spreadsheets also become typed tables for calculations (ADR-0022).
        let tables: Vec<crate::tables::TableData> = extracted.sheets.iter().map(crate::tables::build_table).collect();
        anyhow::Ok((extracted, tables))
    })
    .await
    .context("text extraction task failed")??;

    // ── Chunk ─────────────────────────────────────────────────────────────
    let chunks = match extracted.layout {
        extract::Layout::Prose => split_into_chunks(&extracted.text, 512, 64),
        extract::Layout::Rows => split_rows(&extracted.text, 512),
    };
    let chunk_count = chunks.len();
    anyhow::ensure!(
        chunk_count <= MAX_CHUNKS_PER_DOCUMENT,
        "{doc_filename} is too large to index: {chunk_count} chunks, the limit is {MAX_CHUNKS_PER_DOCUMENT} \
         (a spreadsheet makes one chunk per row or two). Split it into several files."
    );

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
    for batch in chunks.chunks(EMBED_BATCH) {
        for embedding in llm.embed_batch(batch, crate::llm::EmbedKind::Document).await? {
            check_dimension(&embedding, model_dimension)?;
            embeddings.push(embedding);
        }
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

    // Bulk writes, WRITE_BATCH rows per statement. Row-by-row this was
    // 2 statements per chunk: a 50 000-row spreadsheet spent ~11 minutes
    // here, ~7 of them on round trips (the rest is HNSW index maintenance,
    // measured at ~3.7 ms per vector — inherent while the index is shared).
    for (batch_no, batch) in chunks.chunks(WRITE_BATCH).enumerate() {
        let first = batch_no * WRITE_BATCH;
        let indexes: Vec<i32> = (first..first + batch.len()).map(|i| i as i32).collect();
        // token_count: chars/4 heuristic (deliberate simplification — revisit
        // if it bites, per CLAUDE.md's own note on this heuristic). NOT NULL
        // on the live chunks table with no default, so it must be supplied.
        let token_counts: Vec<i32> = batch.iter().map(|c| ((c.chars().count() / 4).max(1)) as i32).collect();

        let inserted: Vec<(Uuid, i32)> = sqlx::query_as(
            r#"
            INSERT INTO chunks (document_id, department_id, chunk_index, content, token_count)
            SELECT $1, $2, t.idx, t.content, t.tokens
            FROM UNNEST($3::int[], $4::text[], $5::int[]) AS t(idx, content, tokens)
            RETURNING id, chunk_index
            "#,
        )
        .bind(document_id)
        .bind(doc_department_id)
        .bind(&indexes)
        .bind(batch)
        .bind(&token_counts)
        .fetch_all(&mut *tx)
        .await?;

        // RETURNING order is not guaranteed; line ids up by chunk_index.
        let mut ids = vec![Uuid::nil(); batch.len()];
        for (id, idx) in inserted {
            ids[idx as usize - first] = id;
        }
        anyhow::ensure!(!ids.contains(&Uuid::nil()), "chunk insert returned fewer rows than it was given");

        // All of the batch's vectors as one flat real[], sliced back into
        // vector(dim) per row in SQL — one parameter instead of a 2-D array,
        // which sqlx does not encode.
        let dim = model_dimension as usize;
        let flat: Vec<f32> = embeddings[first..first + batch.len()].iter().flatten().copied().collect();
        sqlx::query(
            r#"
            INSERT INTO chunk_embeddings (chunk_id, embedding_model_id, department_id, embedding)
            SELECT ($1::uuid[])[i], $2, $3, (($4::real[])[(i - 1) * $5 + 1 : i * $5])::vector
            FROM generate_subscripts($1::uuid[], 1) AS i
            "#,
        )
        .bind(&ids)
        .bind(model_id)
        .bind(doc_department_id)
        .bind(&flat)
        .bind(dim as i32)
        .execute(&mut *tx)
        .await?;
    }

    // The same sheets as typed tables — same transaction, so the document
    // is ready with its chunks and its tables or with neither.
    crate::tables::write_tables(&mut tx, document_id, doc_department_id, &tables).await?;

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
    fn split_rows_never_cuts_a_row() {
        let row = |n: usize| format!("[лист «Л», строка {n}] Имя: Сотрудник {n}; Оклад: {}", n * 1000);
        let text: String = (1..=200).map(row).collect::<Vec<_>>().join("\n");
        let chunks = split_rows(&text, 512);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| c.chars().count() <= 512));
        // Every row appears exactly once, whole, in order.
        let rejoined: Vec<&str> = chunks.iter().flat_map(|c| c.lines()).collect();
        assert_eq!(rejoined, text.lines().collect::<Vec<_>>());
    }

    #[test]
    fn split_rows_falls_back_to_windows_for_a_huge_row() {
        let wide = format!("[лист «Л», строка 1] {}", "Колонка: значение; ".repeat(60));
        let text = format!("short row\n{wide}\nanother short row");
        let chunks = split_rows(&text, 512);
        assert_eq!(chunks.first().map(String::as_str), Some("short row"));
        assert_eq!(chunks.last().map(String::as_str), Some("another short row"));
        assert!(chunks.iter().all(|c| c.chars().count() <= 512));
    }

    #[test]
    fn split_into_chunks_handles_multibyte_and_empty() {
        assert!(split_into_chunks("", 512, 64).is_empty());
        let text = "привет".repeat(200); // 1200 chars, 2 bytes each
        let chunks = split_into_chunks(&text, 512, 64);
        assert!(chunks.iter().all(|c| c.chars().count() <= 512));
    }
}
