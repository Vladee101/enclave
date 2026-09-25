pub mod db;
pub mod ingest;
pub mod llm;
pub mod retrieval;
pub mod commands;
pub mod audit;
pub mod session;
pub mod tables;

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
            let (app_state, llm_client, embedded_pg) =
                tauri::async_runtime::block_on(init(&app_handle)).map_err(|e| {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("{e:#}"),
                    )) as Box<dyn std::error::Error>
                })?;

            let ingest_pool = app_state.ingest_pool.clone();
            app.manage(app_state);
            app.manage(llm_client);
            app.manage(embedded_pg);
            app.manage(session::Session::default());
            app.manage(commands::models::ModelDownloads::default());

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
            commands::auth::cmd_current_session,
            commands::documents::cmd_upload_document,
            commands::documents::cmd_list_documents,
            commands::documents::cmd_list_document_tables,
            commands::documents::cmd_get_job_status,
            commands::documents::cmd_delete_document,
            commands::query::cmd_query,
            commands::query::cmd_query_stream,
            commands::admin::cmd_list_departments,
            commands::admin::cmd_list_my_departments,
            commands::admin::cmd_create_department,
            commands::admin::cmd_delete_department,
            commands::admin::cmd_list_memberships,
            commands::admin::cmd_add_member,
            commands::admin::cmd_remove_member,
            commands::admin::cmd_list_audit,
            commands::admin::cmd_list_adapters,
            commands::admin::cmd_add_adapter,
            commands::models::cmd_models_status,
            commands::models::cmd_download_models,
            commands::models::cmd_import_model,
            commands::models::cmd_restart_app,
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // Windows keeps child processes alive after the parent exits; a
            // leftover llama-server would hold its VRAM and port 8080/8081.
            if let tauri::RunEvent::Exit = event {
                if let Some(llm) = app.try_state::<Option<llm::LlmClient>>() {
                    if let Some(llm) = llm.inner() {
                        llm.shutdown();
                    }
                }
                // The app's own PostgreSQL, stopped cleanly so the next
                // start needs no crash recovery (ADR-0014).
                if let Some(pg) = app.try_state::<Option<db::embedded::EmbeddedPostgres>>() {
                    if let Some(pg) = pg.inner() {
                        pg.stop();
                    }
                }
            }
        });
}

/// Async init: connect both pools, run migrations, start LLM sidecar.
type Initialised = (AppState, Option<llm::LlmClient>, Option<db::embedded::EmbeddedPostgres>);

async fn init(app: &tauri::AppHandle) -> anyhow::Result<Initialised> {
    info!("Enclave starting up…");

    // Where the database is (ADR-0014): all three role URLs set — an
    // existing server (development, LAN deployments); none — the app's own
    // embedded PostgreSQL. Some but not all is a mistake, not a mode.
    let vars = ["ADMIN_DATABASE_URL", "APP_DATABASE_URL", "INGEST_DATABASE_URL"].map(|v| std::env::var(v).ok());
    let (embedded, admin_url, app_url, ingest_url) = match vars {
        [Some(admin), Some(app_url), Some(ingest)] => {
            info!("Using the PostgreSQL server named by *_DATABASE_URL.");
            (None, admin, app_url, ingest)
        }
        [None, None, None] => {
            let pg = db::embedded::EmbeddedPostgres::start(app).await?;
            let db::embedded::Urls { admin, app: app_url, ingest } = &pg.urls;
            let urls = (admin.clone(), app_url.clone(), ingest.clone());
            (Some(pg), urls.0, urls.1, urls.2)
        }
        _ => anyhow::bail!(
            "Set all of ADMIN_DATABASE_URL, APP_DATABASE_URL and INGEST_DATABASE_URL to use an existing \
             server, or none of them to use the embedded one"
        ),
    };

    match connect(app, &embedded, &admin_url, &app_url, &ingest_url).await {
        Ok((state, llm)) => Ok((state, llm, embedded)),
        Err(e) => {
            // Setup failed after our server started: do not leave it running.
            if let Some(pg) = &embedded {
                pg.stop();
            }
            Err(e)
        }
    }
}

async fn connect(
    app: &tauri::AppHandle,
    embedded: &Option<db::embedded::EmbeddedPostgres>,
    admin_url: &str,
    app_url: &str,
    ingest_url: &str,
) -> anyhow::Result<(AppState, Option<llm::LlmClient>)> {
    // Migrations run via the privileged role; app_user has no DDL access.
    let admin_pool = db::connect_and_migrate(admin_url).await?;
    if let Some(pg) = embedded {
        // Before the other pools connect: migration 004's fixed password
        // must never be what they log in with.
        pg.secure_roles(&admin_pool).await?;
    }
    let app_pool   = db::build_pool(app_url).await?;
    // Connects as ingest_worker (BYPASSRLS, non-superuser) — deliberately
    // never a clone of admin_pool. See migrations/005 and the module doc
    // on AppState above.
    let ingest_pool = db::build_pool(ingest_url).await?;

    let llm_client = match llm::LlmClient::spawn(app, &admin_pool).await {
        Ok(client) => Some(client),
        Err(e) => {
            tracing::warn!("llama-server sidecar unavailable, continuing without inference: {e:#}");
            None
        }
    };
    Ok((AppState { app_pool, admin_pool, ingest_pool }, llm_client))
}
