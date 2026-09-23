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

    // Reproduce the drift found in real databases before migrating: default
    // privileges handing app_user write on every new table and EXECUTE on
    // every new function in `public`. Migration 012 must remove them; on a
    // clean database section 9's "table created later" check would pass
    // trivially. (Roles are cluster-wide; on a brand-new cluster app_user
    // does not exist before 004, and there is no drift to reproduce.)
    sqlx::query(
        "DO $$ BEGIN
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'app_user') THEN
                ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT ALL ON TABLES TO app_user;
                ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT EXECUTE ON FUNCTIONS TO app_user;
            END IF;
         END $$",
    )
    .execute(&admin_pool)
    .await?;

    sqlx::migrate!("../migrations").run(&admin_pool).await?;

    // 0. Migrations leave exactly one shared default department (ADR-0016);
    //    cmd_create_user relies on it, and the partial unique index forbids a
    //    second.
    let defaults: Vec<String> = sqlx::query_scalar("SELECT name FROM departments WHERE is_default")
        .fetch_all(&admin_pool)
        .await?;
    assert_eq!(defaults, vec!["General".to_string()]);
    let second_default = sqlx::query("INSERT INTO departments (name, slug, is_default) VALUES ('Other', 'other', true)")
        .execute(&admin_pool)
        .await;
    assert!(second_default.is_err(), "a second default department must be rejected");

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

    // 5b. ingestion_jobs is scoped through its parent document. This is the
    //     query cmd_get_job_status runs: with the identity set in the same
    //     transaction Alice sees her HR job and not Bob's; on a bare pool
    //     query (no set_config — the old cmd_get_job_status bug) she sees
    //     nothing, which left the frontend poller spinning forever.
    {
        let hr_job_id: Uuid = sqlx::query_scalar(
            "INSERT INTO ingestion_jobs (document_id, status) VALUES ($1, 'succeeded') RETURNING id",
        )
        .bind(hr_doc_id)
        .fetch_one(&admin_pool)
        .await?;
        let eng_job_id: Uuid = sqlx::query_scalar(
            "INSERT INTO ingestion_jobs (document_id, status) VALUES ($1, 'succeeded') RETURNING id",
        )
        .bind(eng_doc_id)
        .fetch_one(&admin_pool)
        .await?;

        let job_status_sql = "SELECT status FROM ingestion_jobs WHERE id = $1";

        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(alice_id.to_string())
            .execute(&mut *tx)
            .await?;
        let own = sqlx::query(job_status_sql).bind(hr_job_id).fetch_optional(&mut *tx).await?;
        assert_eq!(own.map(|r| r.get::<String, _>("status")).as_deref(), Some("succeeded"));
        let other = sqlx::query(job_status_sql).bind(eng_job_id).fetch_optional(&mut *tx).await?;
        assert!(other.is_none(), "Alice must not see an Engineering ingestion job");
        tx.rollback().await?;

        let bare = sqlx::query(job_status_sql).bind(hr_job_id).fetch_optional(&app_pool).await?;
        assert!(bare.is_none(), "ingestion_jobs must fail closed without app.current_user_id");
    }

    // 6. Unset session variable fails closed.
    {
        let mut tx = app_pool.begin().await?;
        sqlx::query("SELECT set_config('app.current_user_id', '', true)").execute(&mut *tx).await?;

        let docs = sqlx::query("SELECT title FROM documents").fetch_all(&mut *tx).await?;
        assert_eq!(docs.len(), 0);
        tx.rollback().await?;
    }

    // 7. Document deletion (ADR-0015): uploader or administrator only, and
    //    only through delete_document(). Last on purpose — it deletes.
    {
        // Carol: HR member who did not upload hr_secrets.pdf. Dan: admin,
        // member of no department.
        let carol_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash) VALUES ('carol', 'carol@local', 'x') RETURNING id",
        )
        .fetch_one(&admin_pool)
        .await?;
        sqlx::query("INSERT INTO department_members (user_id, department_id) VALUES ($1, $2)")
            .bind(carol_id)
            .bind(hr_id)
            .execute(&admin_pool)
            .await?;
        let dan_id: Uuid = sqlx::query_scalar(
            "INSERT INTO users (username, email, password_hash, is_admin) VALUES ('dan', 'dan@local', 'x', true) RETURNING id",
        )
        .fetch_one(&admin_pool)
        .await?;

        let denied = "42501"; // insufficient_privilege

        // 7a. Not the uploader, not a member, no identity: all refused with
        //     the same code — no hint whether the document exists.
        for (who, user) in [("carol (member, not uploader)", Some(carol_id)), ("bob (not a member)", Some(bob_id)), ("no identity", None)] {
            let err = delete_as(&app_pool, user, hr_doc_id).await.expect_err(who);
            assert_eq!(sqlstate(&err).as_deref(), Some(denied), "{who}: {err}");
        }

        // 7b. The direct paths are closed for app_user whatever the policies:
        //     no DELETE on documents, no UPDATE of deleted_at, no writes to chunks.
        for (what, sql) in [
            ("DELETE documents", "DELETE FROM documents WHERE id = $1"),
            ("UPDATE documents.deleted_at", "UPDATE documents SET deleted_at = now() WHERE id = $1"),
            ("DELETE chunks", "DELETE FROM chunks WHERE document_id = $1"),
            ("UPDATE chunks", "UPDATE chunks SET content = '' WHERE document_id = $1"),
        ] {
            let mut tx = app_pool.begin().await?;
            sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
                .bind(alice_id.to_string())
                .execute(&mut *tx)
                .await?;
            let err = sqlx::query(sql).bind(hr_doc_id).execute(&mut *tx).await.expect_err(what);
            assert_eq!(sqlstate(&err).as_deref(), Some(denied), "{what}: {err}");
            tx.rollback().await?;
        }

        // 7c. The uploader can: content purged, tombstone kept.
        let (hash, dept, blob_still_used) = delete_as(&app_pool, Some(alice_id), hr_doc_id).await?;
        assert_eq!((hash.as_str(), dept, blob_still_used), ("hash-hr", hr_id, false));
        let chunks_left: i64 = sqlx::query_scalar("SELECT count(*) FROM chunks WHERE document_id = $1")
            .bind(hr_doc_id)
            .fetch_one(&admin_pool)
            .await?;
        let vectors_left: i64 = sqlx::query_scalar("SELECT count(*) FROM chunk_embeddings WHERE chunk_id = $1")
            .bind(hr_chunk_id)
            .fetch_one(&admin_pool)
            .await?;
        assert_eq!((chunks_left, vectors_left), (0, 0), "deletion must purge chunks and embeddings");
        let deleted_by: Option<Uuid> = sqlx::query_scalar("SELECT deleted_by FROM documents WHERE id = $1 AND deleted_at IS NOT NULL")
            .bind(hr_doc_id)
            .fetch_one(&admin_pool)
            .await?;
        assert_eq!(deleted_by, Some(alice_id), "tombstone must record who deleted");

        // Deleting it again: not found, not a second tombstone.
        let err = delete_as(&app_pool, Some(alice_id), hr_doc_id).await.expect_err("second delete");
        assert_eq!(sqlstate(&err).as_deref(), Some("P0002"));

        // 7d. The same file can be uploaded again as a new document —
        //     uniqueness covers live documents only.
        {
            let mut tx = app_pool.begin().await?;
            sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
                .bind(alice_id.to_string())
                .execute(&mut *tx)
                .await?;
            let new_id: Option<Uuid> = sqlx::query_scalar(
                "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, uploaded_by) \
                 VALUES ($1, 'hr_secrets.pdf', 'hash-hr', 'application/pdf', 1024, $2) \
                 ON CONFLICT (department_id, file_hash) WHERE deleted_at IS NULL DO NOTHING RETURNING id",
            )
            .bind(hr_id)
            .bind(alice_id)
            .fetch_optional(&mut *tx)
            .await?;
            assert!(new_id.is_some_and(|id| id != hr_doc_id), "re-upload after deletion must create a new document");
            tx.rollback().await?;
        }

        // 7e. An administrator can delete in a department they are not in
        //     (a misfiled document).
        let (_, dept, _) = delete_as(&app_pool, Some(dan_id), eng_doc_id).await?;
        assert_eq!(dept, eng_id);

        // 8. Department deletion with its documents (ADR-0017): admin only,
        //    never the default department, content purged, tombstones kept.
        let legal_id: Uuid =
            sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('Legal', 'legal') RETURNING id")
                .fetch_one(&admin_pool)
                .await?;
        sqlx::query("INSERT INTO department_members (user_id, department_id) VALUES ($1, $2)")
            .bind(carol_id)
            .bind(legal_id)
            .execute(&admin_pool)
            .await?;
        sqlx::query("INSERT INTO department_adapters (department_id, adapter_id, scale) VALUES ($1, $2, 1.0)")
            .bind(legal_id)
            .bind(hr_adapter_id)
            .execute(&admin_pool)
            .await?;
        // Two Legal documents: one with its own bytes, one whose bytes also
        // back a live HR document — that blob must survive.
        let mut legal_docs = Vec::new();
        for (title, hash) in [("contract.md", "hash-legal-only"), ("shared.md", "hash-shared")] {
            let doc: Uuid = sqlx::query_scalar(
                "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
                 VALUES ($1, $2, $3, 'text/markdown', 10, 'ready', $4) RETURNING id",
            )
            .bind(legal_id)
            .bind(title)
            .bind(hash)
            .bind(carol_id)
            .fetch_one(&admin_pool)
            .await?;
            let chunk: Uuid = sqlx::query_scalar(
                "INSERT INTO chunks (document_id, department_id, chunk_index, content, token_count) \
                 VALUES ($1, $2, 0, 'legal text', 2) RETURNING id",
            )
            .bind(doc)
            .bind(legal_id)
            .fetch_one(&admin_pool)
            .await?;
            sqlx::query("INSERT INTO chunk_embeddings (chunk_id, embedding_model_id, department_id, embedding) VALUES ($1, $2, $3, $4)")
                .bind(chunk)
                .bind(model_id)
                .bind(legal_id)
                .bind(vec![0.3_f32; 768])
                .execute(&admin_pool)
                .await?;
            legal_docs.push(doc);
        }
        sqlx::query(
            "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
             VALUES ($1, 'shared.md', 'hash-shared', 'text/markdown', 10, 'ready', $2)",
        )
        .bind(hr_id)
        .bind(alice_id)
        .execute(&admin_pool)
        .await?;

        // 8a. A member (even one who uploaded everything in it) cannot.
        let err = delete_department_as(&app_pool, Some(carol_id), legal_id).await.expect_err("carol");
        assert_eq!(sqlstate(&err).as_deref(), Some(denied), "{err}");

        // 8b. Not even an admin can delete the default department.
        let general_id: Uuid = sqlx::query_scalar("SELECT id FROM departments WHERE is_default")
            .fetch_one(&admin_pool)
            .await?;
        let err = delete_department_as(&app_pool, Some(dan_id), general_id).await.expect_err("general");
        assert_eq!(sqlstate(&err).as_deref(), Some("23001"), "{err}");

        // 8c. An admin can; content goes, tombstones stay.
        let (mut deleted_docs, orphaned) = delete_department_as(&app_pool, Some(dan_id), legal_id).await?;
        deleted_docs.sort();
        legal_docs.sort();
        assert_eq!(deleted_docs, legal_docs);
        assert_eq!(orphaned, vec!["hash-legal-only".to_string()], "a blob another live document uses must stay");

        let counts: (i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM chunks WHERE department_id = $1), \
                    (SELECT count(*) FROM chunk_embeddings WHERE department_id = $1), \
                    (SELECT count(*) FROM documents WHERE department_id = $1 AND deleted_at IS NULL), \
                    (SELECT count(*) FROM department_members WHERE department_id = $1), \
                    (SELECT count(*) FROM department_adapters WHERE department_id = $1)",
        )
        .bind(legal_id)
        .fetch_one(&admin_pool)
        .await?;
        assert_eq!(counts, (0, 0, 0, 0, 0), "chunks, vectors, live documents, members, adapters");
        let tombstones: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM documents WHERE department_id = $1 AND deleted_by = $2",
        )
        .bind(legal_id)
        .bind(dan_id)
        .fetch_one(&admin_pool)
        .await?;
        assert_eq!(tombstones, 2, "document tombstones must stay for audit_log");
        let dept_deleted_by: Option<Uuid> =
            sqlx::query_scalar("SELECT deleted_by FROM departments WHERE id = $1 AND deleted_at IS NOT NULL")
                .bind(legal_id)
                .fetch_one(&admin_pool)
                .await?;
        assert_eq!(dept_deleted_by, Some(dan_id));

        // 8d. The name is free again; a second delete finds nothing.
        sqlx::query("INSERT INTO departments (name, slug) VALUES ('Legal', 'legal-2')")
            .execute(&admin_pool)
            .await?;
        let err = delete_department_as(&app_pool, Some(dan_id), legal_id).await.expect_err("twice");
        assert_eq!(sqlstate(&err).as_deref(), Some("P0002"), "{err}");

        // 9. Least privilege (ADR-0018): app_user writes exactly what the app
        //    writes on app_pool. `UPDATE users SET is_admin` used to succeed —
        //    a straight escalation past every admin check above.
        sqlx::query("CREATE TABLE probe_future_table (id INT)").execute(&admin_pool).await?;
        for (what, sql) in [
            ("make self admin", "UPDATE users SET is_admin = true WHERE id = $1"),
            ("create a user", "INSERT INTO users (username, email, password_hash, is_admin) VALUES ('mallory', 'm@local', 'x', true) RETURNING $1"),
            ("join a department", "INSERT INTO department_members (user_id, department_id) SELECT $1, id FROM departments WHERE slug = 'engineering'"),
            ("create a department", "INSERT INTO departments (name, slug) SELECT 'Rogue', 'rogue' WHERE $1 IS NOT NULL"),
            ("unassign an adapter", "DELETE FROM department_adapters WHERE $1 IS NOT NULL"),
            ("assign an adapter", "INSERT INTO department_adapters (department_id, adapter_id, scale) SELECT d.id, a.id, 2.0 FROM departments d, adapters a WHERE d.slug = 'hr' AND $1 IS NOT NULL LIMIT 1"),
            ("register an adapter file", "INSERT INTO adapters (name, file_path, base_model, rank, alpha, file_hash) SELECT 'r', 'r.gguf', 'b', 8, 16, 'r' WHERE $1 IS NOT NULL"),
            ("register an embedding model", "INSERT INTO embedding_models (name, dimension) SELECT 'rogue', 768 WHERE $1 IS NOT NULL"),
            ("rewrite a job", "UPDATE ingestion_jobs SET status = 'succeeded' WHERE $1 IS NOT NULL"),
            ("rewrite the audit log", "UPDATE audit_log SET event_type = 'x' WHERE user_id = $1"),
            ("erase the audit log", "DELETE FROM audit_log WHERE user_id = $1"),
            ("write a table created later", "INSERT INTO probe_future_table SELECT 1 WHERE $1 IS NOT NULL"),
        ] {
            let mut tx = app_pool.begin().await?;
            sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
                .bind(alice_id.to_string())
                .execute(&mut *tx)
                .await?;
            let err = sqlx::query(sql).bind(alice_id).execute(&mut *tx).await.expect_err(what);
            assert_eq!(sqlstate(&err).as_deref(), Some(denied), "{what}: {err}");
            tx.rollback().await?;
        }

        // …while the writes the app does make still work.
        {
            let mut tx = app_pool.begin().await?;
            sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
                .bind(alice_id.to_string())
                .execute(&mut *tx)
                .await?;
            let doc: Uuid = sqlx::query_scalar(
                "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, uploaded_by) \
                 VALUES ($1, 'fresh.md', 'hash-fresh', 'text/markdown', 1, $2) RETURNING id",
            )
            .bind(hr_id)
            .bind(alice_id)
            .fetch_one(&mut *tx)
            .await?;
            sqlx::query("UPDATE documents SET status = 'pending', updated_at = now() WHERE id = $1")
                .bind(doc)
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO ingestion_jobs (document_id) VALUES ($1)").bind(doc).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO audit_log (user_id, event_type) VALUES ($1, 'probe')")
                .bind(alice_id)
                .execute(&mut *tx)
                .await?;
            tx.rollback().await?;
        }
    }

    Ok(())
}

/// Run delete_department() as `user`, like cmd_delete_department does.
async fn delete_department_as(pool: &PgPool, user: Option<Uuid>, dept: Uuid) -> Result<(Vec<Uuid>, Vec<String>), sqlx::Error> {
    let mut tx = pool.begin().await?;
    if let Some(user) = user {
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(user.to_string())
            .execute(&mut *tx)
            .await?;
    }
    match sqlx::query_as("SELECT document_ids, orphaned_blobs FROM delete_department($1)")
        .bind(dept)
        .fetch_one(&mut *tx)
        .await
    {
        Ok(row) => {
            tx.commit().await?;
            Ok(row)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// Run delete_document() as `user` in its own transaction, like
/// cmd_delete_document does; commit on success.
async fn delete_as(pool: &PgPool, user: Option<Uuid>, doc: Uuid) -> Result<(String, Uuid, bool), sqlx::Error> {
    let mut tx = pool.begin().await?;
    if let Some(user) = user {
        sqlx::query("SELECT set_config('app.current_user_id', $1, true)")
            .bind(user.to_string())
            .execute(&mut *tx)
            .await?;
    }
    match sqlx::query_as("SELECT file_hash, department_id, blob_still_used FROM delete_document($1)")
        .bind(doc)
        .fetch_one(&mut *tx)
        .await
    {
        Ok(row) => {
            tx.commit().await?;
            Ok(row)
        }
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    e.as_database_error().and_then(|d| d.code()).map(|c| c.into_owned())
}
