use sqlx::{PgPool, Row};
use uuid::Uuid;

const TEST_DB_NAME: &str = "enclave_rls_test";

/// Swap the database name in a Postgres connection URL, keeping the host,
/// port, credentials, and any query string intact.
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

/// Proves ADR-0008 (department-scoped RLS) holds — the release-blocking
/// check CLAUDE.md's "Verification" section calls for.
///
/// Per CLAUDE.md task 12: reads `TEST_ADMIN_URL` (superuser/BYPASSRLS, used
/// to seed data and run migrations) and `TEST_APP_URL` (the `enclave_app`
/// role, RLS-enforced). If either is unset, this silently no-ops instead of
/// failing, so a bare `cargo test` in an environment with no disposable
/// Postgres instance doesn't error out.
///
/// Whatever database name is in TEST_ADMIN_URL/TEST_APP_URL is ignored and
/// replaced with a dedicated `enclave_rls_test` database, dropped and
/// recreated fresh each run, on the same Postgres server. This used to
/// TRUNCATE and seed directly into whatever database those URLs pointed at
/// — harmless if they're disposable, but if someone points them at the same
/// database as ADMIN_DATABASE_URL/APP_DATABASE_URL (an easy thing to do, and
/// exactly what happened once already — the `alice`/`bob`/HR/Engineering
/// fixtures this test creates ended up permanently sitting in the real dev
/// database), it silently destroys real data. Roles (`app_user`,
/// `ingest_worker`, etc.) are cluster-wide so they're unaffected either way;
/// only the tables inside the target database were ever at risk.
#[tokio::test]
async fn test_rls_policies() -> Result<(), Box<dyn std::error::Error>> {
    let (Ok(admin_url), Ok(app_url)) = (std::env::var("TEST_ADMIN_URL"), std::env::var("TEST_APP_URL")) else {
        eprintln!("TEST_ADMIN_URL / TEST_APP_URL not set — skipping rls_validation (see CLAUDE.md task 12).");
        return Ok(());
    };

    // Recreate the dedicated test database via the `postgres` maintenance
    // database on the same server, then point both pools at it instead of
    // whatever database was named in the given URLs.
    let root_url = with_database(&admin_url, "postgres");
    let root_pool = PgPool::connect(&root_url).await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {TEST_DB_NAME}")).execute(&root_pool).await?;
    sqlx::query(&format!("CREATE DATABASE {TEST_DB_NAME}")).execute(&root_pool).await?;
    root_pool.close().await;

    let admin_url = with_database(&admin_url, TEST_DB_NAME);
    let app_url = with_database(&app_url, TEST_DB_NAME);

    let admin_pool = PgPool::connect(&admin_url).await?;
    sqlx::migrate!("../migrations").run(&admin_pool).await?;

    // Departments: HR and Engineering.
    let hr_id: Uuid = sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('HR', 'hr') RETURNING id")
        .fetch_one(&admin_pool)
        .await?;
    let eng_id: Uuid =
        sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('Engineering', 'engineering') RETURNING id")
            .fetch_one(&admin_pool)
            .await?;

    // Users: Alice (HR) and Bob (Engineering).
    let alice_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ('alice', 'alice@local', 'x') RETURNING id",
    )
    .fetch_one(&admin_pool)
    .await?;
    let bob_id: Uuid = sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ('bob', 'bob@local', 'x') RETURNING id",
    )
    .fetch_one(&admin_pool)
    .await?;

    sqlx::query("INSERT INTO department_members (user_id, department_id) VALUES ($1, $2)")
        .bind(alice_id)
        .bind(hr_id)
        .execute(&admin_pool)
        .await?;
    sqlx::query("INSERT INTO department_members (user_id, department_id) VALUES ($1, $2)")
        .bind(bob_id)
        .bind(eng_id)
        .execute(&admin_pool)
        .await?;

    // One document + chunk + embedding per department (ADR-0009: isolation
    // must hold on the denormalized department_id too, not just documents).
    //
    // NOTE on all the fixture shapes below: the live dev database is a
    // hybrid of migrations/001-004 and the (unused-by-the-app) original
    // db/schema.sql reference design — several tables ended up matching
    // schema.sql's shape instead of the migration's, because schema.sql was
    // hand-applied via psql before migrations ever ran, and `CREATE TABLE IF
    // NOT EXISTS` then silently kept the pre-existing (schema.sql) shape.
    // Concretely: embedding_models has `provider`, not `is_active`;
    // documents has NOT NULL mime_type/byte_size; chunks has NOT NULL
    // token_count with no default; department_adapters references a
    // separate `adapters` table via `adapter_id`, not an inline
    // `adapter_path`. This test's fixtures are written against the real,
    // live shapes so it actually exercises real RLS behavior. It does NOT
    // fix the app code (ingest/mod.rs, llm/mod.rs, commands/admin.rs) that
    // still assumes the migrations/002 shape for these same tables — that's
    // a separate, out-of-scope gap; see the ADR.
    let model_id: Uuid = sqlx::query_scalar(
        "INSERT INTO embedding_models (name, dimension) VALUES ('test-model', 768) RETURNING id",
    )
    .fetch_one(&admin_pool)
    .await?;

    let hr_doc_id: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
         VALUES ($1, 'hr_secrets.pdf', 'hash-hr', 'application/pdf', 1024, 'ready', $2) RETURNING id",
    )
    .bind(hr_id)
    .bind(alice_id)
    .fetch_one(&admin_pool)
    .await?;
    let eng_doc_id: Uuid = sqlx::query_scalar(
        "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
         VALUES ($1, 'eng_blueprint.pdf', 'hash-eng', 'application/pdf', 2048, 'ready', $2) RETURNING id",
    )
    .bind(eng_id)
    .bind(bob_id)
    .fetch_one(&admin_pool)
    .await?;

    let hr_chunk_id: Uuid = sqlx::query_scalar(
        "INSERT INTO chunks (document_id, department_id, chunk_index, content, token_count) \
         VALUES ($1, $2, 0, 'hr chunk', 2) RETURNING id",
    )
    .bind(hr_doc_id)
    .bind(hr_id)
    .fetch_one(&admin_pool)
    .await?;
    let eng_chunk_id: Uuid = sqlx::query_scalar(
        "INSERT INTO chunks (document_id, department_id, chunk_index, content, token_count) \
         VALUES ($1, $2, 0, 'eng chunk', 2) RETURNING id",
    )
    .bind(eng_doc_id)
    .bind(eng_id)
    .fetch_one(&admin_pool)
    .await?;

    let hr_vector = vec![0.1f32; 768];
    let eng_vector = vec![0.4f32; 768];
    sqlx::query(
        "INSERT INTO chunk_embeddings (chunk_id, embedding_model_id, department_id, embedding) VALUES ($1, $2, $3, $4)",
    )
    .bind(hr_chunk_id)
    .bind(model_id)
    .bind(hr_id)
    .bind(&hr_vector)
    .execute(&admin_pool)
    .await?;
    sqlx::query(
        "INSERT INTO chunk_embeddings (chunk_id, embedding_model_id, department_id, embedding) VALUES ($1, $2, $3, $4)",
    )
    .bind(eng_chunk_id)
    .bind(model_id)
    .bind(eng_id)
    .bind(&eng_vector)
    .execute(&admin_pool)
    .await?;

    // department_adapters here references a row in the separate `adapters`
    // table (live schema — see the note above), not an inline adapter_path.
    let hr_adapter_id: Uuid = sqlx::query_scalar(
        "INSERT INTO adapters (name, file_path, base_model, rank, alpha, file_hash) \
         VALUES ('hr-adapter', 'hr.gguf', 'base', 8, 16, 'hr-adapter-hash') RETURNING id",
    )
    .fetch_one(&admin_pool)
    .await?;
    let eng_adapter_id: Uuid = sqlx::query_scalar(
        "INSERT INTO adapters (name, file_path, base_model, rank, alpha, file_hash) \
         VALUES ('eng-adapter', 'eng.gguf', 'base', 8, 16, 'eng-adapter-hash') RETURNING id",
    )
    .fetch_one(&admin_pool)
    .await?;

    sqlx::query("INSERT INTO department_adapters (department_id, adapter_id, scale) VALUES ($1, $2, 1.0)")
        .bind(hr_id)
        .bind(hr_adapter_id)
        .execute(&admin_pool)
        .await?;
    sqlx::query("INSERT INTO department_adapters (department_id, adapter_id, scale) VALUES ($1, $2, 1.0)")
        .bind(eng_id)
        .bind(eng_adapter_id)
        .execute(&admin_pool)
        .await?;

    // Now connect as the non-superuser `enclave_app` role to test policies.
    let app_pool = PgPool::connect(&app_url).await?;

    // 1. documents: Alice sees only HR.
    {
        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(alice_id.to_string())
            .execute(&mut *tx)
            .await?;

        let docs = sqlx::query("SELECT title FROM documents").fetch_all(&mut *tx).await?;
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].get::<String, _>("title"), "hr_secrets.pdf");
        tx.rollback().await?;
    }

    // 2. documents: Bob sees only Engineering.
    {
        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(bob_id.to_string())
            .execute(&mut *tx)
            .await?;

        let docs = sqlx::query("SELECT title FROM documents").fetch_all(&mut *tx).await?;
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].get::<String, _>("title"), "eng_blueprint.pdf");
        tx.rollback().await?;
    }

    // 3. chunks + chunk_embeddings inherit the same isolation (ADR-0009's
    //    whole point — denormalized department_id must be policed too).
    {
        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(alice_id.to_string())
            .execute(&mut *tx)
            .await?;

        let chunks = sqlx::query("SELECT content FROM chunks").fetch_all(&mut *tx).await?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].get::<String, _>("content"), "hr chunk");

        let embeddings = sqlx::query("SELECT chunk_id FROM chunk_embeddings").fetch_all(&mut *tx).await?;
        assert_eq!(embeddings.len(), 1);
        assert_eq!(embeddings[0].get::<Uuid, _>("chunk_id"), hr_chunk_id);
        tx.rollback().await?;
    }

    // 4. department_adapters is scoped the same way.
    {
        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(bob_id.to_string())
            .execute(&mut *tx)
            .await?;

        let adapters = sqlx::query("SELECT adapter_id FROM department_adapters").fetch_all(&mut *tx).await?;
        assert_eq!(adapters.len(), 1);
        assert_eq!(adapters[0].get::<Uuid, _>("adapter_id"), eng_adapter_id);
        tx.rollback().await?;
    }

    // 5. Cross-department INSERT is rejected by the WITH CHECK clause, not
    //    merely hidden from SELECT — the explicit department_id filters in
    //    application queries are for scope/speed only (CLAUDE.md invariant
    //    #7); RLS itself must be the thing stopping the write.
    {
        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(alice_id.to_string())
            .execute(&mut *tx)
            .await?;

        let result = sqlx::query(
            "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
             VALUES ($1, 'sneaky.pdf', 'hash-sneaky', 'application/pdf', 1, 'ready', $2)",
        )
        .bind(eng_id)
        .bind(alice_id)
        .execute(&mut *tx)
        .await;

        assert!(result.is_err(), "Alice must not be able to insert a document into Engineering");
        tx.rollback().await?;
    }

    // 6. Unset session variable fails closed.
    {
        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', '', true)").execute(&mut *tx).await?;

        let docs = sqlx::query("SELECT title FROM documents").fetch_all(&mut *tx).await?;
        assert_eq!(docs.len(), 0);
        tx.rollback().await?;
    }

    Ok(())
}
