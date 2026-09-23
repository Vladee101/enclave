use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::{
    AppState,
    audit::{self, event},
    db::rls::set_current_user,
    session::Session,
    llm::{adapters::adapters_for_user, CompletionRequest, LlmClient},
    retrieval,
};

#[derive(Deserialize)]
pub struct QueryArgs {
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
    pool:    &PgPool,
    llm:     &LlmClient,
    user_id: Uuid,
    args:    &QueryArgs,
) -> Result<(String, Vec<crate::llm::LoraEntry>, Vec<retrieval::RetrievedChunk>), String> {
    let top_k = args.top_k.unwrap_or(5);

    let query_embedding = llm.embed(&args.query).await.map_err(|e| e.to_string())?;

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, user_id).await.map_err(|e| e.to_string())?;

    let chunks = retrieval::retrieve(&mut tx, &query_embedding, &args.query, top_k)
        .await
        .map_err(|e| e.to_string())?;

    let lora = adapters_for_user(&mut tx, llm, user_id)
        .await
        .map_err(|e| e.to_string())?;

    // Audited here, in the retrieval transaction: the record is what this
    // user was actually shown, and it commits together with the reads.
    // Identifiers only — the question text stays out of the log (audit.rs).
    let document_ids: Vec<Uuid> = chunks.iter().map(|c| c.document_id).collect();
    let chunk_ids: Vec<Uuid> = chunks.iter().map(|c| c.chunk_id).collect();
    audit::record(
        &mut tx,
        Some(user_id),
        None,
        event::QUERY,
        serde_json::json!({ "document_ids": document_ids, "chunk_ids": chunk_ids, "top_k": top_k }),
    )
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

const SIDECAR_UNAVAILABLE: &str =
    "llama-server sidecar unavailable; cannot answer queries";

/// `lib.rs` manages `Option<LlmClient>` (None when the sidecar failed to
/// start — NFR7 graceful degradation). Tauri looks state up by exact type,
/// so commands must take `State<'_, Option<LlmClient>>` and unwrap here,
/// the same way `ingest::jobs::tick` does.
fn require_llm(llm: &Option<LlmClient>) -> Result<&LlmClient, String> {
    llm.as_ref().ok_or_else(|| SIDECAR_UNAVAILABLE.to_string())
}

/// Main RAG + LoRA query pipeline (ADR-0004, 0006), non-streaming.
#[tauri::command]
pub async fn cmd_query(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    llm:     State<'_, Option<LlmClient>>,
    args:    QueryArgs,
) -> Result<QueryResult, String> {
    let user_id = session.require()?.id;
    let llm = require_llm(&llm)?;
    let (prompt, lora, chunks) = prepare(&state.app_pool, llm, user_id, &args).await?;

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
    session:    State<'_, Session>,
    llm:        State<'_, Option<LlmClient>>,
    request_id: String,
    args:       QueryArgs,
) -> Result<QueryResult, String> {
    let user_id = session.require()?.id;
    let llm = require_llm(&llm)?;
    let (prompt, lora, chunks) = prepare(&state.app_pool, llm, user_id, &args).await?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_llm_none_is_clear_error() {
        let Err(err) = require_llm(&None) else { panic!("expected an error") };
        assert_eq!(err, SIDECAR_UNAVAILABLE);
    }
}
