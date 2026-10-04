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
pub mod server;
pub mod tls;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

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
pub fn cmd_app_mode(mode: tauri::State<'_, AppMode>) -> AppMode {
    mode.inner().clone()
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
