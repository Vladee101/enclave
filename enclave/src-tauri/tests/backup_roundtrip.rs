//! A backup made, the data changed, the backup restored (ADR-0026): the
//! database and the document files come back exactly as they were when the
//! backup was made, and a damaged backup changes nothing.
//!
//! Runs a scratch PostgreSQL from the bundled binaries
//! (`binaries/pg`, scripts/fetch-postgres.ps1) — the same server the app
//! runs, with the same pg_dump/pg_restore. Skipped where they are absent
//! (CI), unless ENCLAVE_REQUIRE_BACKUP_TEST is set.

use enclave_lib::backup::{self, Cluster};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Server {
    bin:   PathBuf,
    data:  PathBuf,
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let stopped = Command::new(self.bin.join("pg_ctl"))
            .arg("stop").arg("-D").arg(&self.data).args(["-m", "immediate", "-w"])
            .stdout(Stdio::null()).stderr(Stdio::null())
            .status();
        if !matches!(stopped, Ok(s) if s.success()) {
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

fn sha(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

async fn start_server(bin: &Path, root: &Path) -> (Server, String) {
    let data = root.join("pgdata");
    let out = Command::new(bin.join("initdb"))
        .arg("-D").arg(&data)
        .args(["-U", "postgres", "-E", "UTF8", "--auth=trust"])
        .args(["--locale-provider=builtin", "--builtin-locale=C.UTF-8"])
        .output()
        .expect("initdb runs");
    assert!(out.status.success(), "initdb: {}", String::from_utf8_lossy(&out.stderr));

    let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let child = Command::new(bin.join("postgres"))
        .arg("-D").arg(&data)
        .args(["-c", "listen_addresses=127.0.0.1", "-p", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(fs::File::create(root.join("postgres.log")).unwrap())
        .spawn()
        .expect("postgres starts");
    let server = Server { bin: bin.to_path_buf(), data, child };

    let base = format!("postgres://postgres@127.0.0.1:{port}/");
    let started = Instant::now();
    loop {
        if let Ok(pool) = PgPool::connect(&format!("{base}postgres")).await {
            pool.close().await;
            break;
        }
        assert!(started.elapsed() < Duration::from_secs(60), "scratch PostgreSQL did not start");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    (server, base)
}

/// A user, their department and a document, with its file in the store
/// or not.
async fn add_document(pool: &PgPool, blobs: &Path, user: &str, content: &[u8], on_disk: bool) {
    let hash = sha(content);
    let user_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO users (username, email, password_hash) VALUES ($1, $1 || '@local', 'x') RETURNING id",
    )
    .bind(user)
    .fetch_one(pool)
    .await
    .unwrap();
    let dept_id: uuid::Uuid = sqlx::query_scalar(
        "INSERT INTO departments (name, slug) VALUES ($1, md5($1)) RETURNING id",
    )
    .bind(format!("Отдел {user}"))
    .fetch_one(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, uploaded_by, status)
         VALUES ($1, $2, $3, 'text/plain', $4, $5, 'ready')",
    )
    .bind(dept_id)
    .bind(format!("{}.txt", String::from_utf8_lossy(content)))
    .bind(&hash)
    .bind(content.len() as i64)
    .bind(user_id)
    .execute(pool)
    .await
    .unwrap();
    if on_disk {
        fs::write(blobs.join(&hash), content).unwrap();
    }
}

async fn titles(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT title FROM documents ORDER BY title").fetch_all(pool).await.unwrap()
}

async fn usernames(pool: &PgPool) -> Vec<String> {
    sqlx::query_scalar("SELECT username FROM users ORDER BY username").fetch_all(pool).await.unwrap()
}

fn stored(blobs: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        fs::read_dir(blobs).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[tokio::test]
async fn a_restored_backup_brings_back_the_database_and_the_files() {
    let bin = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries").join("pg").join("bin");
    if !bin.join(if cfg!(windows) { "postgres.exe" } else { "postgres" }).is_file() {
        assert!(
            std::env::var_os("ENCLAVE_REQUIRE_BACKUP_TEST").is_none(),
            "ENCLAVE_REQUIRE_BACKUP_TEST is set but {} has no PostgreSQL",
            bin.display()
        );
        eprintln!("No bundled PostgreSQL in {} — skipping backup_roundtrip.", bin.display());
        return;
    }

    let root = std::env::temp_dir().join(format!("enclave-backup-roundtrip-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let app_data = root.join("app-data");
    let blobs = app_data.join("blobs");
    fs::create_dir_all(&blobs).unwrap();
    let (server, base) = start_server(&bin, &root).await;
    let cluster = Cluster { bin: bin.clone(), base: base.clone() };

    let maintenance = PgPool::connect(&cluster.url("postgres")).await.unwrap();
    sqlx::query("CREATE DATABASE enclave").execute(&maintenance).await.unwrap();
    let db = PgPool::connect(&cluster.url("enclave")).await.unwrap();
    enclave_lib::db::MIGRATOR.run(&db).await.unwrap();

    // ── The state the backup captures ────────────────────────────────────
    add_document(&db, &blobs, "anna", "договор аренды".as_bytes(), true).await;
    add_document(&db, &blobs, "boris", "счёт".as_bytes(), false).await; // its file already lost

    let archive = root.join("copy.enclave-backup");
    let manifest = backup::create(&db, &bin, &cluster.url("enclave"), &blobs, &archive, Arc::new(|_| {}))
        .await
        .expect("backup is written");
    assert_eq!(manifest.counts.documents, 2);
    assert_eq!(manifest.blobs.len(), 1);
    assert_eq!(manifest.missing, vec![sha("счёт".as_bytes())]);
    assert!(!root.join("copy.enclave-backup.part").exists() && !root.join("copy.enclave-backup.dump.tmp").exists());

    // ── Life goes on after it ────────────────────────────────────────────
    add_document(&db, &blobs, "vera", "приказ".as_bytes(), true).await;
    let before_restore = (titles(&db).await, usernames(&db).await, stored(&blobs));

    // ── A damaged copy is refused and changes nothing ────────────────────
    let damaged = root.join("damaged.enclave-backup");
    let mut bytes = fs::read(&archive).unwrap();
    let needle = "договор".as_bytes();
    let at = bytes.windows(needle.len()).position(|w| w == needle).expect("the file is stored, not compressed");
    bytes[at] ^= 0x01;
    fs::write(&damaged, bytes).unwrap();
    let err = backup::stage(&cluster, &app_data, &damaged, "anna", Arc::new(|_| {})).await.unwrap_err();
    assert!(format!("{err:#}").contains("checksum"), "{err:#}");
    assert!(backup::staged(&app_data).unwrap().is_none());
    assert!(!app_data.join("restore").exists());
    let staging_left: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'enclave_restore')")
        .fetch_one(&maintenance)
        .await
        .unwrap();
    assert!(!staging_left);

    // ── The intact copy is staged: live data still untouched ────────────
    let staged = backup::stage(&cluster, &app_data, &archive, "anna", Arc::new(|_| {})).await.expect("restore is staged");
    assert_eq!(staged.staged_by, "anna");
    assert!(backup::staged(&app_data).unwrap().is_some());
    assert_eq!((titles(&db).await, usernames(&db).await, stored(&blobs)), before_restore);
    db.close().await;

    // ── Next start: swapped in ───────────────────────────────────────────
    let applied = backup::apply_staged(&maintenance, &app_data).await.unwrap().expect("a staged restore");
    assert_eq!(applied.manifest.created_at, manifest.created_at);
    assert!(!app_data.join("restore").exists());
    assert!(backup::apply_staged(&maintenance, &app_data).await.unwrap().is_none(), "applied once only");

    let db = PgPool::connect(&cluster.url("enclave")).await.unwrap();
    assert_eq!(titles(&db).await, vec!["договор аренды.txt".to_string(), "счёт.txt".to_string()]);
    assert_eq!(usernames(&db).await, vec!["anna".to_string(), "boris".to_string()]);
    assert_eq!(stored(&blobs), vec![sha("договор аренды".as_bytes())]);
    assert_eq!(fs::read(blobs.join(sha("договор аренды".as_bytes()))).unwrap(), "договор аренды".as_bytes());
    // The app's roles still have what the migrations granted them.
    let granted: bool = sqlx::query_scalar("SELECT has_table_privilege('app_user', 'documents', 'SELECT')")
        .fetch_one(&db)
        .await
        .unwrap();
    assert!(granted);
    // And the restored database is at this build's migration.
    let at: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations").fetch_one(&db).await.unwrap();
    assert_eq!(at, enclave_lib::db::latest_migration());
    db.close().await;
    maintenance.close().await;

    drop(server);
    let _ = fs::remove_dir_all(&root);
}
