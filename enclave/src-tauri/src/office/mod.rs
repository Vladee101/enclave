//! Office mode (ADR-0031): one PC is the server — database, files, worker,
//! models — and the others are clients that hold no database credentials
//! at all. The server's core answers the same commands the window calls,
//! over HTTPS (`server`); a client's core forwards its window's commands
//! there (`client`), pinned to the server's self-signed certificate
//! (`tls`).
//!
//! The mode lives in `{app_data}/office.json`; no file is the single-PC
//! mode, as before.

pub mod client;
pub mod pairing;
pub mod server;
pub mod tls;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tauri::{AppHandle, Manager, State};

use crate::{
    audit::{self, event},
    commands::admin::require_admin,
    error::AppError,
    session::Session,
    Core,
};
use pairing::Invitations;

pub const DEFAULT_PORT: u16 = 47100;
/// Questions the server's model answers at once (`--parallel`).
pub const SERVER_SLOTS: u32 = 3;
const CONFIG: &str = "office.json";

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(tag = "mode", rename_all = "lowercase")]
pub enum Mode {
    /// Everything on this PC (ADR-0014), nothing on the network.
    #[default]
    Single,
    /// Everything on this PC, and the API for clients on `port`.
    Server {
        #[serde(default = "default_port")]
        port: u16,
    },
    /// Only the window: commands go to `server` (`https://host:port`),
    /// whose certificate must have this SHA-256 `fingerprint`.
    Client { server: String, fingerprint: String },
}

fn default_port() -> u16 {
    DEFAULT_PORT
}

pub fn load(app_data: &Path) -> Result<Mode> {
    let path = app_data.join(CONFIG);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Mode::Single),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

pub fn save(app_data: &Path, mode: &Mode) -> Result<()> {
    std::fs::create_dir_all(app_data)?;
    std::fs::write(app_data.join(CONFIG), serde_json::to_vec_pretty(mode)?)?;
    Ok(())
}

/// What a server says about itself before anyone signs in: a client and a
/// server must be the same build, down to the database schema (ADR-0031 —
/// exact match, the server is updated first).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Hello {
    pub version:   String,
    pub migration: i64,
}

impl Hello {
    pub fn this_build() -> Self {
        Self { version: env!("CARGO_PKG_VERSION").to_string(), migration: crate::db::latest_migration() }
    }
}

/// The window's view of the mode, for `cmd_app_mode`.
#[derive(Serialize, Clone, Debug)]
pub struct AppMode {
    pub mode:        &'static str,
    /// The server's address, for a client.
    pub server:      Option<String>,
    /// The server's certificate fingerprint, on the server — what a client
    /// is configured with until pairing by code exists.
    pub fingerprint: Option<String>,
    pub port:        Option<u16>,
}

#[tauri::command]
pub fn cmd_app_mode(mode: State<'_, AppMode>) -> AppMode {
    mode.inner().clone()
}

fn app_data(app: &AppHandle) -> Result<std::path::PathBuf, AppError> {
    app.path().app_data_dir().map_err(AppError::of)
}

fn to_value(e: AppError) -> Value {
    serde_json::to_value(e).unwrap_or_default()
}

/// An invitation as the server's administrator sees it: the code to read
/// out, and where clients find this computer.
#[derive(Serialize)]
pub struct Invitation {
    /// `ABCD-EFGH-IJKL-MNOP`.
    pub code:       String,
    pub expires_in: u64,
    pub port:       u16,
    /// This computer's name and its address on the local network.
    pub addresses:  Vec<String>,
}

/// Make a one-time code for connecting a computer (ADR-0031). Here, on
/// the server's own window, by an administrator — never over the network.
#[tauri::command]
pub async fn cmd_office_invite(
    core:        State<'_, Core>,
    session:     State<'_, Session>,
    invitations: State<'_, Arc<Invitations>>,
    mode:        State<'_, AppMode>,
) -> Result<Invitation, AppError> {
    let admin = require_admin(&core, &session.caller()).await?;
    let Some(port) = mode.port.filter(|_| mode.mode == "server") else {
        return Err(AppError::new("office_not_server", "This computer is not the office server."));
    };
    let code = invitations.create(admin, Instant::now());
    let mut conn = core.admin_pool.acquire().await.map_err(AppError::of)?;
    audit::record(&mut conn, Some(admin), None, event::OFFICE_INVITE_CREATED, serde_json::json!({}))
        .await
        .map_err(AppError::of)?;
    Ok(Invitation {
        code: pairing::display(&code),
        expires_in: pairing::LIFETIME.as_secs(),
        port,
        addresses: local_addresses(),
    })
}

/// Connect this computer to an office server with the administrator's
/// code; it becomes a client at the next start. Anyone at this computer
/// may: it is this computer's choice, and grants nothing on the server.
#[tauri::command]
pub async fn cmd_office_pair(app: AppHandle, mode: State<'_, AppMode>, address: String, code: String) -> Result<String, Value> {
    if mode.mode == "server" {
        return Err(to_value(AppError::new(
            "office_is_server",
            "This computer is the office server; it cannot be a client too.",
        )));
    }
    let (server, fingerprint) = client::pair(&address, &code).await?;
    let dir = app_data(&app).map_err(to_value)?;
    save(&dir, &Mode::Client { server: server.clone(), fingerprint })
        .map_err(|e| to_value(AppError::internal(format!("{e:#}"))))?;
    tracing::info!("Connected to the office server at {server}; a client from the next start.");
    Ok(server)
}

/// Switch this computer's mode, from the next start: `single` or
/// `server`. Becoming or ceasing to be the server is an administrator's
/// decision; a client leaving its server is this computer's own.
#[tauri::command]
pub async fn cmd_office_set_mode(
    app:     AppHandle,
    mode:    State<'_, AppMode>,
    session: State<'_, Session>,
    target:  String,
) -> Result<(), AppError> {
    let next = match target.as_str() {
        "single" => Mode::Single,
        "server" if mode.mode != "client" => Mode::Server { port: DEFAULT_PORT },
        _ => {
            return Err(AppError::new("bad_arguments", format!("Cannot switch to {target} from here.")).with("name", "target"))
        }
    };
    let audited = match app.try_state::<Core>() {
        Some(core) => {
            let admin = require_admin(&core, &session.caller()).await?;
            Some((core.inner().clone(), admin))
        }
        None => None, // a client: no database here, and nothing to guard
    };
    save(&app_data(&app)?, &next).map_err(|e| AppError::internal(format!("{e:#}")))?;
    if let Some((core, admin)) = audited {
        let mut conn = core.admin_pool.acquire().await.map_err(AppError::of)?;
        audit::record(
            &mut conn,
            Some(admin),
            None,
            event::OFFICE_MODE_CHANGED,
            serde_json::json!({ "from": mode.mode, "to": target }),
        )
        .await
        .map_err(AppError::of)?;
    }
    Ok(())
}

/// This computer's name and IPv4 address on the local network, for the
/// administrator to read out. The address is the one the system would
/// route through: found by `connect` on a UDP socket, which sends nothing.
pub fn local_addresses() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(name) = std::env::var("COMPUTERNAME") {
        out.push(name);
    }
    if let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("10.255.255.255:1").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                if !addr.ip().is_loopback() && !addr.ip().is_unspecified() {
                    out.push(addr.ip().to_string());
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_file_is_the_single_pc_mode_and_the_port_has_a_default() {
        let dir = std::env::temp_dir().join(format!("enclave-office-{}", uuid::Uuid::new_v4()));
        assert_eq!(load(&dir).unwrap(), Mode::Single);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(CONFIG), r#"{"mode":"server"}"#).unwrap();
        assert_eq!(load(&dir).unwrap(), Mode::Server { port: DEFAULT_PORT });
        let client = Mode::Client { server: "https://office-pc:47100".into(), fingerprint: "ab".into() };
        save(&dir, &client).unwrap();
        assert_eq!(load(&dir).unwrap(), client);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
