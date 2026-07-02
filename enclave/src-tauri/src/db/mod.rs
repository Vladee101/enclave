use anyhow::Context;
use sqlx::PgPool;
use tracing::info;

pub mod rls;

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
    sqlx::migrate!("../migrations")
        .run(&pool)
        .await
        .context("Migration failed")?;
    info!("Database ready.");
    Ok(pool)
}
