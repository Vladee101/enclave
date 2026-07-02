use sqlx::PgConnection;
use uuid::Uuid;

/// Set the per-transaction session variable that RLS policies key off
/// (ADR-0008).  Must be called at the start of every transaction that
/// touches a department-scoped table.
///
/// Using `set_config(..., true)` makes the change transaction-local,
/// which is safe under connection pooling: when the transaction ends
/// the variable reverts, so the next request borrowing the connection
/// cannot inherit a stale identity.
pub async fn set_current_user(
    conn: &mut PgConnection,
    user_id: Uuid,
) -> anyhow::Result<()> {
    sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
        .bind(user_id.to_string())
        .execute(conn)
        .await?;
    Ok(())
}

/// Clear the session variable (e.g. for background jobs that run
/// without a user identity after acquiring their own pool connection).
pub async fn clear_current_user(conn: &mut PgConnection) -> anyhow::Result<()> {
    sqlx::query("SELECT set_config('app.current_user_id', '', true)")
        .execute(conn)
        .await?;
    Ok(())
}
