use serde_json::Value;
use sqlx::PgConnection;
use uuid::Uuid;

/// Event types written to `audit_log.event_type`.
pub mod event {
    pub const LOGIN:              &str = "login";
    pub const LOGIN_FAILED:       &str = "login_failed";
    pub const LOGOUT:             &str = "logout";
    pub const USER_CREATED:       &str = "user_created";
    pub const DEPARTMENT_CREATED: &str = "department_created";
    pub const MEMBER_ADDED:       &str = "member_added";
    pub const MEMBER_REMOVED:     &str = "member_removed";
    pub const ADAPTER_ASSIGNED:   &str = "adapter_assigned";
    pub const DOCUMENT_UPLOADED:  &str = "document_uploaded";
    pub const DOCUMENT_DELETED:   &str = "document_deleted";
    pub const QUERY:              &str = "query";
}

/// Append one row to `audit_log` (FR14).
///
/// Takes a connection rather than a pool so the record is written inside
/// the transaction of the action it describes: both commit or neither does,
/// and there is no audited action without its row. On `app_pool` the
/// `audit_log_own_insert` policy requires `user_id = current_user_id()`, so
/// the caller must already have run `set_current_user` in that transaction
/// — a user can only ever write audit rows as themselves.
///
/// Payloads carry identifiers (documents, chunks, departments), not content:
/// the question text and document excerpts are exactly the data Enclave
/// exists to protect, and the audit answers "who accessed what", which the
/// identifiers already do.
pub async fn record(
    conn:          &mut PgConnection,
    user_id:       Option<Uuid>,
    department_id: Option<Uuid>,
    event_type:    &str,
    payload:       Value,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO audit_log (user_id, department_id, event_type, payload) VALUES ($1, $2, $3, $4)",
    )
    .bind(user_id)
    .bind(department_id)
    .bind(event_type)
    .bind(payload)
    .execute(conn)
    .await?;
    Ok(())
}
