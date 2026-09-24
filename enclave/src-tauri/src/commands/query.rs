use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use tauri::{AppHandle, Emitter, State};
use uuid::Uuid;

use crate::{
    AppState,
    audit::{self, event},
    db::rls::set_current_user,
    session::Session,
    llm::{adapters::adapters_for_user, CompletionRequest, EmbedKind, LlmClient},
    retrieval,
    tables::plan,
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
    pub answer:      String,
    pub sources:     Vec<SourceRef>,
    /// Set when the answer is a calculation over a spreadsheet table
    /// (ADR-0022): what was computed, shown under the answer.
    pub calculation: Option<String>,
}

/// Everything the completion needs, and what to show next to the answer.
pub struct Prepared {
    pub prompt:      String,
    pub lora:        Vec<crate::llm::LoraEntry>,
    pub sources:     Vec<SourceRef>,
    pub calculation: Option<String>,
}

/// Shared prep for both blocking and streaming query commands (and the
/// `retrieve` example, so it asks exactly what the app asks):
///
/// 1. Embed the query — before any transaction (CLAUDE.md invariant #4
///    forbids holding one across an LLM/embedding HTTP call).
/// 2. One transaction with the RLS identity set (invariant #2): hybrid
///    retrieval, the user's LoRA adapters (ADR-0004, 0006), and the
///    spreadsheet tables among the retrieved documents (ADR-0022).
/// 3. No tables → audit and commit in that transaction, grounded prompt.
/// 4. Tables → commit the reads, ask the planner (no transaction open),
///    then a second identity-scoped transaction runs the validated plan
///    and writes the audit record. No plan, or a plan that fails → the
///    same grounded prompt as step 3. A calculation is never guessed at:
///    the fallback is ordinary retrieval, not an approximate number.
pub async fn prepare(
    pool:    &PgPool,
    llm:     &LlmClient,
    user_id: Uuid,
    args:    &QueryArgs,
) -> Result<Prepared, String> {
    let top_k = args.top_k.unwrap_or(5);
    let e = |e: anyhow::Error| e.to_string();
    let db = |e: sqlx::Error| e.to_string();

    let query_embedding = llm.embed(&args.query, EmbedKind::Query).await.map_err(e)?;

    let mut tx = pool.begin().await.map_err(db)?;
    set_current_user(&mut tx, user_id).await.map_err(e)?;

    let chunks = retrieval::retrieve(&mut tx, &query_embedding, &args.query, top_k).await.map_err(e)?;
    let lora = adapters_for_user(&mut tx, llm, user_id).await.map_err(e)?;
    let document_ids: Vec<Uuid> = chunks.iter().map(|c| c.document_id).collect();
    let candidates = plan::load_candidates(&mut tx, &document_ids).await.map_err(e)?;

    let computation = if candidates.is_empty() {
        audit_retrieval(&mut tx, user_id, &chunks, top_k).await.map_err(e)?;
        tx.commit().await.map_err(db)?;
        None
    } else {
        tx.commit().await.map_err(db)?;
        let plan = match ask_planner(llm, &candidates, &args.query).await {
            Ok(plan) => plan,
            Err(err) => {
                tracing::warn!("Table planner gave no usable plan, answering from retrieval: {err:#}");
                None
            }
        };

        let mut tx = pool.begin().await.map_err(db)?;
        set_current_user(&mut tx, user_id).await.map_err(e)?;
        let computation = match plan {
            None => None,
            Some(plan) => {
                // A savepoint: a failed aggregate must not abort the
                // transaction the audit record is written in.
                let mut sp = sqlx::Connection::begin(&mut *tx).await.map_err(db)?;
                match plan::execute(&mut sp, &candidates, plan).await {
                    Ok(c) => {
                        sp.commit().await.map_err(db)?;
                        Some(c)
                    }
                    Err(err) => {
                        tracing::warn!("Table calculation failed, answering from retrieval: {err:#}");
                        sp.rollback().await.map_err(db)?;
                        None
                    }
                }
            }
        };
        match &computation {
            Some(c) => audit::record(
                &mut tx,
                Some(user_id),
                None,
                event::QUERY,
                serde_json::json!({
                    "document_ids": [c.candidate.document_id],
                    "chunk_ids": [],
                    "table_id": c.candidate.table_id,
                    "matched_rows": c.matched_rows,
                    "top_k": top_k,
                }),
            )
            .await
            .map_err(e)?,
            None => audit_retrieval(&mut tx, user_id, &chunks, top_k).await.map_err(e)?,
        }
        tx.commit().await.map_err(db)?;
        computation
    };

    // Transactions are committed — these HTTP calls hold no connection.
    let prepared = match computation {
        Some(c) => {
            let (system, user) = plan::answer_messages(&c, &args.query);
            let description = c.describe();
            Prepared {
                prompt: llm.apply_template(system, &user).await.map_err(e)?,
                lora,
                sources: vec![SourceRef {
                    document_id: c.candidate.document_id,
                    filename:    c.candidate.filename.clone(),
                    excerpt:     description.clone(),
                    score:       1.0,
                }],
                calculation: Some(description),
            }
        }
        None => {
            let (system, user) = grounded_messages(&chunks, &args.query);
            Prepared {
                prompt: llm.apply_template(system, &user).await.map_err(e)?,
                lora,
                sources: into_sources(chunks),
                calculation: None,
            }
        }
    };
    Ok(prepared)
}

/// Audited in the retrieval transaction: the record is what this user was
/// actually shown, and it commits together with the reads. Identifiers
/// only — the question text stays out of the log (audit.rs).
async fn audit_retrieval(
    tx:      &mut sqlx::PgConnection,
    user_id: Uuid,
    chunks:  &[retrieval::RetrievedChunk],
    top_k:   usize,
) -> anyhow::Result<()> {
    let document_ids: Vec<Uuid> = chunks.iter().map(|c| c.document_id).collect();
    let chunk_ids: Vec<Uuid> = chunks.iter().map(|c| c.chunk_id).collect();
    audit::record(
        tx,
        Some(user_id),
        None,
        event::QUERY,
        serde_json::json!({ "document_ids": document_ids, "chunk_ids": chunk_ids, "top_k": top_k }),
    )
    .await
}

/// The planner call: the model's answer is constrained to `plan_schema`
/// (only these tables' columns exist for it), deterministic, without LoRA
/// adapters — department voice has no place in a plan.
async fn ask_planner(llm: &LlmClient, candidates: &[plan::Candidate], question: &str) -> anyhow::Result<Option<plan::Plan>> {
    let (system, user) = plan::planner_messages(candidates, question);
    let prompt = llm.apply_template(system, &user).await?;
    let answer = llm
        .complete(&CompletionRequest {
            prompt,
            n_predict: 400,
            temperature: 0.0,
            json_schema: Some(plan::plan_schema(candidates)),
            ..Default::default()
        })
        .await?;
    tracing::debug!("Table planner answered: {answer}");
    plan::parse_plan(&answer, candidates)
}

/// The system and user messages for a grounded answer. Public so the
/// `retrieve --answer` example asks exactly what the app asks.
///
/// Document text goes in the user turn as quoted material, never in the
/// system turn: an instruction planted in a document stays data.
pub fn grounded_messages(chunks: &[retrieval::RetrievedChunk], question: &str) -> (&'static str, String) {
    let context = chunks
        .iter()
        .enumerate()
        .map(|(i, c)| format!("[Source {}] {}\n{}", i + 1, c.filename, c.content))
        .collect::<Vec<_>>()
        .join("\n\n");
    let system = "You answer questions about the organization's documents. \
                  Use only the sources given in the user's message and cite them as [Source N]. \
                  If the sources do not contain the answer, say so. \
                  Answer once, concisely, in the language of the question.";
    (system, format!("Sources:\n\n{context}\n\nQuestion: {question}"))
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
    let Prepared { prompt, lora, sources, calculation } = prepare(&state.app_pool, llm, user_id, &args).await?;

    let req = CompletionRequest {
        prompt,
        n_predict: 768,
        temperature: 0.3,
        lora,
        stream: false,
        json_schema: None,
    };
    let answer = llm.complete(&req).await.map_err(|e| e.to_string())?;

    Ok(QueryResult { answer, sources, calculation })
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
    let Prepared { prompt, lora, sources, calculation } = prepare(&state.app_pool, llm, user_id, &args).await?;

    let req = CompletionRequest {
        prompt,
        n_predict: 768,
        temperature: 0.3,
        lora,
        stream: true,
        json_schema: None,
    };

    let event_name = format!("llm-token:{request_id}");
    let answer = llm
        .complete_stream(&req, |token| {
            let _ = app.emit(&event_name, StreamToken { token: token.to_string() });
        })
        .await
        .map_err(|e| e.to_string())?;

    Ok(QueryResult { answer, sources, calculation })
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
