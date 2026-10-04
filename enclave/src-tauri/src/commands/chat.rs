//! Chat history commands (ADR-0030): thin wrappers over `crate::chat`, all
//! on app_pool with the caller's identity — RLS keeps conversations their
//! owner's.

use serde::Deserialize;
use tauri::State;
use uuid::Uuid;

use crate::{
    chat::{self, ConversationInfo, StoredMessage},
    db::rls::set_current_user,
    error::AppError,
    session::{Caller, Session},
    Core,
};

fn not_found() -> AppError {
    AppError::new("conversation_not_found", "Conversation not found.")
}

#[tauri::command]
pub async fn cmd_list_conversations(
    core:    State<'_, Core>,
    session: State<'_, Session>,
) -> Result<Vec<ConversationInfo>, AppError> {
    list_conversations(&core, &session.caller()).await
}

pub async fn list_conversations(core: &Core, caller: &Caller) -> Result<Vec<ConversationInfo>, AppError> {
    let user_id = caller.require()?.id;
    let mut tx = core.app_pool.begin().await.map_err(AppError::of)?;
    set_current_user(&mut tx, user_id).await.map_err(AppError::of)?;
    let list = chat::list(&mut tx).await.map_err(AppError::of)?;
    tx.commit().await.map_err(AppError::of)?;
    Ok(list)
}

#[tauri::command]
pub async fn cmd_get_conversation(
    core:    State<'_, Core>,
    session: State<'_, Session>,
    conversation_id: Uuid,
) -> Result<Vec<StoredMessage>, AppError> {
    get_conversation(&core, &session.caller(), conversation_id).await
}

pub async fn get_conversation(core: &Core, caller: &Caller, conversation_id: Uuid) -> Result<Vec<StoredMessage>, AppError> {
    let user_id = caller.require()?.id;
    let mut tx = core.app_pool.begin().await.map_err(AppError::of)?;
    set_current_user(&mut tx, user_id).await.map_err(AppError::of)?;
    let messages = chat::messages(&mut tx, conversation_id).await.map_err(|e| AppError::from_anyhow(&e))?;
    tx.commit().await.map_err(AppError::of)?;
    messages.ok_or_else(not_found)
}

#[derive(Deserialize)]
pub struct RenameArgs {
    pub conversation_id: Uuid,
    pub title:           String,
}

#[tauri::command]
pub async fn cmd_rename_conversation(
    core:    State<'_, Core>,
    session: State<'_, Session>,
    args: RenameArgs,
) -> Result<(), AppError> {
    rename_conversation(&core, &session.caller(), args).await
}

pub async fn rename_conversation(core: &Core, caller: &Caller, args: RenameArgs) -> Result<(), AppError> {
    let user_id = caller.require()?.id;
    let title = chat::title_of(&args.title);
    let mut tx = core.app_pool.begin().await.map_err(AppError::of)?;
    set_current_user(&mut tx, user_id).await.map_err(AppError::of)?;
    let done = sqlx::query("UPDATE chats SET title = $2 WHERE id = $1")
        .bind(args.conversation_id)
        .bind(title)
        .execute(&mut *tx)
        .await
        .map_err(AppError::of)?;
    if done.rows_affected() == 0 {
        return Err(not_found());
    }
    tx.commit().await.map_err(AppError::of)?;
    Ok(())
}

/// Delete a conversation with its messages (they cascade).
#[tauri::command]
pub async fn cmd_delete_conversation(
    core:    State<'_, Core>,
    session: State<'_, Session>,
    conversation_id: Uuid,
) -> Result<(), AppError> {
    delete_conversation(&core, &session.caller(), conversation_id).await
}

pub async fn delete_conversation(core: &Core, caller: &Caller, conversation_id: Uuid) -> Result<(), AppError> {
    let user_id = caller.require()?.id;
    let mut tx = core.app_pool.begin().await.map_err(AppError::of)?;
    set_current_user(&mut tx, user_id).await.map_err(AppError::of)?;
    let done = sqlx::query("DELETE FROM chats WHERE id = $1")
        .bind(conversation_id)
        .execute(&mut *tx)
        .await
        .map_err(AppError::of)?;
    if done.rows_affected() == 0 {
        return Err(not_found());
    }
    tx.commit().await.map_err(AppError::of)?;
    Ok(())
}
