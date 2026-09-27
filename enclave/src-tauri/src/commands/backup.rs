//! Backup and restore for administrators (ADR-0026). The work is in
//! `crate::backup`; these wrappers check the caller, pick the paths and
//! report progress as `backup-progress` events.

use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tauri::{AppHandle, Emitter, Manager, State};

use crate::{
    audit::{self, event},
    backup::{self, Manifest, Progress, Staged},
    commands::admin::require_admin,
    db::embedded::{self, EmbeddedPostgres},
    session::Session,
    AppState, DatabaseUrls,
};

/// Set while a backup or a restore runs: one at a time.
#[derive(Default)]
pub struct BackupBusy(AtomicBool);

struct Running<'a>(&'a AtomicBool);

impl<'a> Running<'a> {
    fn start(flag: &'a AtomicBool) -> Result<Self, String> {
        if flag.swap(true, Ordering::SeqCst) {
            return Err("A backup or restore is already running.".into());
        }
        Ok(Self(flag))
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// What the Admin page shows about a backup.
#[derive(Serialize)]
pub struct BackupSummary {
    pub created_at:  chrono::DateTime<chrono::Utc>,
    pub app_version: String,
    pub documents:   i64,
    pub users:       i64,
    pub departments: i64,
    pub files:       usize,
    pub missing:     usize,
    pub bytes:       u64,
}

impl From<&Manifest> for BackupSummary {
    fn from(m: &Manifest) -> Self {
        Self {
            created_at:  m.created_at,
            app_version: m.app_version.clone(),
            documents:   m.counts.documents,
            users:       m.counts.users,
            departments: m.counts.departments,
            files:       m.blobs.len(),
            missing:     m.missing.len(),
            bytes:       m.bytes(),
        }
    }
}

#[derive(Serialize)]
pub struct StagedSummary {
    pub backup:    BackupSummary,
    pub staged_at: chrono::DateTime<chrono::Utc>,
    pub staged_by: String,
}

impl From<&Staged> for StagedSummary {
    fn from(s: &Staged) -> Self {
        Self { backup: (&s.manifest).into(), staged_at: s.staged_at, staged_by: s.staged_by.clone() }
    }
}

fn progress(app: &AppHandle) -> backup::OnProgress {
    let app = app.clone();
    Arc::new(move |p: Progress| {
        let _ = app.emit("backup-progress", p);
    })
}

fn app_data(app: &AppHandle) -> Result<PathBuf, String> {
    app.path().app_data_dir().map_err(|e| e.to_string())
}

fn embedded_pg<'a>(pg: &'a State<'_, Option<EmbeddedPostgres>>) -> Result<&'a EmbeddedPostgres, String> {
    pg.inner().as_ref().ok_or_else(|| {
        "Enclave is using a PostgreSQL server (*_DATABASE_URL), not its own database: \
         restore that server's data with the server's own backup tools."
            .to_string()
    })
}

/// Write a backup to `path` (chosen in the save dialog).
#[tauri::command]
pub async fn cmd_backup_create(
    app:     AppHandle,
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    urls:    State<'_, DatabaseUrls>,
    busy:    State<'_, BackupBusy>,
    path:    String,
) -> Result<BackupSummary, String> {
    let user_id = require_admin(&state, &session).await?;
    let _running = Running::start(&busy.0)?;
    let bin = embedded::tools_dir(&app).map_err(|e| format!("{e:#}"))?;
    let out = PathBuf::from(&path);
    let manifest = backup::create(&state.admin_pool, &bin, &urls.admin, &app_data(&app)?.join("blobs"), &out, progress(&app))
        .await
        .map_err(|e| format!("{e:#}"))?;

    let mut conn = state.admin_pool.acquire().await.map_err(|e| e.to_string())?;
    let file = out.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    audit::record(
        &mut conn,
        Some(user_id),
        None,
        event::BACKUP_CREATED,
        serde_json::json!({
            "file": file,
            "documents": manifest.counts.documents,
            "files": manifest.blobs.len(),
            "missing": manifest.missing.len(),
            "bytes": manifest.bytes(),
        }),
    )
    .await
    .map_err(|e| e.to_string())?;
    Ok((&manifest).into())
}

/// What a backup file holds, for the restore confirmation. Fails for a file
/// that is not a backup this build can restore.
#[tauri::command]
pub async fn cmd_backup_inspect(
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    path:    String,
) -> Result<BackupSummary, String> {
    require_admin(&state, &session).await?;
    let manifest = tauri::async_runtime::spawn_blocking(move || backup::read_manifest(&PathBuf::from(path)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))?;
    Ok((&manifest).into())
}

/// Check the backup and stage it to replace all data at the next start.
#[tauri::command]
pub async fn cmd_backup_restore(
    app:     AppHandle,
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    pg:      State<'_, Option<EmbeddedPostgres>>,
    busy:    State<'_, BackupBusy>,
    path:    String,
) -> Result<StagedSummary, String> {
    require_admin(&state, &session).await?;
    let pg = embedded_pg(&pg)?;
    let _running = Running::start(&busy.0)?;
    let username = session.require()?.username;
    let staged = backup::stage(&pg.cluster(), &app_data(&app)?, &PathBuf::from(path), &username, progress(&app))
        .await
        .map_err(|e| format!("{e:#}"))?;
    Ok((&staged).into())
}

/// The restore waiting for the next start, if any.
#[tauri::command]
pub async fn cmd_backup_staged(
    app:     AppHandle,
    state:   State<'_, AppState>,
    session: State<'_, Session>,
) -> Result<Option<StagedSummary>, String> {
    require_admin(&state, &session).await?;
    let staged = backup::staged(&app_data(&app)?).map_err(|e| format!("{e:#}"))?;
    Ok(staged.as_ref().map(Into::into))
}

/// Keep the current data after all.
#[tauri::command]
pub async fn cmd_backup_cancel_restore(
    app:     AppHandle,
    state:   State<'_, AppState>,
    session: State<'_, Session>,
    pg:      State<'_, Option<EmbeddedPostgres>>,
    busy:    State<'_, BackupBusy>,
) -> Result<(), String> {
    require_admin(&state, &session).await?;
    let pg = embedded_pg(&pg)?;
    let _running = Running::start(&busy.0)?;
    backup::discard_staged(&pg.cluster(), &app_data(&app)?).await.map_err(|e| format!("{e:#}"))
}
