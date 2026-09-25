//! Statements the app runs at startup, run against a database built from
//! the migrations alone — the shape every new install gets (ADR-0014).
//!
//! The development database was once hand-built from db/schema.sql and
//! carries columns no migration creates. The embedding sidecar's
//! registration named one of them (`provider`) and worked there for months,
//! while on a fresh database it failed and ingestion had no embedding model
//! (fixed by migration 016). Other tests insert their own fixtures and never
//! run the app's statements, so they could not see it.
//!
//! Same environment as rls_validation (TEST_ADMIN_URL, skip unless
//! ENCLAVE_REQUIRE_DB_TESTS), its own throwaway database.

use enclave_lib::llm::REGISTER_EMBEDDING_MODEL_SQL;
use sqlx::PgPool;

const TEST_DB_NAME: &str = "enclave_schema_contract_test";

fn with_database(url: &str, db_name: &str) -> String {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let last_slash = base.rfind('/').expect("connection URL must contain a path");
    let mut new_url = format!("{}/{}", &base[..last_slash], db_name);
    if let Some(q) = query {
        new_url.push('?');
        new_url.push_str(q);
    }
    new_url
}

#[tokio::test]
async fn startup_statements_fit_a_migrations_only_schema() -> Result<(), Box<dyn std::error::Error>> {
    let Ok(admin_url) = std::env::var("TEST_ADMIN_URL") else {
        if std::env::var_os("ENCLAVE_REQUIRE_DB_TESTS").is_some() {
            panic!("ENCLAVE_REQUIRE_DB_TESTS is set but TEST_ADMIN_URL is not");
        }
        eprintln!("TEST_ADMIN_URL not set — skipping schema_contract.");
        return Ok(());
    };

    let root = PgPool::connect(&with_database(&admin_url, "postgres")).await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {TEST_DB_NAME}")).execute(&root).await?;
    sqlx::query(&format!("CREATE DATABASE {TEST_DB_NAME}")).execute(&root).await?;
    root.close().await;

    let admin = PgPool::connect(&with_database(&admin_url, TEST_DB_NAME)).await?;
    sqlx::migrate!("../migrations").run(&admin).await?;

    // Registered twice, as on every restart: the second is the upsert path.
    for _ in 0..2 {
        sqlx::query(REGISTER_EMBEDDING_MODEL_SQL)
            .bind("nomic-embed-text-v1.5")
            .bind(768)
            .execute(&admin)
            .await?;
    }
    let active: Vec<(String, i32)> =
        sqlx::query_as("SELECT name, dimension FROM embedding_models WHERE is_active").fetch_all(&admin).await?;
    assert_eq!(active, [("nomic-embed-text-v1.5".to_string(), 768)]);
    Ok(())
}
