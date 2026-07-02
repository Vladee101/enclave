pub mod db;
pub mod ingest;
pub mod llm;
pub mod retrieval;
pub mod commands;

use anyhow::Context;
use sqlx::PgPool;
use tauri::Manager;
use tracing::info;

/// Shared application state registered with `.manage()`.
/// `app_pool`    — connects as the `app_user` role (non-BYPASSRLS); all RLS-
///                enforced user-facing queries go here (ADR-0008).
/// `admin_pool`  — connects as a privileged role; used for provisioning
///                (users, departments, memberships) and running migrations.
/// `ingest_pool` — connects as `ingest_worker` (BYPASSRLS, non-superuser,
///                non-admin); the *only* pool the ingestion worker may use
///                (CLAUDE.md invariant #3, ADR-0009).
pub struct AppState {
    pub app_pool:    PgPool,
    pub admin_pool:  PgPool,
    pub ingest_pool: PgPool,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "enclave=debug,sqlx=warn".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let app_handle = app.handle().clone();

            // Pool construction is async; block here so state is managed
            // before any command can be invoked (ADR-0008 hard invariant).
            let (app_state, llm_client) =
                tauri::async_runtime::block_on(init(&app_handle)).map_err(|e| {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("{e:#}"),
                    )) as Box<dyn std::error::Error>
                })?;

            let ingest_pool = app_state.ingest_pool.clone();
            app.manage(app_state);
            app.manage(llm_client);

            let app2 = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                ingest::jobs::run_job_loop(ingest_pool, app2).await;
            });

            info!("Enclave ready.");
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::auth::cmd_login,
            commands::auth::cmd_logout,
            commands::auth::cmd_list_users,
            commands::auth::cmd_create_user,
            commands::documents::cmd_upload_document,
            commands::documents::cmd_list_documents,
            commands::documents::cmd_get_job_status,
            commands::query::cmd_query,
            commands::query::cmd_query_stream,
            commands::admin::cmd_list_departments,
            commands::admin::cmd_list_my_departments,
            commands::admin::cmd_create_department,
            commands::admin::cmd_list_adapters,
            commands::admin::cmd_add_adapter,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Async init: connect both pools, run migrations, start LLM sidecar.
async fn init(app: &tauri::AppHandle) -> anyhow::Result<(AppState, Option<llm::LlmClient>)> {
    info!("Enclave starting up…");

    let admin_url = std::env::var("ADMIN_DATABASE_URL")
        .context("ADMIN_DATABASE_URL env var must be set")?;
    let app_url = std::env::var("APP_DATABASE_URL")
        .context("APP_DATABASE_URL env var must be set")?;
    let ingest_url = std::env::var("INGEST_DATABASE_URL")
        .context("INGEST_DATABASE_URL env var must be set")?;

    // Migrations run via the privileged role; app_user has no DDL access.
    let admin_pool = db::connect_and_migrate(&admin_url).await?;
    let app_pool   = db::build_pool(&app_url).await?;
    // Connects as ingest_worker (BYPASSRLS, non-superuser) — deliberately
    // never a clone of admin_pool. See migrations/005 and the module doc
    // on AppState above.
    let ingest_pool = db::build_pool(&ingest_url).await?;

    let llm_client = match llm::LlmClient::spawn(app, &admin_pool).await {
        Ok(client) => Some(client),
        Err(e) => {
            tracing::warn!("llama-server sidecar unavailable, continuing without inference: {e:#}");
            None
        }
    };
    Ok((AppState { app_pool, admin_pool, ingest_pool }, llm_client))
}
