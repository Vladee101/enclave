use std::sync::Mutex;

use serde::Serialize;
use uuid::Uuid;

/// The signed-in user, as the core knows it.
///
/// Commands take the caller's identity from here, never from IPC arguments:
/// a `user_id` passed by the webview is only a claim, and RLS (ADR-0008)
/// faithfully enforces whatever identity it is handed. `cmd_login` is the
/// only writer, after verifying the PIN. One session per process — Enclave
/// is a single-user desktop app with one window.
#[derive(Default)]
pub struct Session(Mutex<Option<SessionUser>>);

#[derive(Serialize, Clone, Debug)]
pub struct SessionUser {
    pub id:       Uuid,
    pub username: String,
    /// Display hint for the UI only. Admin-only commands re-check
    /// `users.is_admin` in the database on every call (`require_admin`),
    /// so a demotion takes effect without a re-login.
    pub is_admin: bool,
}

impl Session {
    pub fn set(&self, user: SessionUser) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(user);
    }

    pub fn clear(&self) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    pub fn get(&self) -> Option<SessionUser> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The signed-in user, or the error every user-scoped command returns
    /// when nobody is.
    pub fn require(&self) -> Result<SessionUser, String> {
        self.get().ok_or_else(|| "Not signed in.".to_string())
    }
}
