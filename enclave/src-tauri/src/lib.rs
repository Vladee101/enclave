pub mod db;
pub mod ingest;
pub mod llm;
pub mod retrieval;
pub mod commands;
pub mod audit;
pub mod backup;
pub mod chat;
pub mod session;
pub mod error;
pub mod instructions;
pub mod office;
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

/// What the user-facing commands need, in one value both callers can
/// hold: the window's commands (through Tauri state) and the office
/// server (ADR-0031). The core functions under `commands` take it with a
/// `Caller`; no ingestion pool here — only the worker uses that one.
#[derive(Clone)]
pub struct Core {
    pub app_pool:   PgPool,
    pub admin_pool: PgPool,
    pub llm:        Option<llm::LlmClient>,
    /// `{app_data}/blobs` (ADR-0013).
    pub blob_root:  std::path::PathBuf,
    pub logins:     std::sync::Arc<session::LoginGuard>,
}

/// The privileged role's connection string, for `pg_dump` (ADR-0026) —
/// the one thing a pool cannot hand back.
pub struct DatabaseUrls {
    pub admin: String,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // First: a second launch hands over to the running Enclave and exits
        // before its own setup. Two instances on one data directory took
        // each other's database for a crashed one and stopped it — seen on
        // the clean-machine check (docs/clean-machine-check.md).
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // Here, not before the builder: a second instance has exited by
            // now and cannot touch the running one's log file.
            let app_data = app.path().app_data_dir()?;
            init_logging(Some(app_data.clone()));
            let app_handle = app.handle().clone();

            // Office mode (ADR-0031). A client runs nothing of its own —
            // no database, no models, no worker — and forwards every
            // command to the server.
            let mode = office::load(&app_data)?;
            app.manage(session::Session::default());
            if let office::Mode::Client { server, fingerprint } = &mode {
                app.manage(office::client::Remote::new(server, fingerprint)?);
                app.manage(office::AppMode { mode: "client", server: Some(server.clone()), fingerprint: None, port: None });
                info!("Enclave ready as a client of {server}.");
                return Ok(());
            }

            // Pool construction is async; block here so state is managed
            // before any command can be invoked (ADR-0008 hard invariant).
            let slots = if matches!(mode, office::Mode::Server { .. }) { office::SERVER_SLOTS } else { 1 };
            let (app_state, llm_client, embedded_pg, urls) =
                tauri::async_runtime::block_on(init(&app_handle, slots)).map_err(|e| {
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("{e:#}"),
                    )) as Box<dyn std::error::Error>
                })?;

            let ingest_pool = app_state.ingest_pool.clone();
            let core = Core {
                app_pool:   app_state.app_pool.clone(),
                admin_pool: app_state.admin_pool.clone(),
                llm:        llm_client.clone(),
                blob_root:  app.path().app_data_dir()?.join("blobs"),
                logins:     Default::default(),
            };
            let mut app_mode = office::AppMode { mode: "single", server: None, fingerprint: None, port: None };
            if let office::Mode::Server { port } = mode {
                let identity = office::tls::Identity::load_or_create(&app_data.join("office"))?;
                let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
                let running = tauri::async_runtime::block_on(office::server::start(core.clone(), &identity, addr))?;
                app_mode = office::AppMode { mode: "server", server: None, fingerprint: Some(running.fingerprint), port: Some(port) };
            }
            app.manage(app_mode);
            app.manage(core);
            app.manage(app_state);
            app.manage(llm_client);
            app.manage(embedded_pg);
            app.manage(urls);
            app.manage(commands::backup::BackupBusy::default());
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
            commands::admin::cmd_set_department_instructions,
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
            commands::chat::cmd_list_conversations,
            commands::chat::cmd_get_conversation,
            commands::chat::cmd_rename_conversation,
            commands::chat::cmd_delete_conversation,
            commands::backup::cmd_backup_create,
            commands::backup::cmd_backup_inspect,
            commands::backup::cmd_backup_restore,
            commands::backup::cmd_backup_staged,
            commands::backup::cmd_backup_cancel_restore,
            office::cmd_app_mode,
            office::client::cmd_remote_call,
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

/// Log to the console (development) and to `{app_data}/enclave.log` — a
/// release build has no console, so without the file nothing the core
/// reports is ever seen. The previous run's log is kept as
/// `enclave.prev.log`.
fn init_logging(app_data: Option<std::path::PathBuf>) {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "enclave=debug,sqlx=warn".into());
    let file = app_data.and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let log = dir.join("enclave.log");
        let _ = std::fs::rename(&log, dir.join("enclave.prev.log"));
        std::fs::File::create(&log).ok()
    });
    let file_layer = file.map(|f| fmt::layer().with_ansi(false).with_writer(std::sync::Mutex::new(f)));
    let _ = tracing_subscriber::registry().with(filter).with(fmt::layer()).with(file_layer).try_init();
}

/// Async init: connect both pools, run migrations, start LLM sidecar.
type Initialised = (AppState, Option<llm::LlmClient>, Option<db::embedded::EmbeddedPostgres>, DatabaseUrls);

async fn init(app: &tauri::AppHandle, slots: u32) -> anyhow::Result<Initialised> {
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

    match connect(app, &embedded, &admin_url, &app_url, &ingest_url, slots).await {
        Ok((state, llm)) => Ok((state, llm, embedded, DatabaseUrls { admin: admin_url })),
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
    slots: u32,
) -> anyhow::Result<(AppState, Option<llm::LlmClient>)> {
    // Migrations run via the privileged role; app_user has no DDL access.
    let admin_pool = db::connect_and_migrate(admin_url).await?;
    if let Some(pg) = embedded {
        // Before the other pools connect: migration 004's fixed password
        // must never be what they log in with.
        pg.secure_roles(&admin_pool).await?;
        if let Some(staged) = &pg.restored {
            // Recorded in the restored database's own log; the one it
            // replaced is gone.
            let mut conn = admin_pool.acquire().await?;
            audit::record(
                &mut conn,
                None,
                None,
                audit::event::BACKUP_RESTORED,
                serde_json::json!({
                    "backup_created_at": staged.manifest.created_at,
                    "documents": staged.manifest.counts.documents,
                    "staged_by": staged.staged_by,
                }),
            )
            .await?;
        }
    }
    let app_pool   = db::build_pool(app_url).await?;
    // Connects as ingest_worker (BYPASSRLS, non-superuser) — deliberately
    // never a clone of admin_pool. See migrations/005 and the module doc
    // on AppState above.
    let ingest_pool = db::build_pool(ingest_url).await?;

    let llm_client = match llm::LlmClient::spawn(app, &admin_pool, slots).await {
        Ok(client) => Some(client),
        Err(e) => {
            tracing::warn!("llama-server sidecar unavailable, continuing without inference: {e:#}");
            None
        }
    };
    Ok((AppState { app_pool, admin_pool, ingest_pool }, llm_client))
}
