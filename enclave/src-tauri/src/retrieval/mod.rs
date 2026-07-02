use anyhow::Result;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub mod rrf;

// ─── Result types ────────────────────────────────────────────────────────────

/// A single retrieved passage with its source metadata.
#[derive(Debug, Clone)]
pub struct RetrievedChunk {
    pub chunk_id:    Uuid,
    pub document_id: Uuid,
    pub filename:    String,
    pub content:     String,
    /// Final RRF score (higher = more relevant).
    pub score:       f64,
}

// ─── Hybrid retrieval (ADR-0006) ─────────────────────────────────────────────

/// Hybrid retrieval: dense ANN leg + lexical FTS leg, fused via RRF (ADR-0006).
///
/// Takes a live connection (rather than `&PgPool`) so the caller can run this
/// inside the same transaction that set `app.current_user_id` — required for
/// RLS to apply (CLAUDE.md invariant #2); a fresh pool connection would not
/// see that transaction-local session variable.
pub async fn retrieve(
    conn: &mut PgConnection,
    query_embedding: &[f32],
    query_text: &str,
    top_k: usize,
) -> Result<Vec<RetrievedChunk>> {
    let k = top_k as i64;

    // ── Dense leg (pgvector cosine ANN) ──────────────────────────────────
    let dense_rows = sqlx::query(
        r#"
        SELECT
            c.id        AS chunk_id,
            c.document_id,
            d.title     AS filename,
            c.content
        FROM chunk_embeddings ce
        JOIN chunks    c ON c.id = ce.chunk_id
        JOIN documents d ON d.id = c.document_id
        WHERE d.status = 'ready'
        ORDER BY ce.embedding <=> $1::vector
        LIMIT $2
        "#
    )
    .bind(query_embedding)
    .bind(k * 2)
    .fetch_all(&mut *conn)
    .await?;

    // ── Lexical leg (tsvector FTS) ────────────────────────────────────────
    let lex_rows = sqlx::query(
        r#"
        SELECT
            c.id        AS chunk_id,
            c.document_id,
            d.title     AS filename,
            c.content
        FROM chunks    c
        JOIN documents d ON d.id = c.document_id
        WHERE c.content_tsv @@ plainto_tsquery('english', $1)
          AND d.status = 'ready'
        ORDER BY ts_rank_cd(c.content_tsv, plainto_tsquery('english', $1)) DESC
        LIMIT $2
        "#
    )
    .bind(query_text)
    .bind(k * 2)
    .fetch_all(&mut *conn)
    .await?;

    // ── RRF fusion ────────────────────────────────────────────────────────
    let dense_ids: Vec<Uuid> = dense_rows.iter().map(|r| r.get::<Uuid, &str>("chunk_id")).collect();
    let lex_ids:   Vec<Uuid> = lex_rows.iter().map(|r| r.get::<Uuid, &str>("chunk_id")).collect();

    let fused = rrf::fuse(&dense_ids, &lex_ids, 60.0);

    // Build a lookup map from chunk_id → row data (dense takes priority for content)
    let mut meta: std::collections::HashMap<Uuid, (Uuid, String, String)> = std::collections::HashMap::new();
    for r in &dense_rows {
        let cid: Uuid = r.get("chunk_id");
        let did: Uuid = r.get("document_id");
        let fname: String = r.get("filename");
        let cont: String = r.get("content");
        meta.insert(cid, (did, fname, cont));
    }
    for r in &lex_rows {
        let cid: Uuid = r.get("chunk_id");
        let did: Uuid = r.get("document_id");
        let fname: String = r.get("filename");
        let cont: String = r.get("content");
        meta.entry(cid).or_insert_with(|| (did, fname, cont));
    }

    let results: Vec<RetrievedChunk> = fused
        .into_iter()
        .take(top_k)
        .filter_map(|(chunk_id, score)| {
            meta.get(&chunk_id).map(|(doc_id, filename, content)| RetrievedChunk {
                chunk_id,
                document_id: *doc_id,
                filename: filename.clone(),
                content: content.clone(),
                score,
            })
        })
        .collect();

    Ok(results)
}
