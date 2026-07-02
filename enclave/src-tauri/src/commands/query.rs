use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::{
    AppState,
    db::rls::set_current_user,
    llm::{adapters::adapters_for_user, CompletionRequest, LlmClient},
    retrieval,
};

#[derive(Deserialize)]
pub struct QueryArgs {
    pub user_id: Uuid,
    pub query:   String,
    pub top_k:   Option<usize>,
}

#[derive(Serialize)]
pub struct SourceRef {
    pub document_id: Uuid,
    pub filename:    String,
    pub excerpt:     String,
    pub score:       f64,
}

#[derive(Serialize)]
pub struct QueryResult {
    pub answer:  String,
    pub sources: Vec<SourceRef>,
}

/// Shared prep for both blocking and streaming query commands:
/// embeds the query (before opening a transaction — CLAUDE.md invariant #4
/// forbids holding a DB transaction across an LLM/embedding HTTP call), then
/// sets the RLS session variable and runs hybrid retrieval + adapter
/// resolution *inside that same transaction* (invariant #2 — the session
/// variable is transaction-local, so a query on a different connection, or
/// on this one after the transaction ends, would see no identity and RLS
/// would fail closed), builds the grounded prompt, and resolves the user's
/// LoRA adapters (ADR-0004, 0006).
async fn prepare(
    pool: &PgPool,
    llm:  &LlmClient,
    args: &QueryArgs,
) -> Result<(String, Vec<crate::llm::LoraEntry>, Vec<retrieval::RetrievedChunk>), String> {
    let top_k = args.top_k.unwrap_or(5);

    let query_embedding = llm.embed(&args.query).await.map_err(|e| e.to_string())?;

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, args.user_id).await.map_err(|e| e.to_string())?;

    let chunks = retrieval::retrieve(&mut tx, &query_embedding, &args.query, top_k)
        .await
        .map_err(|e| e.to_string())?;

    let lora = adapters_for_user(&mut tx, llm, args.user_id)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    let context = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| format!("[Source {}] {}\n{}", i + 1, c.filename, c.content))
        .collect::<Vec<_>>()
        .join("\n\n");

    let prompt = format!(
        "You are a helpful assistant. Answer the question using only the provided context.\n\
         If the context does not contain enough information, say so.\n\n\
         Context:\n{context}\n\n\
         Question: {}\n\nAnswer:",
        args.query
    );

    Ok((prompt, lora, chunks))
}

fn into_sources(chunks: Vec<retrieval::RetrievedChunk>) -> Vec<SourceRef> {
    chunks
        .into_iter()
        .map(|c| SourceRef {
            document_id: c.document_id,
            filename:    c.filename,
            excerpt:     c.content.chars().take(200).collect(),
            score:       c.score,
        })
        .collect()
}

/// Main RAG + LoRA query pipeline (ADR-0004, 0006), non-streaming.
#[tauri::command]
pub async fn cmd_query(
    state: State<'_, AppState>,
    llm:   State<'_, LlmClient>,
    args:  QueryArgs,
) -> Result<QueryResult, String> {
    let (prompt, lora, chunks) = prepare(&state.app_pool, &llm, &args).await?;

    let req = CompletionRequest {
        prompt,
        n_predict: 768,
        temperature: 0.3,
        lora,
        stream: false,
    };
    let answer = llm.complete(&req).await.map_err(|e| e.to_string())?;

    Ok(QueryResult { answer, sources: into_sources(chunks) })
}

/// Token payload emitted on `llm-token:<request_id>` as the answer streams in.
#[derive(Serialize, Clone)]
struct StreamToken {
    token: String,
}

/// Same pipeline as `cmd_query`, but streams the completion token-by-token
/// via Tauri events on `llm-token:<request_id>` (ADR-0004, 0006).
#[tauri::command]
pub async fn cmd_query_stream(
    app:        AppHandle,
    state:      State<'_, AppState>,
    llm:        State<'_, LlmClient>,
    request_id: String,
    args:       QueryArgs,
) -> Result<QueryResult, String> {
    let (prompt, lora, chunks) = prepare(&state.app_pool, &llm, &args).await?;

    let req = CompletionRequest {
        prompt,
        n_predict: 768,
        temperature: 0.3,
        lora,
        stream: true,
    };

    let event_name = format!("llm-token:{request_id}");
    let answer = llm
        .complete_stream(&req, |token| {
            let _ = app.emit(&event_name, StreamToken { token: token.to_string() });
        })
        .await
        .map_err(|e| e.to_string())?;

    Ok(QueryResult { answer, sources: into_sources(chunks) })
}
