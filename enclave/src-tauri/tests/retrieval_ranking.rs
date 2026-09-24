//! Lexical ranking at spreadsheet scale (ADR-0020): a question naming one
//! row's identifier must find that row among thousands that share every
//! other word of the question (the column headers).
//!
//! Before IDF ranking the row was 12 501st of 50 000 candidates: `ts_rank_cd`
//! weighs "сумма" (in every row) the same as "-012345" (in one). This test
//! builds the same shape — 1 500 rows > COMMON_DF, so the header words are
//! "common" — and runs the app's real `retrieval::retrieve` as a member under
//! RLS, then as a non-member.
//!
//! Same environment as rls_validation (TEST_ADMIN_URL / TEST_APP_URL, skip
//! unless ENCLAVE_REQUIRE_DB_TESTS), its own throwaway database.

use enclave_lib::{db::rls::set_current_user, retrieval};
use sqlx::PgPool;
use uuid::Uuid;

const TEST_DB_NAME: &str = "enclave_retrieval_test";
const ROWS: i32 = 1500;

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
async fn rare_identifier_beats_words_every_row_shares() -> Result<(), Box<dyn std::error::Error>> {
    let (Ok(admin_url), Ok(app_url)) = (std::env::var("TEST_ADMIN_URL"), std::env::var("TEST_APP_URL")) else {
        if std::env::var_os("ENCLAVE_REQUIRE_DB_TESTS").is_some() {
            panic!("ENCLAVE_REQUIRE_DB_TESTS is set but TEST_ADMIN_URL / TEST_APP_URL are not");
        }
        eprintln!("TEST_ADMIN_URL / TEST_APP_URL not set — skipping retrieval_ranking.");
        return Ok(());
    };

    let root = PgPool::connect(&with_database(&admin_url, "postgres")).await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {TEST_DB_NAME}")).execute(&root).await?;
    sqlx::query(&format!("CREATE DATABASE {TEST_DB_NAME}")).execute(&root).await?;
    root.close().await;

    let admin = PgPool::connect(&with_database(&admin_url, TEST_DB_NAME)).await?;
    sqlx::migrate!("../migrations").run(&admin).await?;

    let dept: Uuid = sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('Договоры', 'contracts') RETURNING id")
        .fetch_one(&admin)
        .await?;
    let other_dept: Uuid = sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('Другой', 'other') RETURNING id")
        .fetch_one(&admin)
        .await?;
    let member: Uuid = sqlx::query_scalar("INSERT INTO users (username, email, password_hash) VALUES ('member', 'm@local', 'x') RETURNING id")
        .fetch_one(&admin)
        .await?;
    let outsider: Uuid = sqlx::query_scalar("INSERT INTO users (username, email, password_hash) VALUES ('outsider', 'o@local', 'x') RETURNING id")
        .fetch_one(&admin)
        .await?;
    sqlx::query("INSERT INTO department_members VALUES ($1, $2), ($3, $4)")
        .bind(member)
        .bind(dept)
        .bind(outsider)
        .bind(other_dept)
        .execute(&admin)
        .await?;
    let model: Uuid = sqlx::query_scalar("INSERT INTO embedding_models (name, dimension, is_active) VALUES ('test', 768, true) RETURNING id")
        .fetch_one(&admin)
        .await?;
    let doc: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
         VALUES ($1, 'Реестр.xlsx', 'h', 'application/vnd.ms-excel', 1, 'ready', $2) RETURNING id",
    )
    .bind(dept)
    .bind(member)
    .fetch_one(&admin)
    .await?;

    // One line per row, exactly as extract_spreadsheet writes them; every
    // row shares "Номер договора", "Сумма", "Ответственный".
    sqlx::query(
        r#"
        WITH rows AS (
            INSERT INTO chunks (document_id, department_id, chunk_index, content, token_count)
            SELECT $1, $2, n,
                   format('[лист «Реестр», строка %s] Номер договора: Д-%s; Сумма, руб.: %s; Ответственный: %s',
                          n + 2, lpad(n::text, 6, '0'), n * 1000, (ARRAY['Иванов','Петрова','Сидоров'])[1 + n % 3]),
                   40
            FROM generate_series(1, $3) AS n
            RETURNING id, chunk_index
        )
        INSERT INTO chunk_embeddings (chunk_id, embedding_model_id, department_id, embedding)
        -- Near-identical vectors, as templated rows get: the dense leg
        -- cannot tell them apart, so finding the row is the lexical leg's job.
        SELECT id, $4, $2, array_fill((1.0 + chunk_index * 1e-6)::real, ARRAY[768])::vector FROM rows
        "#,
    )
    .bind(doc)
    .bind(dept)
    .bind(ROWS)
    .bind(model)
    .execute(&admin)
    .await?;

    let app = PgPool::connect(&with_database(&app_url, TEST_DB_NAME)).await?;
    let question = "Какая сумма у договора Д-001234 и кто по нему ответственный?";
    let query_vector = vec![1.0_f32; 768];

    let mut tx = app.begin().await?;
    set_current_user(&mut tx, member).await?;
    let found = retrieval::retrieve(&mut tx, &query_vector, question, 5).await?;
    tx.rollback().await?;
    assert!(
        found.iter().any(|c| c.content.contains("Д-001234;")),
        "the row with the asked-for identifier must be retrieved; got:\n{}",
        found.iter().map(|c| c.content.as_str()).collect::<Vec<_>>().join("\n")
    );

    // RLS still decides: the same question from another department finds
    // nothing — the rarity counts are taken under RLS too.
    let mut tx = app.begin().await?;
    set_current_user(&mut tx, outsider).await?;
    let foreign = retrieval::retrieve(&mut tx, &query_vector, question, 5).await?;
    tx.rollback().await?;
    assert!(foreign.is_empty(), "an outsider must retrieve nothing, got {} chunks", foreign.len());

    Ok(())
}
