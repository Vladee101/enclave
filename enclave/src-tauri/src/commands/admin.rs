use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::FromRow;
use tauri::State;
use uuid::Uuid;

use crate::{
    AppState,
    audit::{self, event},
    db::rls::set_current_user,
    session::Session,
};

#[derive(Serialize, FromRow, Debug)]
pub struct DepartmentInfo {
    pub id:   Uuid,
    pub name: String,
}

/// A department's assignment of one adapter (`department_adapters` joined to
/// the `adapters` catalog it references via `adapter_id`). There's no
/// `description` column anywhere in the live schema for a department
/// assignment, so unlike an earlier version of this struct, this one
/// doesn't invent a place to store it.
#[derive(Serialize, FromRow, Debug)]
pub struct AdapterInfo {
    pub id:            Uuid,
    pub department_id: Uuid,
    pub adapter_path:  String,
    pub scale:         f32,
    pub is_active:     bool,
}

/// Turn an adapter file path into a short, human-readable, unique name for
/// the `adapters.name` UNIQUE column. Suffixed with a fragment of the
/// content hash rather than a random id, so re-registering the exact same
/// file (same hash) deterministically reuses the same name instead of
/// growing a new one every time.
fn adapter_name_from_path(path: &str, file_hash: &str) -> String {
    let stem = path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(path)
        .trim_end_matches(".gguf");
    let suffix = &file_hash[..file_hash.len().min(8)];
    format!("{stem}-{suffix}")
}

/// Turn a department name into a URL/DB-safe, unique slug.
fn slugify(name: &str) -> String {
    let mut collapsed = String::new();
    let mut last_dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            collapsed.push(c);
            last_dash = false;
        } else if !last_dash {
            collapsed.push('-');
            last_dash = true;
        }
    }
    let base = collapsed.trim_matches('-');
    let suffix: String = Uuid::new_v4().simple().to_string().chars().take(8).collect();
    format!("{base}-{suffix}")
}

/// Reject unless the signed-in user has `users.is_admin = true`; returns
/// their id. Admin-only commands run on `admin_pool`, which bypasses RLS
/// entirely — this check is the *only* gate on them, so every admin-only
/// command must call it before doing anything else. The flag is read from
/// the database on every call, not from the session, so a demotion applies
/// immediately.
async fn require_admin(state: &State<'_, AppState>, session: &State<'_, Session>) -> Result<Uuid, String> {
    let user_id = session.require()?.id;
    let is_admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(&state.admin_pool)
        .await
        .map_err(|e| e.to_string())?;

    match is_admin {
        Some(true) => Ok(user_id),
        _ => Err("Admin privileges required.".to_string()),
    }
}

/// List every department in the org. Admin-only — see `cmd_list_my_departments`
/// for the RLS-scoped listing ordinary users (e.g. the upload picker) should use.
#[tauri::command]
pub async fn cmd_list_departments(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
) -> Result<Vec<DepartmentInfo>, String> {
    require_admin(&state, &session).await?;
    sqlx::query_as::<_, DepartmentInfo>("SELECT id, name FROM departments ORDER BY name")
        .fetch_all(&state.admin_pool)
        .await
        .map_err(|e| e.to_string())
}

/// List only the departments the calling user belongs to (RLS-enforced via
/// app_pool + migrations/005's `departments_member_select` policy). This is
/// what the Documents-page upload picker uses, so a user can no longer see
/// or target departments they aren't a member of.
#[tauri::command]
pub async fn cmd_list_my_departments(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
) -> Result<Vec<DepartmentInfo>, String> {
    let user_id = session.require()?.id;

    // Must be one explicit transaction: set_config(..., true) is
    // transaction-local, so setting it on a bare acquired connection with no
    // BEGIN reverts before the SELECT below ever runs (CLAUDE.md invariant #2).
    let mut tx = state.app_pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, user_id).await.map_err(|e| e.to_string())?;

    let depts = sqlx::query_as::<_, DepartmentInfo>("SELECT id, name FROM departments ORDER BY name")
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(depts)
}

/// Create a new org-wide department. Admin-only.
#[derive(Deserialize)]
pub struct CreateDeptArgs {
    pub name: String,
}

#[tauri::command]
pub async fn cmd_create_department(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    args:    CreateDeptArgs,
) -> Result<DepartmentInfo, String> {
    let admin_id = require_admin(&state, &session).await?;

    let mut tx = state.admin_pool.begin().await.map_err(|e| e.to_string())?;
    let dept = sqlx::query_as::<_, DepartmentInfo>(
        "INSERT INTO departments (name, slug) VALUES ($1, $2) RETURNING id, name",
    )
    .bind(&args.name)
    .bind(slugify(&args.name))
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    audit::record(&mut tx, Some(admin_id), Some(dept.id), event::DEPARTMENT_CREATED, serde_json::json!({ "name": dept.name }))
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(dept)
}

/// One user's membership in one department, with display names for the
/// Admin page table.
#[derive(Serialize, FromRow, Debug)]
pub struct MembershipInfo {
    pub user_id:         Uuid,
    pub username:        String,
    pub department_id:   Uuid,
    pub department_name: String,
}

/// List every membership in the org. Admin-only.
#[tauri::command]
pub async fn cmd_list_memberships(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
) -> Result<Vec<MembershipInfo>, String> {
    require_admin(&state, &session).await?;
    sqlx::query_as::<_, MembershipInfo>(
        r#"
        SELECT u.id AS user_id, u.username, d.id AS department_id, d.name AS department_name
        FROM department_members dm
        JOIN users       u ON u.id = dm.user_id
        JOIN departments d ON d.id = dm.department_id
        ORDER BY d.name, u.username
        "#,
    )
    .fetch_all(&state.admin_pool)
    .await
    .map_err(|e| e.to_string())
}

/// Add or remove one user's membership in one department. Admin-only.
///
/// Membership is the single input every RLS policy keys off
/// (`is_member_of()` reads `department_members`), so a change here takes
/// effect on the member's very next transaction — no re-login, no cache.
#[derive(Deserialize)]
pub struct MembershipArgs {
    pub user_id:       Uuid,
    pub department_id: Uuid,
}

#[tauri::command]
pub async fn cmd_add_member(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    args:    MembershipArgs,
) -> Result<(), String> {
    let admin_id = require_admin(&state, &session).await?;

    let mut tx = state.admin_pool.begin().await.map_err(|e| e.to_string())?;
    let added = sqlx::query(
        r#"
        INSERT INTO department_members (user_id, department_id)
        VALUES ($1, $2)
        ON CONFLICT DO NOTHING
        "#,
    )
    .bind(args.user_id)
    .bind(args.department_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?
    .rows_affected();

    // Only a real change is an event; re-adding an existing member is a no-op.
    if added > 0 {
        audit::record(&mut tx, Some(admin_id), Some(args.department_id), event::MEMBER_ADDED, serde_json::json!({ "user_id": args.user_id }))
            .await
            .map_err(|e| e.to_string())?;
    }

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub async fn cmd_remove_member(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    args:    MembershipArgs,
) -> Result<(), String> {
    let admin_id = require_admin(&state, &session).await?;

    let mut tx = state.admin_pool.begin().await.map_err(|e| e.to_string())?;
    let removed = sqlx::query("DELETE FROM department_members WHERE user_id = $1 AND department_id = $2")
        .bind(args.user_id)
        .bind(args.department_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?
        .rows_affected();

    if removed > 0 {
        audit::record(&mut tx, Some(admin_id), Some(args.department_id), event::MEMBER_REMOVED, serde_json::json!({ "user_id": args.user_id }))
            .await
            .map_err(|e| e.to_string())?;
    }

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// List all LoRA adapter assignments. Admin-only, since it lists them
/// org-wide (not scoped to the caller's departments).
#[tauri::command]
pub async fn cmd_list_adapters(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
) -> Result<Vec<AdapterInfo>, String> {
    require_admin(&state, &session).await?;
    sqlx::query_as::<_, AdapterInfo>(
        r#"
        SELECT da.id, da.department_id, a.file_path AS adapter_path, da.scale, a.is_active
        FROM department_adapters da
        JOIN adapters a ON a.id = da.adapter_id
        ORDER BY a.created_at, da.id
        "#,
    )
    .fetch_all(&state.admin_pool)
    .await
    .map_err(|e| e.to_string())
}

/// Register (or reuse) a LoRA adapter file and assign it to a department.
/// Admin-only.
///
/// The live schema splits this into a shared `adapters` catalog (one row
/// per distinct adapter *file*) and `department_adapters`, a pure
/// department↔adapter junction — the same file can be assigned to several
/// departments without duplicating the `adapters` row (ADR-0004's point).
/// This command upserts both in one transaction: the `adapters` row is
/// keyed by content hash, so assigning the same file to a second department
/// reuses the existing row instead of erroring on the UNIQUE file_hash
/// constraint.
///
/// `base_model`/`rank`/`alpha` aren't collected from the Admin UI (it only
/// asks for a path + scale) and are metadata fixed at LoRA training time
/// that can't be inferred from the file path, so these are placeholder
/// defaults until the UI collects them for real.
///
/// Note: llama-server only preloads adapters it was launched with
/// (`--lora <path>`, read via `LlmClient::spawn` at sidecar startup). A
/// newly-added adapter won't be usable until the sidecar restarts.
#[derive(Deserialize)]
pub struct AddAdapterArgs {
    pub department_id: Uuid,
    pub adapter_path:  String,
    pub scale:         f32,
}

#[tauri::command]
pub async fn cmd_add_adapter(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    args:    AddAdapterArgs,
) -> Result<AdapterInfo, String> {
    let admin_id = require_admin(&state, &session).await?;

    // Content-addressed like documents: hash the actual file if it's
    // already on disk. An admin may register an adapter assignment before
    // the .gguf file is placed in binaries/adapters/, so fall back to
    // hashing the path itself rather than failing the whole registration.
    let file_hash = match tokio::fs::read(&args.adapter_path).await {
        Ok(bytes) => format!("{:x}", Sha256::digest(&bytes)),
        Err(_) => format!("path:{:x}", Sha256::digest(args.adapter_path.as_bytes())),
    };
    let name = adapter_name_from_path(&args.adapter_path, &file_hash);

    let mut tx = state.admin_pool.begin().await.map_err(|e| e.to_string())?;

    let adapter_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO adapters (name, file_path, base_model, rank, alpha, file_hash)
        VALUES ($1, $2, 'base', 8, 16, $3)
        ON CONFLICT (file_hash) DO UPDATE SET file_path = EXCLUDED.file_path
        RETURNING id
        "#,
    )
    .bind(&name)
    .bind(&args.adapter_path)
    .bind(&file_hash)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let da_id: Uuid = sqlx::query_scalar(
        r#"
        INSERT INTO department_adapters (department_id, adapter_id, scale)
        VALUES ($1, $2, $3)
        ON CONFLICT (department_id, adapter_id) DO UPDATE SET scale = EXCLUDED.scale
        RETURNING id
        "#,
    )
    .bind(args.department_id)
    .bind(adapter_id)
    .bind(args.scale)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let info = sqlx::query_as::<_, AdapterInfo>(
        r#"
        SELECT da.id, da.department_id, a.file_path AS adapter_path, da.scale, a.is_active
        FROM department_adapters da
        JOIN adapters a ON a.id = da.adapter_id
        WHERE da.id = $1
        "#,
    )
    .bind(da_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    audit::record(
        &mut tx,
        Some(admin_id),
        Some(args.department_id),
        event::ADAPTER_ASSIGNED,
        serde_json::json!({ "adapter_id": adapter_id, "adapter_path": info.adapter_path, "scale": info.scale }),
    )
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(info)
}

/// One `audit_log` row for display.
#[derive(Serialize, FromRow, Debug)]
pub struct AuditEntry {
    pub id:              Uuid,
    pub created_at:      chrono::DateTime<chrono::Utc>,
    pub username:        Option<String>,
    pub department_name: Option<String>,
    pub event_type:      String,
    pub payload:         Option<serde_json::Value>,
}

/// The audit journal, newest first (UC10). An admin sees every row via
/// admin_pool; anyone else gets only their own rows, through app_pool and
/// the `audit_log_own_select` policy — the same RLS path as everything
/// else a non-admin reads, not a WHERE clause in this function.
#[tauri::command]
pub async fn cmd_list_audit(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    limit:   Option<i64>,
) -> Result<Vec<AuditEntry>, String> {
    let user_id = session.require()?.id;
    let limit = limit.unwrap_or(200).clamp(1, 1000);

    // LEFT JOINs: on the RLS path a department the user has since left
    // comes back as NULL instead of leaking its name.
    let sql = r#"
        SELECT a.id, a.created_at, u.username, d.name AS department_name, a.event_type, a.payload
        FROM audit_log a
        LEFT JOIN users       u ON u.id = a.user_id
        LEFT JOIN departments d ON d.id = a.department_id
        ORDER BY a.created_at DESC
        LIMIT $1
    "#;

    if require_admin(&state, &session).await.is_ok() {
        return sqlx::query_as::<_, AuditEntry>(sql)
            .bind(limit)
            .fetch_all(&state.admin_pool)
            .await
            .map_err(|e| e.to_string());
    }

    let mut tx = state.app_pool.begin().await.map_err(|e| e.to_string())?;
    set_current_user(&mut tx, user_id).await.map_err(|e| e.to_string())?;
    let rows = sqlx::query_as::<_, AuditEntry>(sql)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(rows)
}
