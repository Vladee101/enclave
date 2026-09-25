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

// ─── Lexical leg with IDF (ADR-0020) ─────────────────────────────────────────

/// A word found in at least this many of the visible chunks is too common to
/// pick anything out (column headers repeated on every spreadsheet row, words
/// every document shares). Counting stops here, so the cost per query word
/// is bounded however large the corpus grows.
const COMMON_DF: i64 = 1000;

/// Full-text leg ranked by word rarity.
///
/// Postgres `ts_rank_cd` has no IDF: every matched word counts the same. On a
/// 50 000-row spreadsheet the question "какая сумма у договора Д-012345"
/// matched all 50 000 rows on the header words (сумма, договора — df 50 000)
/// and the one row with "-012345" (df 1) ranked 12 501st. So:
///
/// 1. each question word's document frequency is counted over the chunks
///    this user can see (RLS applies — rarity within their departments),
///    capped at COMMON_DF;
/// 2. words below the cap are the selective ones: candidates are the chunks
///    containing any of them, ranked by the sum of their weights
///    ln(1 + COMMON_DF / df) — BM25's IDF shape without term frequency;
///    `ts_rank_cd` over all words breaks ties;
/// 3. a question with no selective word falls back to OR over all its words
///    ranked by `ts_rank_cd`, as before.
///
/// Also why it is fast again: the old leg evaluated the RLS policy on every
/// one of the 50 000 candidates (1.2–1.4 s); now candidates are the few rows
/// holding a rare word. OR rather than plainto_tsquery's AND throughout: an
/// AND over a natural-language question matched nothing (ADR-0006 note).
///
/// `scope`: only these documents (rarity is then counted within them too).
async fn lexical_leg(
    conn: &mut PgConnection,
    query_text: &str,
    limit: i64,
    scope: Option<&[Uuid]>,
) -> Result<Vec<sqlx::postgres::PgRow>> {
    let word_df: Vec<(String, i64)> = sqlx::query_as(
        r#"
        SELECT q.lex,
               (SELECT count(*) FROM (
                    SELECT 1
                    FROM chunks c
                    JOIN documents d ON d.id = c.document_id
                    WHERE c.content_tsv @@ quote_literal(q.lex)::tsquery
                      AND d.status = 'ready'
                      AND d.deleted_at IS NULL
                      AND ($3::uuid[] IS NULL OR c.document_id = ANY ($3::uuid[]))
                    LIMIT $2
                ) s) AS df
        FROM unnest(tsvector_to_array(to_tsvector('english', $1))) AS q(lex)
        "#,
    )
    .bind(query_text)
    .bind(COMMON_DF)
    .bind(scope)
    .fetch_all(&mut *conn)
    .await?;

    let (selective, weights): (Vec<String>, Vec<f64>) = word_df
        .iter()
        .filter(|(_, df)| *df > 0 && *df < COMMON_DF)
        .map(|(lex, df)| (lex.clone(), (1.0 + COMMON_DF as f64 / *df as f64).ln()))
        .unzip();
    let all_words: Vec<String> = word_df.into_iter().map(|(lex, _)| lex).collect();

    if selective.is_empty() {
        return Ok(sqlx::query(
            r#"
            WITH q AS (
                SELECT array_to_string(array(SELECT quote_literal(w) FROM unnest($1::text[]) w), ' | ')::tsquery AS tsq
            )
            SELECT c.id AS chunk_id, c.document_id, d.title AS filename, c.content
            FROM chunks c
            JOIN documents d ON d.id = c.document_id
            CROSS JOIN q
            WHERE c.content_tsv @@ q.tsq
              AND d.status = 'ready'
              AND d.deleted_at IS NULL
              AND ($3::uuid[] IS NULL OR c.document_id = ANY ($3::uuid[]))
            ORDER BY ts_rank_cd(c.content_tsv, q.tsq) DESC
            LIMIT $2
            "#,
        )
        .bind(&all_words)
        .bind(limit)
        .bind(scope)
        .fetch_all(&mut *conn)
        .await?);
    }

    Ok(sqlx::query(
        r#"
        WITH w AS (
            SELECT t.lex, t.idf, quote_literal(t.lex)::tsquery AS tsq
            FROM unnest($1::text[], $2::float8[]) AS t(lex, idf)
        ),
        q AS (
            SELECT array_to_string(array(SELECT quote_literal(x) FROM unnest($1::text[]) x), ' | ')::tsquery AS rare,
                   array_to_string(array(SELECT quote_literal(x) FROM unnest($3::text[]) x), ' | ')::tsquery AS every
        )
        SELECT c.id AS chunk_id, c.document_id, d.title AS filename, c.content
        FROM chunks c
        JOIN documents d ON d.id = c.document_id
        CROSS JOIN q
        WHERE c.content_tsv @@ q.rare
          AND d.status = 'ready'
          AND d.deleted_at IS NULL
          AND ($5::uuid[] IS NULL OR c.document_id = ANY ($5::uuid[]))
        ORDER BY (SELECT sum(w.idf) FROM w WHERE c.content_tsv @@ w.tsq) DESC,
                 ts_rank_cd(c.content_tsv, q.every) DESC
        LIMIT $4
        "#,
    )
    .bind(&selective)
    .bind(&weights)
    .bind(&all_words)
    .bind(limit)
    .bind(scope)
    .fetch_all(&mut *conn)
    .await?)
}

// ─── Dense leg over chosen documents ─────────────────────────────────────────

/// Nearest chunks of the chosen documents.
///
/// A plain HNSW scan collects its nearest candidates across the whole index
/// first (`hnsw.ef_search`, 40) and filters after, so for one document among
/// many it can return nothing. pgvector's iterative scan (0.8+) keeps
/// scanning until enough rows pass the filter; `strict_order` keeps them in
/// true distance order, which RRF ranks by. The planner still chooses: for
/// a small document it goes through `idx_chunks_document_id` and sorts
/// exactly (measured 1.7 ms, 27 chunks among 50 000), for a 50 000-row one
/// through HNSW (6 ms; an exact scan of it took 1.4 s). The setting is
/// transaction-local and this transaction is the question's own.
async fn dense_leg_scoped(
    conn: &mut PgConnection,
    query_embedding: &[f32],
    limit: i64,
    documents: &[Uuid],
) -> Result<Vec<sqlx::postgres::PgRow>> {
    sqlx::query("SET LOCAL hnsw.iterative_scan = strict_order").execute(&mut *conn).await?;
    Ok(sqlx::query(
        r#"
        SELECT c.id AS chunk_id, c.document_id, d.title AS filename, c.content
        FROM chunk_embeddings ce
        JOIN chunks    c ON c.id = ce.chunk_id
        JOIN documents d ON d.id = c.document_id
        WHERE c.document_id = ANY ($3::uuid[])
          AND d.status = 'ready'
          AND d.deleted_at IS NULL
          AND ce.embedding_model_id = (
                SELECT id FROM embedding_models WHERE is_active = true LIMIT 1
              )
        ORDER BY ce.embedding <=> $1::vector
        LIMIT $2
        "#,
    )
    .bind(query_embedding)
    .bind(limit)
    .bind(documents)
    .fetch_all(&mut *conn)
    .await?)
}

// ─── Hybrid retrieval (ADR-0006) ─────────────────────────────────────────────

/// Hybrid retrieval: dense ANN leg + lexical FTS leg, fused via RRF (ADR-0006).
///
/// Takes a live connection (rather than `&PgPool`) so the caller can run this
/// inside the same transaction that set `app.current_user_id` — required for
/// RLS to apply (CLAUDE.md invariant #2); a fresh pool connection would not
/// see that transaction-local session variable.
///
/// `scope`: search only these documents (the ones chosen in the chat
/// panel); None searches everything the user can see. Either way RLS
/// decides what exists — a chosen id from another department finds nothing.
pub async fn retrieve(
    conn: &mut PgConnection,
    query_embedding: &[f32],
    query_text: &str,
    top_k: usize,
    scope: Option<&[Uuid]>,
) -> Result<Vec<RetrievedChunk>> {
    let k = top_k as i64;

    // ── Dense leg (pgvector cosine ANN) ──────────────────────────────────
    let dense_rows = match scope {
        Some(documents) => dense_leg_scoped(conn, query_embedding, k * 2, documents).await?,
        None => sqlx::query(
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
              AND d.deleted_at IS NULL
              -- Only vectors from the model that embedded the query: cosine
              -- between vectors of two different models is meaningless, and
              -- ADR-0007 keeps old-model rows around during re-embedding.
              AND ce.embedding_model_id = (
                    SELECT id FROM embedding_models WHERE is_active = true LIMIT 1
                  )
            ORDER BY ce.embedding <=> $1::vector
            LIMIT $2
            "#
        )
        .bind(query_embedding)
        .bind(k * 2)
        .fetch_all(&mut *conn)
        .await?,
    };

    // ── Lexical leg (tsvector FTS) ────────────────────────────────────────
    let lex_rows = lexical_leg(conn, query_text, k * 2, scope).await?;

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
