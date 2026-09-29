//! Errors a command returns to the UI.
//!
//! The UI shows errors in the user's language, so an error the user can
//! meet in normal use carries a stable `code` and its `params`; the
//! frontend looks the code up in its dictionary (`errors.<code>`). The
//! English `message` goes along for the log and as the fallback when the
//! frontend has no text for the code — which is the case for `internal`:
//! a database or I/O failure, shown as it is, not dressed up.
//!
//! Codes are part of the IPC contract: rename one only together with both
//! dictionaries.

use serde::Serialize;
use serde_json::{Map, Value};
use std::fmt;

#[derive(Debug, Clone, Serialize)]
pub struct AppError {
    pub code:    &'static str,
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub params:  Map<String, Value>,
    pub message: String,
}

impl AppError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, params: Map::new(), message: message.into() }
    }

    /// A parameter the UI text interpolates as `{key}`.
    pub fn with(mut self, key: &str, value: impl Serialize) -> Self {
        self.params.insert(key.to_string(), serde_json::to_value(value).unwrap_or(Value::Null));
        self
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new("internal", message)
    }

    /// For `.map_err(AppError::of)` on a database or I/O error.
    pub fn of(e: impl fmt::Display) -> Self {
        Self::internal(e.to_string())
    }

    /// The coded error somewhere in an `anyhow` error (raised with
    /// `bail!(AppError::…)` or attached as context), with the whole chain
    /// as its message; `internal` when there is none.
    pub fn from_anyhow(e: &anyhow::Error) -> Self {
        let coded = e
            .downcast_ref::<AppError>()
            .or_else(|| e.chain().find_map(|c| c.downcast_ref::<AppError>()));
        match coded {
            Some(c) => Self { message: format!("{e:#}"), ..c.clone() },
            None => Self::internal(format!("{e:#}")),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl From<String> for AppError {
    fn from(message: String) -> Self {
        Self::internal(message)
    }
}

impl From<&str> for AppError {
    fn from(message: &str) -> Self {
        Self::internal(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{bail, Context};

    #[test]
    fn a_coded_error_keeps_its_code_through_anyhow_and_context() {
        fn inner() -> anyhow::Result<()> {
            bail!(AppError::new("backup_damaged", "the backup is damaged").with("name", "database.dump"));
        }
        let e = inner().context("restore failed").unwrap_err();
        let coded = AppError::from_anyhow(&e);
        assert_eq!(coded.code, "backup_damaged");
        assert_eq!(coded.params["name"], "database.dump");
        assert_eq!(coded.message, "restore failed: the backup is damaged");

        let plain = AppError::from_anyhow(&anyhow::anyhow!("disk full"));
        assert_eq!((plain.code, plain.message.as_str()), ("internal", "disk full"));
    }

    #[test]
    fn serialises_as_the_frontend_expects() {
        let json = serde_json::to_value(AppError::new("admin_required", "Admin privileges required.")).unwrap();
        assert_eq!(json, serde_json::json!({ "code": "admin_required", "message": "Admin privileges required." }));
    }
}
