use anyhow::Result;
use sqlx::{PgConnection, Row};
use tracing::warn;
use uuid::Uuid;

use super::{LlmClient, LoraEntry};

/// Resolve the active LoRA adapters for a user's departments and build
/// the `lora: [{id, scale}]` slice that goes into the llama-server
/// completion request (ADR-0003, 0004).
///
/// The `id` sent to the sidecar must be the index it assigned the adapter
/// when loading it at startup (CLAUDE.md invariant #5), resolved here via
/// `LlmClient::adapter_id_for_path` — never a value derived from row order
/// in `department_adapters`, which has no relationship to how many
/// *distinct* adapter files the sidecar actually has loaded (one adapter
/// file can be assigned to several departments, i.e. several rows).
///
/// Takes a live connection (not `&PgPool`) for the same reason as
/// `retrieval::retrieve`: this must run inside the transaction that set
/// `app.current_user_id`, or department_adapters'/department_members' RLS
/// policies see no session identity and fail closed.
///
/// department_adapters is a pure junction (department_id, adapter_id,
/// scale, is_default); the file path and is_active flag live on the
/// separate `adapters` catalog table it references via `adapter_id`, so
/// this joins through it (there is no `adapter_path`/`is_active`/
/// `created_at` directly on department_adapters in the live schema).
pub async fn adapters_for_user(conn: &mut PgConnection, llm: &LlmClient, user_id: Uuid) -> Result<Vec<LoraEntry>> {
    let rows = sqlx::query(
        r#"
        SELECT a.file_path AS adapter_path, da.scale
        FROM department_adapters da
        JOIN adapters a ON a.id = da.adapter_id
        JOIN department_members dm ON dm.department_id = da.department_id
        WHERE dm.user_id   = $1
          AND a.is_active  = true
        ORDER BY a.created_at, da.id
        "#
    )
    .bind(user_id)
    .fetch_all(&mut *conn)
    .await?;

    let mut entries = Vec::with_capacity(rows.len());
    for r in rows {
        let adapter_path: String = r.get("adapter_path");
        let scale: f32 = r.get("scale");
        match llm.adapter_id_for_path(&adapter_path).await {
            Some(id) => entries.push(LoraEntry { id, scale }),
            None => warn!(
                "Adapter '{}' assigned to user but not loaded in the sidecar; skipping.",
                adapter_path
            ),
        }
    }

    Ok(entries)
}
