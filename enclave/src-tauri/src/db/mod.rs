use anyhow::Context;
use sqlx::PgPool;
use tracing::info;

pub mod embedded;
pub mod rls;

/// The migrations built into this binary. Also run on a database restored
/// from a backup before it replaces the live one (ADR-0026).
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

/// The newest migration this build knows; a backup made by a build that
/// knew a newer one cannot be restored here.
pub fn latest_migration() -> i64 {
    MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
}

/// Build a connection pool without running migrations.
pub async fn build_pool(url: &str) -> anyhow::Result<PgPool> {
    info!("Connecting to database…");
    sqlx::postgres::PgPoolOptions::new()
        .max_connections(10)
        .connect(url)
        .await
        .context("Failed to connect to PostgreSQL")
}

/// Build a pool and run all pending migrations against it.
pub async fn connect_and_migrate(url: &str) -> anyhow::Result<PgPool> {
    let pool = build_pool(url).await?;
    info!("Running migrations…");
    MIGRATOR
        .run(&pool)
        .await
        .context("Migration failed")?;
    info!("Database ready.");
    Ok(pool)
}
