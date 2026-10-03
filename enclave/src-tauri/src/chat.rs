//! Chat history (ADR-0030): each user's conversations, until the user
//! deletes them, visible to that user only.
//!
//! Everything runs on app_pool with the user's identity set: RLS keeps a
//! conversation its owner's (`chats`, `chat_messages`, migration 020). An answer quotes documents, so
//! reading one checks — under RLS, now — that every document it rests on is
//! still within the reader's reach; if not, the answer comes back hidden,
//! without its text, sources or calculation. Deleting a document erases
//! such answers in the database as well (trigger in migration 020).
//!
//! Saved after the answer is complete, in one short transaction — never
//! across a model call (invariant #4).

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::{PgConnection, PgPool, Row};
use uuid::Uuid;

use crate::db::rls::set_current_user;
use crate::error::AppError;

/// Longest title kept; a new conversation is titled by its first question.
const TITLE_CHARS: usize = 80;

/// One exchange to save: the question as shown, and the answer.
pub struct Exchange<'a> {
    pub conversation_id: Option<Uuid>,
    pub question:        &'a str,
    /// Documents the question was limited to in the chat panel.
    pub scope:           &'a [Uuid],
    pub answer:          &'a str,
    /// The sources as shown under the answer (serialized `SourceRef`s).
    pub sources:         serde_json::Value,
    pub calculation:     Option<&'a str>,
    /// The answer is a clarification's question, not an answer.
    pub clarification:   bool,
    /// Every document the answer rests on.
    pub rests_on:        &'a [Uuid],
}

/// A conversation's title from its first question: one line, at most
/// TITLE_CHARS characters.
pub fn title_of(question: &str) -> String {
    let line = question.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut title: String = line.chars().take(TITLE_CHARS).collect();
    if line.chars().count() > TITLE_CHARS {
        title.push('…');
    }
    if title.is_empty() {
        title.push('…');
    }
    title
}

/// Save an exchange; returns its conversation (created for the first one).
pub async fn record(pool: &PgPool, user_id: Uuid, ex: &Exchange<'_>) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    set_current_user(&mut tx, user_id).await?;

    let conversation_id = match ex.conversation_id {
        // RLS: someone else's conversation is not found, not written into.
        Some(id) => sqlx::query_scalar("UPDATE chats SET updated_at = now() WHERE id = $1 RETURNING id")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(|| AppError::new("conversation_not_found", "Conversation not found."))?,
        None => sqlx::query_scalar("INSERT INTO chats (user_id, title) VALUES ($1, $2) RETURNING id")
            .bind(user_id)
            .bind(title_of(ex.question))
            .fetch_one(&mut *tx)
            .await?,
    };

    // Each document once: several sources are often chunks of one.
    let mut rests_on = ex.rests_on.to_vec();
    rests_on.sort();
    rests_on.dedup();

    let last: i32 = sqlx::query_scalar("SELECT coalesce(max(position), 0) FROM chat_messages WHERE chat_id = $1")
        .bind(conversation_id)
        .fetch_one(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO chat_messages (chat_id, user_id, position, role, content, document_ids)
         VALUES ($1, $2, $3, 'user', $4, $5)",
    )
    .bind(conversation_id)
    .bind(user_id)
    .bind(last + 1)
    .bind(ex.question)
    .bind(ex.scope)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO chat_messages
             (chat_id, user_id, position, role, content, sources, calculation, clarification, document_ids)
         VALUES ($1, $2, $3, 'assistant', $4, $5, $6, $7, $8)",
    )
    .bind(conversation_id)
    .bind(user_id)
    .bind(last + 2)
    .bind(ex.answer)
    .bind(&ex.sources)
    .bind(ex.calculation)
    .bind(ex.clarification)
    .bind(&rests_on)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(conversation_id)
}

#[derive(Serialize, Debug)]
pub struct ConversationInfo {
    pub id:         Uuid,
    pub title:      String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

/// The user's conversations, most recent first.
pub async fn list(conn: &mut PgConnection) -> Result<Vec<ConversationInfo>> {
    sqlx::query("SELECT id, title, updated_at FROM chats ORDER BY updated_at DESC")
        .fetch_all(conn)
        .await?
        .into_iter()
        .map(|r| Ok(ConversationInfo { id: r.try_get("id")?, title: r.try_get("title")?, updated_at: r.try_get("updated_at")? }))
        .collect()
}

#[derive(Serialize, Debug)]
pub struct ScopeDoc {
    pub id:       Uuid,
    pub filename: String,
}

#[derive(Serialize, Debug)]
pub struct StoredMessage {
    pub role:          String,
    /// None when hidden.
    pub content:       Option<String>,
    /// The answer rests on a document the reader can no longer see, or
    /// one deleted since: shown as such, without its text.
    pub hidden:        bool,
    pub sources:       serde_json::Value,
    pub calculation:   Option<String>,
    pub clarification: bool,
    /// A question's chosen documents, those still visible.
    pub scope:         Vec<ScopeDoc>,
}

/// A conversation's messages in order, each answer checked against what
/// the reader can see now. None if the conversation is not the user's.
pub async fn messages(conn: &mut PgConnection, conversation_id: Uuid) -> Result<Option<Vec<StoredMessage>>> {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM chats WHERE id = $1)")
        .bind(conversation_id)
        .fetch_one(&mut *conn)
        .await?;
    if !exists {
        return Ok(None);
    }
    // `visible`: every document the message names, as RLS lets the reader
    // see them now (deleted ones excluded).
    let rows = sqlx::query(
        r#"
        SELECT m.role, m.content, m.sources, m.calculation, m.clarification,
               coalesce(v.docs, '[]'::jsonb) AS visible,
               -- Distinct: an answer from several chunks of one document
               -- names it once per chunk (saved so before deduplication).
               (SELECT count(DISTINCT x)::int FROM unnest(m.document_ids) x) AS named
        FROM chat_messages m
        LEFT JOIN LATERAL (
            SELECT jsonb_agg(jsonb_build_object('id', d.id, 'filename', d.title)) AS docs
            FROM documents d
            WHERE d.id = ANY(m.document_ids) AND d.deleted_at IS NULL
        ) v ON true
        WHERE m.chat_id = $1
        ORDER BY m.position
        "#,
    )
    .bind(conversation_id)
    .fetch_all(&mut *conn)
    .await
    .context("reading the conversation")?;

    rows.into_iter()
        .map(|r| {
            let role: String = r.try_get("role")?;
            let visible: serde_json::Value = r.try_get("visible")?;
            let visible: Vec<ScopeDoc> = visible
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|d| {
                    Some(ScopeDoc {
                        id:       d.get("id")?.as_str()?.parse().ok()?,
                        filename: d.get("filename")?.as_str()?.to_string(),
                    })
                })
                .collect();
            let named: i32 = r.try_get("named")?;
            let content: Option<String> = r.try_get("content")?;
            let hidden = role == "assistant" && (content.is_none() || (visible.len() as i32) < named);
            Ok(if hidden {
                StoredMessage {
                    role,
                    content: None,
                    hidden: true,
                    sources: serde_json::json!([]),
                    calculation: None,
                    clarification: r.try_get("clarification")?,
                    scope: Vec::new(),
                }
            } else {
                let is_question = role == "user";
                StoredMessage {
                    role,
                    content,
                    hidden: false,
                    sources: r.try_get("sources")?,
                    calculation: r.try_get("calculation")?,
                    clarification: r.try_get("clarification")?,
                    scope: if is_question { visible } else { Vec::new() },
                }
            })
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_the_first_question_on_one_line() {
        assert_eq!(title_of("  Сколько дней\nотпуска?  "), "Сколько дней отпуска?");
        let long = "слово ".repeat(40);
        let t = title_of(&long);
        assert_eq!(t.chars().count(), TITLE_CHARS + 1);
        assert!(t.ends_with('…'));
        assert_eq!(title_of("   "), "…");
    }
}
