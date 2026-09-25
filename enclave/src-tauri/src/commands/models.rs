//! First-run model setup (ADR-0024): status, download, import, restart.
//!
//! No session is required: on first run nobody has a profile yet, and the
//! weights are the machine's, not a department's. Nothing here reads the
//! database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tauri::{AppHandle, Manager, State};

use crate::llm::{models, LlmClient};

/// Set while a download runs, so a second click does not start another.
#[derive(Default)]
pub struct ModelDownloads(AtomicBool);

#[tauri::command]
pub fn cmd_models_status(app: AppHandle) -> Result<Vec<models::ModelStatus>, String> {
    models::status(&app).map_err(|e| e.to_string())
}

/// Start downloading the missing models in the background; progress comes
/// as `models-progress` events, the end as `models-done`.
#[tauri::command]
pub fn cmd_download_models(app: AppHandle, downloads: State<'_, ModelDownloads>) -> Result<(), String> {
    if downloads.0.swap(true, Ordering::SeqCst) {
        return Ok(()); // already running
    }
    tauri::async_runtime::spawn(async move {
        models::download_missing(app.clone()).await;
        app.state::<ModelDownloads>().0.store(false, Ordering::SeqCst);
    });
    Ok(())
}

/// Use a model file the user already has.
#[tauri::command]
pub async fn cmd_import_model(app: AppHandle, key: String, path: String) -> Result<(), String> {
    tauri::async_runtime::spawn_blocking(move || models::import(&app, &key, &PathBuf::from(path.trim().trim_matches('"'))))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| format!("{e:#}"))
}

/// Restart to load newly installed models. `AppHandle::restart` does not
/// run the exit hook, so the model servers and the embedded database are
/// stopped here first — the same order as on exit.
///
/// Not in development: there the page comes from the Vite server that
/// `pnpm tauri dev` runs, and a restarted process outlives it — a white
/// window (observed). The frontend shows this message instead.
#[tauri::command]
pub fn cmd_restart_app(app: AppHandle) -> Result<(), String> {
    if cfg!(debug_assertions) {
        return Err("Development build: close Enclave and start it again with `pnpm tauri dev` \
                    (a restarted process would lose the Vite dev server)."
            .into());
    }
    if let Some(llm) = app.try_state::<Option<LlmClient>>() {
        if let Some(llm) = llm.inner() {
            llm.shutdown();
        }
    }
    if let Some(pg) = app.try_state::<Option<crate::db::embedded::EmbeddedPostgres>>() {
        if let Some(pg) = pg.inner() {
            pg.stop();
        }
    }
    app.restart()
}
