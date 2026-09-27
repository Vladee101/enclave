//! Backups (ADR-0026): the database and the documents' files in one archive.
//!
//! The archive is a ZIP with every entry stored, not compressed (the dump
//! is compressed by pg_dump, most documents by their own format):
//!
//! - `enclave-backup.json` — the manifest: what made the backup, the
//!   migration the database was at, counts for the restore confirmation,
//!   and the size and SHA-256 of every other entry;
//! - `database.dump` — `pg_dump --format=custom` of the whole database;
//! - `blobs/{sha256}` — the file of every live document.
//!
//! The dump and the list of files come from one snapshot: a REPEATABLE
//! READ transaction exports it, `pg_dump --snapshot` dumps exactly that
//! state, and the file list is read in the same transaction.
//!
//! A restore never touches the live data until the next start. It checks
//! every entry against the manifest, restores the dump into a separate
//! database, runs this build's migrations on it, checks that every live
//! document has its file, and only then leaves a marker; `apply_staged`,
//! called by the embedded server's start, swaps the database and the files.
//! Only the app's own database can be restored this way; a server's is
//! restored with the server's own tools.

use anyhow::{bail, ensure, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tracing::info;
use zip::write::SimpleFileOptions;
use zip::CompressionMethod;

use crate::db::embedded::{self, DATABASE};

/// Bumped when the archive layout changes; a build reads only its own.
pub const FORMAT: u32 = 1;

const MANIFEST: &str = "enclave-backup.json";
const DUMP: &str = "database.dump";
const BLOBS: &str = "blobs/";

/// The database a restore is staged in, next to the live one.
const STAGING_DATABASE: &str = "enclave_restore";

/// `{app_data}/restore/`: the staged files and, once staging is complete,
/// the marker.
const STAGING_DIR: &str = "restore";
const MARKER: &str = "staged.json";

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    pub size:   u64,
    pub sha256: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Counts {
    pub documents:   i64,
    pub users:       i64,
    pub departments: i64,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Manifest {
    pub format:         u32,
    pub app_version:    String,
    pub created_at:     DateTime<Utc>,
    /// The newest migration applied to the backed-up database.
    pub migration:      i64,
    pub server_version: String,
    pub counts:         Counts,
    pub database:       Entry,
    /// Keyed by the file's SHA-256, which is also its name in the store.
    pub blobs:          Vec<Entry>,
    /// Live documents whose file was not in the store, or not what its name
    /// says, when the backup was made. They are listed, not hidden: the
    /// restored documents lack their files exactly as the originals did.
    pub missing:        Vec<String>,
}

impl Manifest {
    pub fn bytes(&self) -> u64 {
        self.database.size + self.blobs.iter().map(|b| b.size).sum::<u64>()
    }
}

/// A restore waiting for the next start.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Staged {
    pub manifest:  Manifest,
    pub staged_at: DateTime<Utc>,
    /// A name, not an id: the id belongs to the database being replaced.
    pub staged_by: String,
}

/// Progress for the UI: a stage and, for files, how many are done.
#[derive(Serialize, Clone, Debug)]
pub struct Progress {
    pub stage: &'static str,
    pub done:  usize,
    pub total: usize,
}

pub type OnProgress = Arc<dyn Fn(Progress) + Send + Sync>;

/// The server a restore is staged on: the embedded one
/// (`EmbeddedPostgres::cluster`), or a scratch one in tests.
pub struct Cluster {
    /// The PostgreSQL `bin` directory (for `pg_restore`).
    pub bin:  PathBuf,
    /// `postgres://postgres:…@127.0.0.1:port/` — a database name completes it.
    pub base: String,
}

impl Cluster {
    pub fn url(&self, database: &str) -> String {
        format!("{}{database}", self.base)
    }
}

// ─── Making a backup ─────────────────────────────────────────────────────────

/// Write a backup of the database `database_url` names (superuser) and of
/// `blob_root` to `out`. The archive is written to `out.part` and renamed
/// when complete, so `out` is never a half-written backup.
pub async fn create(
    admin: &PgPool,
    bin: &Path,
    database_url: &str,
    blob_root: &Path,
    out: &Path,
    progress: OnProgress,
) -> Result<Manifest> {
    let part = with_suffix(out, ".part");
    let dump = with_suffix(out, ".dump.tmp");
    let result = create_inner(admin, bin, database_url, blob_root, out, &part, &dump, progress).await;
    let _ = fs::remove_file(&dump);
    if result.is_err() {
        let _ = fs::remove_file(&part);
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn create_inner(
    admin: &PgPool,
    bin: &Path,
    database_url: &str,
    blob_root: &Path,
    out: &Path,
    part: &Path,
    dump: &Path,
    progress: OnProgress,
) -> Result<Manifest> {
    progress(Progress { stage: "database", done: 0, total: 0 });

    // One snapshot for the dump and the file list. The transaction stays
    // open until pg_dump has taken the snapshot over — to its end, simply.
    let mut tx = admin.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY").execute(&mut *tx).await?;
    let snapshot: String = sqlx::query_scalar("SELECT pg_export_snapshot()").fetch_one(&mut *tx).await?;
    let hashes: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT file_hash FROM documents WHERE deleted_at IS NULL ORDER BY 1")
            .fetch_all(&mut *tx)
            .await?;
    let (documents, users, departments): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM documents WHERE deleted_at IS NULL),
                (SELECT count(*) FROM users),
                (SELECT count(*) FROM departments WHERE deleted_at IS NULL)",
    )
    .fetch_one(&mut *tx)
    .await?;
    let migration: i64 = sqlx::query_scalar("SELECT COALESCE(max(version), 0) FROM _sqlx_migrations WHERE success")
        .fetch_one(&mut *tx)
        .await?;
    let server_version: String = sqlx::query_scalar("SHOW server_version").fetch_one(&mut *tx).await?;

    let mut cmd = embedded::tool(bin, "pg_dump");
    cmd.args(["--format=custom", &format!("--snapshot={snapshot}")]).arg("--file").arg(dump);
    run_with_url(cmd, database_url, "pg_dump").await?;
    tx.rollback().await?;

    let manifest = Manifest {
        format: FORMAT,
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        created_at: Utc::now(),
        migration,
        server_version,
        counts: Counts { documents, users, departments },
        database: Entry { size: 0, sha256: String::new() },
        blobs: Vec::new(),
        missing: Vec::new(),
    };
    let (part2, dump2, blob_root) = (part.to_path_buf(), dump.to_path_buf(), blob_root.to_path_buf());
    let manifest = tauri::async_runtime::spawn_blocking(move || {
        write_archive(&part2, &dump2, &blob_root, &hashes, manifest, &*progress)
    })
    .await
    .context("backup task failed")??;
    fs::rename(part, out).with_context(|| format!("could not write {}", out.display()))?;
    info!(
        "Backup written: {} documents, {} files, {} missing, {:.1} MB.",
        manifest.counts.documents,
        manifest.blobs.len(),
        manifest.missing.len(),
        manifest.bytes() as f64 / 1e6
    );
    Ok(manifest)
}

fn write_archive(
    part: &Path,
    dump: &Path,
    blob_root: &Path,
    hashes: &[String],
    mut manifest: Manifest,
    progress: &(dyn Fn(Progress) + Send + Sync),
) -> Result<Manifest> {
    let mut zip = zip::ZipWriter::new(File::create(part).with_context(|| format!("could not create {}", part.display()))?);

    manifest.database = add_file(&mut zip, DUMP, dump)?;

    for (i, hash) in hashes.iter().enumerate() {
        progress(Progress { stage: "files", done: i, total: hashes.len() });
        let path = blob_root.join(hash);
        if !path.is_file() {
            manifest.missing.push(hash.clone());
            continue;
        }
        let entry = add_file(&mut zip, &format!("{BLOBS}{hash}"), &path)?;
        if entry.sha256 != *hash {
            // Damaged on disk: not what the document was uploaded as.
            zip.abort_file()?;
            manifest.missing.push(hash.clone());
            continue;
        }
        manifest.blobs.push(entry);
    }
    progress(Progress { stage: "files", done: hashes.len(), total: hashes.len() });

    zip.start_file(MANIFEST, SimpleFileOptions::default().compression_method(CompressionMethod::Deflated))?;
    zip.write_all(&serde_json::to_vec_pretty(&manifest)?)?;
    zip.finish()?.sync_all()?;
    Ok(manifest)
}

/// Copy a file into the archive, stored, hashing it on the way.
fn add_file<W: Write + std::io::Seek>(zip: &mut zip::ZipWriter<W>, name: &str, path: &Path) -> Result<Entry> {
    let mut file = File::open(path).with_context(|| format!("could not read {}", path.display()))?;
    let size = file.metadata()?.len();
    let options = SimpleFileOptions::default()
        .compression_method(CompressionMethod::Stored)
        .large_file(size >= u32::MAX as u64);
    zip.start_file(name, options)?;
    let sha256 = copy_hashing(&mut file, zip)?;
    Ok(Entry { size, sha256 })
}

fn copy_hashing(from: &mut impl Read, to: &mut impl Write) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        to.write_all(&buf[..n])?;
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ─── Reading one ─────────────────────────────────────────────────────────────

/// The manifest of an archive, checked for what this build can restore.
/// Cheap: reads one small entry.
pub fn read_manifest(archive: &Path) -> Result<Manifest> {
    let mut zip = open_archive(archive)?;
    manifest_of(&mut zip)
}

fn open_archive(archive: &Path) -> Result<zip::ZipArchive<File>> {
    let file = File::open(archive).with_context(|| format!("could not open {}", archive.display()))?;
    zip::ZipArchive::new(file).context("not an Enclave backup (not a ZIP archive)")
}

fn manifest_of(zip: &mut zip::ZipArchive<File>) -> Result<Manifest> {
    let mut entry = zip.by_name(MANIFEST).context("not an Enclave backup (no manifest)")?;
    let mut bytes = Vec::new();
    entry.read_to_end(&mut bytes)?;
    let manifest: Manifest = serde_json::from_slice(&bytes).context("the backup's manifest is damaged")?;
    check_restorable(&manifest)?;
    Ok(manifest)
}

fn check_restorable(manifest: &Manifest) -> Result<()> {
    ensure!(
        manifest.format == FORMAT,
        "backup format {} is not supported by this version of Enclave (it reads format {FORMAT})",
        manifest.format
    );
    let latest = crate::db::latest_migration();
    ensure!(
        manifest.migration <= latest,
        "this backup was made by a newer Enclave ({}, database version {}); this one knows up to {latest} — update Enclave first",
        manifest.app_version,
        manifest.migration
    );
    for hash in manifest.blobs.iter().map(|b| &b.sha256).chain(&manifest.missing) {
        // Names are joined onto a directory below: nothing but a hash.
        ensure!(is_sha256(hash), "the backup's manifest is damaged (bad file name {hash:?})");
    }
    Ok(())
}

fn is_sha256(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Extract the dump and the files into `dir`, each checked against the
/// manifest.
fn extract_checked(archive: &Path, dir: &Path, progress: &(dyn Fn(Progress) + Send + Sync)) -> Result<Manifest> {
    let mut zip = open_archive(archive)?;
    let manifest = manifest_of(&mut zip)?;

    progress(Progress { stage: "checking", done: 0, total: manifest.blobs.len() });
    extract_entry(&mut zip, DUMP, &dir.join(DUMP), &manifest.database)?;

    let blobs = dir.join("blobs");
    fs::create_dir_all(&blobs)?;
    for (i, blob) in manifest.blobs.iter().enumerate() {
        progress(Progress { stage: "checking", done: i, total: manifest.blobs.len() });
        extract_entry(&mut zip, &format!("{BLOBS}{}", blob.sha256), &blobs.join(&blob.sha256), blob)?;
    }
    Ok(manifest)
}

fn extract_entry(zip: &mut zip::ZipArchive<File>, name: &str, to: &Path, expected: &Entry) -> Result<()> {
    let mut entry = zip.by_name(name).with_context(|| format!("the backup is incomplete: {name} is missing"))?;
    let mut out = File::create(to).with_context(|| format!("could not write {}", to.display()))?;
    let sha256 = copy_hashing(&mut entry, &mut out)?;
    let size = out.metadata()?.len();
    ensure!(
        size == expected.size && sha256 == expected.sha256,
        "the backup is damaged: {name} does not match its checksum"
    );
    Ok(())
}

// ─── Restoring ───────────────────────────────────────────────────────────────

/// Check the archive and prepare it to replace the live data at the next
/// start. Nothing live is touched; a failure leaves nothing behind.
pub async fn stage(
    pg: &Cluster,
    app_data: &Path,
    archive: &Path,
    staged_by: &str,
    progress: OnProgress,
) -> Result<Staged> {
    discard_staged(pg, app_data).await?;
    let result = stage_inner(pg, app_data, archive, staged_by, progress).await;
    if result.is_err() {
        let _ = discard_staged(pg, app_data).await;
    }
    result
}

async fn stage_inner(
    pg: &Cluster,
    app_data: &Path,
    archive: &Path,
    staged_by: &str,
    progress: OnProgress,
) -> Result<Staged> {
    let dir = app_data.join(STAGING_DIR);
    fs::create_dir_all(&dir)?;

    let (archive2, dir2, progress2) = (archive.to_path_buf(), dir.clone(), progress.clone());
    let manifest = tauri::async_runtime::spawn_blocking(move || extract_checked(&archive2, &dir2, &*progress2))
        .await
        .context("restore task failed")??;

    progress(Progress { stage: "database", done: 0, total: 0 });
    let maintenance = sqlx::PgPool::connect(&pg.url("postgres")).await?;
    sqlx::query(&format!("CREATE DATABASE {STAGING_DATABASE}")).execute(&maintenance).await?;
    maintenance.close().await;

    // --no-owner: objects belong to whoever restores, the superuser, as in
    // a database the migrations built. Privileges are kept: they name the
    // app's roles, which every Enclave database has.
    let staging_url = pg.url(STAGING_DATABASE);
    let mut cmd = embedded::tool(&pg.bin, "pg_restore");
    cmd.args(["--no-owner", "--exit-on-error", "--single-transaction"]).arg(dir.join(DUMP));
    // Most of a restore is rebuilding the HNSW index. With the default
    // 64 MB its graph does not fit in memory: 50k chunks took 5.5 min in
    // all, 2.9 min with 512 MB (1 GB and parallel workers gained nothing).
    cmd.env("PGOPTIONS", "-c maintenance_work_mem=512MB");
    run_with_url(cmd, &staging_url, "pg_restore").await?;
    let _ = fs::remove_file(dir.join(DUMP));

    // An older backup is brought up to this build's schema now, so a
    // migration that fails fails here, not on the next start.
    let staged_db = sqlx::PgPool::connect(&staging_url).await?;
    let checked = async {
        crate::db::MIGRATOR.run(&staged_db).await.context("the backup's database could not be migrated")?;
        let live: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT file_hash FROM documents WHERE deleted_at IS NULL")
                .fetch_all(&staged_db)
                .await?;
        check_files(&manifest, &live)
    }
    .await;
    staged_db.close().await;
    checked?;

    let staged = Staged { manifest, staged_at: Utc::now(), staged_by: staged_by.to_string() };
    // Written last: its presence means everything above succeeded.
    fs::write(dir.join(MARKER), serde_json::to_vec_pretty(&staged)?)?;
    info!("Restore staged: {} documents, backup of {}.", staged.manifest.counts.documents, staged.manifest.created_at);
    Ok(staged)
}

/// Every live document of the restored database has its file, unless it
/// was already missing when the backup was made.
fn check_files(manifest: &Manifest, live: &[String]) -> Result<()> {
    let present: HashSet<&str> =
        manifest.blobs.iter().map(|b| b.sha256.as_str()).chain(manifest.missing.iter().map(String::as_str)).collect();
    let absent = live.iter().filter(|h| !present.contains(h.as_str())).count();
    ensure!(absent == 0, "the backup is incomplete: {absent} document file(s) are neither in it nor listed as missing");
    Ok(())
}

/// The restore waiting for the next start, if any.
pub fn staged(app_data: &Path) -> Result<Option<Staged>> {
    let marker = app_data.join(STAGING_DIR).join(MARKER);
    if !marker.is_file() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(&marker)?).context("the staged restore's marker is damaged")?))
}

/// Drop a staged (or half-staged) restore.
pub async fn discard_staged(pg: &Cluster, app_data: &Path) -> Result<()> {
    let maintenance = sqlx::PgPool::connect(&pg.url("postgres")).await?;
    let dropped = sqlx::query(&format!("DROP DATABASE IF EXISTS {STAGING_DATABASE} WITH (FORCE)"))
        .execute(&maintenance)
        .await;
    maintenance.close().await;
    dropped?;
    let dir = app_data.join(STAGING_DIR);
    if dir.exists() {
        fs::remove_dir_all(&dir).with_context(|| format!("could not remove {}", dir.display()))?;
    }
    Ok(())
}

/// At start, before anything connects: swap a staged restore in. Each step
/// can be repeated, so a start interrupted halfway finishes the swap next
/// time — the marker goes last.
pub async fn apply_staged(maintenance: &PgPool, app_data: &Path) -> Result<Option<Staged>> {
    let Some(staged) = staged(app_data)? else {
        // Staging that never completed (the app closed mid-restore).
        let dir = app_data.join(STAGING_DIR);
        if dir.exists() {
            sqlx::query(&format!("DROP DATABASE IF EXISTS {STAGING_DATABASE} WITH (FORCE)")).execute(maintenance).await?;
            let _ = fs::remove_dir_all(&dir);
        }
        return Ok(None);
    };
    let dir = app_data.join(STAGING_DIR);

    let waiting: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
        .bind(STAGING_DATABASE)
        .fetch_one(maintenance)
        .await?;
    if waiting {
        sqlx::query(&format!("DROP DATABASE IF EXISTS {DATABASE} WITH (FORCE)")).execute(maintenance).await?;
        sqlx::query(&format!("ALTER DATABASE {STAGING_DATABASE} RENAME TO {DATABASE}")).execute(maintenance).await?;
    }

    let files = dir.join("blobs");
    if files.is_dir() {
        let blobs = app_data.join("blobs");
        if blobs.exists() {
            fs::remove_dir_all(&blobs).with_context(|| format!("could not remove {}", blobs.display()))?;
        }
        fs::rename(&files, &blobs).with_context(|| format!("could not move {} into place", files.display()))?;
    }

    fs::remove_dir_all(&dir).with_context(|| format!("could not remove {}", dir.display()))?;
    info!("Restored the backup of {} ({} documents).", staged.manifest.created_at, staged.manifest.counts.documents);
    Ok(Some(staged))
}

// ─── PostgreSQL tools ────────────────────────────────────────────────────────

/// Run a PostgreSQL client tool against `url`. The password goes in the
/// tool's environment (PGPASSWORD), not on its command line, where any
/// process of the user could read it.
async fn run_with_url(mut cmd: std::process::Command, url: &str, what: &str) -> Result<()> {
    let (url, password) = split_password(url)?;
    cmd.arg(format!("--dbname={url}"));
    if let Some(password) = password {
        cmd.env("PGPASSWORD", password);
    }
    let out = tokio::process::Command::from(cmd).output().await.with_context(|| format!("could not run {what}"))?;
    if !out.status.success() {
        bail!("{what} failed ({}): {}", out.status, String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

/// `postgres://user:pa%40ss@host/db` → (`postgres://user@host/db`, `pa@ss`).
fn split_password(url: &str) -> Result<(String, Option<String>)> {
    let mut parsed = url::Url::parse(url).context("the database URL is not valid")?;
    let password = parsed
        .password()
        .map(|p| percent_encoding::percent_decode_str(p).decode_utf8().map(|p| p.into_owned()))
        .transpose()
        .context("the database password is not valid UTF-8")?;
    parsed.set_password(None).map_err(|_| anyhow::anyhow!("the database URL has no host"))?;
    Ok((parsed.to_string(), password))
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("enclave-backup-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sha(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    fn manifest() -> Manifest {
        Manifest {
            format: FORMAT,
            app_version: "0.1.0".into(),
            created_at: Utc::now(),
            migration: 1,
            server_version: "18.0".into(),
            counts: Counts { documents: 2, users: 1, departments: 1 },
            database: Entry { size: 0, sha256: String::new() },
            blobs: Vec::new(),
            missing: Vec::new(),
        }
    }

    /// An archive of two files and a dump, as `create` writes it after
    /// pg_dump, with the second file damaged on disk.
    fn archive(dir: &Path) -> (PathBuf, Manifest) {
        let store = dir.join("store");
        fs::create_dir_all(&store).unwrap();
        let (good, bad) = (b"contract".as_slice(), b"invoice".as_slice());
        fs::write(store.join(sha(good)), good).unwrap();
        fs::write(store.join(sha(bad)), b"invoice, altered").unwrap();
        fs::write(dir.join("db.dump"), b"PGDMP fake dump").unwrap();
        let path = dir.join("backup.enclave-backup");
        let mut hashes = vec![sha(good), sha(bad), sha(b"never stored")];
        hashes.sort();
        let m = write_archive(&path, &dir.join("db.dump"), &store, &hashes, manifest(), &|_| {}).unwrap();
        (path, m)
    }

    #[test]
    fn damaged_and_absent_files_are_listed_as_missing_not_archived() {
        let dir = scratch("write");
        let (path, m) = archive(&dir);
        assert_eq!(m.blobs.iter().map(|b| b.sha256.clone()).collect::<Vec<_>>(), vec![sha(b"contract")]);
        let mut missing = m.missing.clone();
        missing.sort();
        let mut expected = vec![sha(b"invoice"), sha(b"never stored")];
        expected.sort();
        assert_eq!(missing, expected);
        assert_eq!(m.database.sha256, sha(b"PGDMP fake dump"));

        // Only the good file is in the archive.
        let zip = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut names: Vec<_> = zip.file_names().map(str::to_string).collect();
        names.sort();
        assert_eq!(names, vec![format!("blobs/{}", sha(b"contract")), DUMP.to_string(), MANIFEST.to_string()]);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_intact_archive_extracts_and_a_tampered_one_is_refused() {
        let dir = scratch("extract");
        let (path, _) = archive(&dir);
        let out = dir.join("out");
        fs::create_dir_all(&out).unwrap();
        extract_checked(&path, &out, &|_| {}).unwrap();
        assert_eq!(fs::read(out.join("blobs").join(sha(b"contract"))).unwrap(), b"contract");

        // Same archive, the file's bytes changed inside it.
        let tampered = dir.join("tampered.enclave-backup");
        let mut src = zip::ZipArchive::new(File::open(&path).unwrap()).unwrap();
        let mut dst = zip::ZipWriter::new(File::create(&tampered).unwrap());
        for i in 0..src.len() {
            let mut e = src.by_index(i).unwrap();
            let name = e.name().to_string();
            let mut bytes = Vec::new();
            e.read_to_end(&mut bytes).unwrap();
            if name.starts_with(BLOBS) {
                bytes = b"contracT".to_vec();
            }
            dst.start_file(name, SimpleFileOptions::default()).unwrap();
            dst.write_all(&bytes).unwrap();
        }
        dst.finish().unwrap();
        let out2 = dir.join("out2");
        fs::create_dir_all(&out2).unwrap();
        let err = extract_checked(&tampered, &out2, &|_| {}).unwrap_err().to_string();
        assert!(err.contains("does not match its checksum"), "{err}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_backup_from_a_newer_build_or_with_odd_names_is_refused() {
        let mut m = manifest();
        m.migration = crate::db::latest_migration() + 1;
        assert!(check_restorable(&m).unwrap_err().to_string().contains("newer Enclave"));

        let mut m = manifest();
        m.missing = vec!["../../evil".into()];
        assert!(check_restorable(&m).is_err());

        let mut m = manifest();
        m.format = FORMAT + 1;
        assert!(check_restorable(&m).is_err());
    }

    #[test]
    fn every_live_document_needs_its_file_or_a_missing_entry() {
        let mut m = manifest();
        m.blobs = vec![Entry { size: 1, sha256: sha(b"a") }];
        m.missing = vec![sha(b"b")];
        assert!(check_files(&m, &[sha(b"a"), sha(b"b")]).is_ok());
        assert!(check_files(&m, &[sha(b"a"), sha(b"c")]).is_err());
    }

    #[test]
    fn the_password_is_taken_out_of_the_url() {
        let (url, pw) = split_password("postgres://postgres:pa%40ss@localhost:5433/enclave?sslmode=disable").unwrap();
        assert_eq!(url, "postgres://postgres@localhost:5433/enclave?sslmode=disable");
        assert_eq!(pw.as_deref(), Some("pa@ss"));
        let (url, pw) = split_password("postgres://app@127.0.0.1/enclave").unwrap();
        assert_eq!(url, "postgres://app@127.0.0.1/enclave");
        assert_eq!(pw, None);
    }
}
