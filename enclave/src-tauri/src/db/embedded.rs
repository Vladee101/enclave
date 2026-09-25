//! The app's own PostgreSQL (ADR-0014).
//!
//! When no `*_DATABASE_URL` is set, the app runs the trimmed PostgreSQL 18 +
//! pgvector laid out by `scripts/fetch-postgres.ps1`:
//!
//! 1. First run: generated passwords (saved DPAPI-encrypted in the user's
//!    profile), `initdb` of `{app_data}/pgdata` — scram-sha-256, builtin
//!    C.UTF-8 locale, so `lower()` handles Cyrillic the same on any Windows.
//! 2. Every run: a server left over from a crash is stopped cleanly, then
//!    `postgres.exe` is started directly, without a console, listening on
//!    127.0.0.1 only, on a free port. Not `pg_ctl start`: a server started
//!    that way from a console died (backends failing with 0xC0000142) once
//!    the console went away — measured while prototyping.
//! 3. After migrations, `secure_roles` replaces the fixed development
//!    password migration 004 gives `enclave_app` and `ingest_worker`.
//! 4. On exit, `stop`: `pg_ctl stop -m fast` (a checkpoint, ~2 s), not a
//!    kill, so the next start needs no crash recovery.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::net::{Ipv4Addr, SocketAddrV4, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};

/// The database the app's roles connect to.
const DATABASE: &str = "enclave";

/// How long a starting server gets before startup is abandoned.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// Seconds `pg_ctl stop` waits for a clean shutdown.
const STOP_TIMEOUT_SECS: &str = "20";

/// Connection strings for the three roles (the same three the *_URL env
/// vars name in server mode).
pub struct Urls {
    pub admin:  String,
    pub app:    String,
    pub ingest: String,
}

/// Passwords generated on first run. Never logged, never in a URL that is
/// logged.
#[derive(Serialize, Deserialize)]
struct Secrets {
    superuser: String,
    app:       String,
    ingest:    String,
}

/// A running embedded server. `stop` must be called on exit.
pub struct EmbeddedPostgres {
    bin:     PathBuf,
    data:    PathBuf,
    secrets: Secrets,
    child:   Mutex<Option<Child>>,
    pub urls: Urls,
}

/// 24 random bytes as hex: URL-safe, no quoting anywhere.
fn generate_password() -> Result<String> {
    let mut bytes = [0u8; 24];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no secure random source: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// The directory holding `bin/postgres.exe`: `ENCLAVE_PG_DIR`, the bundled
/// resources, or (dev builds) `src-tauri/binaries/pg` — the same order as
/// `llm::llama_server_exe`.
fn pg_dir(app: &AppHandle) -> Result<PathBuf> {
    let exe = if cfg!(windows) { "postgres.exe" } else { "postgres" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("ENCLAVE_PG_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join("pg"));
    }
    if cfg!(debug_assertions) {
        candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries").join("pg"));
    }
    candidates
        .iter()
        .find(|dir| dir.join("bin").join(exe).is_file())
        .cloned()
        .with_context(|| {
            format!(
                "embedded PostgreSQL not found in {candidates:?} — run scripts/fetch-postgres.ps1, \
                 or set ADMIN_DATABASE_URL / APP_DATABASE_URL / INGEST_DATABASE_URL to use an existing server"
            )
        })
}

/// A command for a PostgreSQL tool that opens no console window.
fn tool(bin: &Path, name: &str) -> Command {
    let exe = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    #[allow(unused_mut)]
    let mut cmd = Command::new(bin.join(exe));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.stdin(Stdio::null());
    cmd
}

/// Run a tool to completion; its output goes into the error if it fails.
fn run_tool(mut cmd: Command, what: &str) -> Result<()> {
    let out = cmd.output().with_context(|| format!("could not run {what}"))?;
    if !out.status.success() {
        bail!(
            "{what} failed ({}): {}{}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim(),
            String::from_utf8_lossy(&out.stdout).trim()
        );
    }
    Ok(())
}

/// A port nobody listens on right now. Chosen per start, not fixed: a fixed
/// port collides with another PostgreSQL (the user's own, or a second copy
/// of the app for another Windows user).
fn free_port() -> Result<u16> {
    let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

// ─── Secrets at rest ─────────────────────────────────────────────────────────

/// DPAPI, current-user scope: only this Windows account on this machine can
/// decrypt the file.
#[cfg(windows)]
mod protect {
    use anyhow::{bail, Result};
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
    };

    fn call(data: &[u8], encrypt: bool) -> Result<Vec<u8>> {
        let input = CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB { cbData: 0, pbData: std::ptr::null_mut() };
        // SAFETY: input points at `data` for the duration of the call; on
        // success DPAPI allocates output with LocalAlloc, copied then freed.
        let ok = unsafe {
            if encrypt {
                CryptProtectData(&input, std::ptr::null(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            } else {
                CryptUnprotectData(&input, std::ptr::null_mut(), std::ptr::null(), std::ptr::null(), std::ptr::null(), CRYPTPROTECT_UI_FORBIDDEN, &mut output)
            }
        };
        if ok == 0 {
            bail!("DPAPI {} failed: {}", if encrypt { "encryption" } else { "decryption" }, std::io::Error::last_os_error());
        }
        // SAFETY: DPAPI reported success, so output is a valid allocation of cbData bytes.
        let bytes = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize).to_vec() };
        unsafe { LocalFree(output.pbData as _) };
        Ok(bytes)
    }

    pub fn seal(data: &[u8]) -> Result<Vec<u8>> {
        call(data, true)
    }

    pub fn open(data: &[u8]) -> Result<Vec<u8>> {
        call(data, false)
    }
}

/// Elsewhere (the app ships for Windows only so far, ADR-0014) the file is
/// stored as is, readable by its owner only.
#[cfg(not(windows))]
mod protect {
    pub fn seal(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }
    pub fn open(data: &[u8]) -> anyhow::Result<Vec<u8>> {
        Ok(data.to_vec())
    }
}

fn save_secrets(path: &Path, secrets: &Secrets) -> Result<()> {
    let sealed = protect::seal(&serde_json::to_vec(secrets)?)?;
    fs::write(path, sealed).with_context(|| format!("could not write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

fn load_secrets(path: &Path) -> Result<Secrets> {
    let sealed = fs::read(path).with_context(|| format!("could not read {}", path.display()))?;
    Ok(serde_json::from_slice(&protect::open(&sealed)?)?)
}

// ─── Lifecycle ───────────────────────────────────────────────────────────────

impl EmbeddedPostgres {
    /// Initialise on first run, then start the server and wait until it
    /// accepts connections with the generated superuser password.
    pub async fn start(app: &AppHandle) -> Result<Self> {
        let root = app.path().app_data_dir().context("no app data directory")?;
        let bin = pg_dir(app)?.join("bin");
        let data = root.join("pgdata");
        let secrets_path = root.join("db-secrets.bin");
        let log_path = root.join("postgres.log");
        fs::create_dir_all(&root)?;

        let initialised = data.join("PG_VERSION").is_file();
        let secrets = if secrets_path.is_file() {
            load_secrets(&secrets_path)?
        } else if initialised {
            bail!(
                "{} exists but its passwords ({}) are missing — the database cannot be opened. \
                 Restore the file, or move the data directory away to start with an empty database.",
                data.display(),
                secrets_path.display()
            );
        } else {
            let s = Secrets { superuser: generate_password()?, app: generate_password()?, ingest: generate_password()? };
            save_secrets(&secrets_path, &s)?;
            s
        };

        let (bin2, data2, pw) = (bin.clone(), data.clone(), secrets.superuser.clone());
        let child = tauri::async_runtime::spawn_blocking(move || -> Result<(Child, u16)> {
            if !initialised {
                init_cluster(&bin2, &data2, &pw)?;
            }
            stop_leftover(&bin2, &data2);
            let port = free_port()?;
            Ok((spawn_server(&bin2, &data2, port, &log_path)?, port))
        })
        .await
        .context("PostgreSQL start task failed")??;
        let (mut child, port) = child;

        let url = |user: &str, password: &str, db: &str| format!("postgres://{user}:{password}@127.0.0.1:{port}/{db}");
        wait_ready(&mut child, port, &url("postgres", &secrets.superuser, "postgres")).await?;

        // The database itself, on first run (CREATE DATABASE cannot run in
        // a transaction, so not in a migration).
        let maintenance = sqlx::PgPool::connect(&url("postgres", &secrets.superuser, "postgres")).await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = $1)")
            .bind(DATABASE)
            .fetch_one(&maintenance)
            .await?;
        if !exists {
            sqlx::query(&format!("CREATE DATABASE {DATABASE}")).execute(&maintenance).await?;
            info!("Embedded PostgreSQL: database {DATABASE} created.");
        }
        maintenance.close().await;

        let urls = Urls {
            admin:  url("postgres", &secrets.superuser, DATABASE),
            app:    url("enclave_app", &secrets.app, DATABASE),
            ingest: url("ingest_worker", &secrets.ingest, DATABASE),
        };
        info!("Embedded PostgreSQL ready on 127.0.0.1:{port}.");
        Ok(Self { bin, data, secrets, child: Mutex::new(Some(child)), urls })
    }

    /// Give the login roles their generated passwords. Migration 004
    /// creates them with a fixed development password (migrations are
    /// immutable, ADR-0001); run after every migration, it is idempotent.
    /// ALTER ROLE takes no bind parameters, so the statement is built by
    /// the server's own `format(%I, %L)` quoting.
    pub async fn secure_roles(&self, admin: &sqlx::PgPool) -> Result<()> {
        for (role, password) in [("enclave_app", &self.secrets.app), ("ingest_worker", &self.secrets.ingest)] {
            let statement: String = sqlx::query_scalar("SELECT format('ALTER ROLE %I WITH LOGIN PASSWORD %L', $1::text, $2::text)")
                .bind(role)
                .bind(password)
                .fetch_one(admin)
                .await?;
            sqlx::query(&statement).execute(admin).await.with_context(|| format!("could not set the password of {role}"))?;
        }
        Ok(())
    }

    /// Clean shutdown (a checkpoint, then exit); the process is killed only
    /// if that fails.
    pub fn stop(&self) {
        let mut cmd = tool(&self.bin, "pg_ctl");
        cmd.arg("stop").arg("-D").arg(&self.data).args(["-m", "fast", "-w", "-t", STOP_TIMEOUT_SECS]);
        match run_tool(cmd, "pg_ctl stop") {
            Ok(()) => info!("Embedded PostgreSQL stopped."),
            Err(e) => {
                warn!("Clean PostgreSQL shutdown failed, killing it: {e:#}");
                if let Some(child) = self.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
                    let _ = child.kill();
                }
            }
        }
        let _ = self.child.lock().unwrap_or_else(|e| e.into_inner()).take().map(|mut c| c.wait());
    }
}

fn init_cluster(bin: &Path, data: &Path, superuser_password: &str) -> Result<()> {
    info!("Embedded PostgreSQL: first run, initialising {} (takes ~15 s)…", data.display());
    let started = Instant::now();
    // initdb reads the password from a file; it lives next to the data
    // directory only while initdb runs.
    let pwfile = data.with_file_name("initdb-pw.tmp");
    fs::write(&pwfile, superuser_password)?;
    let mut cmd = tool(bin, "initdb");
    cmd.arg("-D")
        .arg(data)
        .args(["-U", "postgres", "-E", "UTF8", "--auth=scram-sha-256"])
        .args(["--locale-provider=builtin", "--builtin-locale=C.UTF-8"])
        .arg(format!("--pwfile={}", pwfile.display()));
    let result = run_tool(cmd, "initdb");
    let _ = fs::remove_file(&pwfile);
    if result.is_err() {
        // A half-made data directory would be taken for a real one next time.
        let _ = fs::remove_dir_all(data);
    }
    result?;
    info!("Embedded PostgreSQL initialised in {:.1} s.", started.elapsed().as_secs_f64());
    Ok(())
}

/// A server from a previous run that was not stopped (the app crashed or
/// was killed) still owns the data directory; stop it cleanly. With no
/// server running pg_ctl just says so — that is fine too.
fn stop_leftover(bin: &Path, data: &Path) {
    if !data.join("postmaster.pid").is_file() {
        return;
    }
    let mut cmd = tool(bin, "pg_ctl");
    cmd.arg("stop").arg("-D").arg(data).args(["-m", "fast", "-w", "-t", STOP_TIMEOUT_SECS]);
    if run_tool(cmd, "pg_ctl stop").is_ok() {
        info!("Embedded PostgreSQL: stopped a server left from a previous run.");
    }
}

fn spawn_server(bin: &Path, data: &Path, port: u16, log_path: &Path) -> Result<Child> {
    // The server writes its log to stderr; a file, rewritten each start,
    // keeps the last session for diagnosis.
    let log = fs::File::create(log_path)?;
    let mut cmd = tool(bin, "postgres");
    cmd.arg("-D")
        .arg(data)
        .args(["-c", "listen_addresses=127.0.0.1", "-p", &port.to_string()])
        .stdout(Stdio::null())
        .stderr(log);
    cmd.spawn().context("could not start postgres")
}

async fn wait_ready(child: &mut Child, port: u16, superuser_url: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("PostgreSQL exited during startup ({status}); see postgres.log in the app data directory");
        }
        if TcpStream::connect_timeout(&SocketAddrV4::new(Ipv4Addr::LOCALHOST, port).into(), Duration::from_millis(200)).is_ok()
            && sqlx::PgPool::connect(superuser_url).await.is_ok()
        {
            return Ok(());
        }
        if started.elapsed() > START_TIMEOUT {
            let _ = child.kill();
            bail!("PostgreSQL did not accept connections within {} s", START_TIMEOUT.as_secs());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_are_long_random_and_url_safe() {
        let a = generate_password().unwrap();
        let b = generate_password().unwrap();
        assert_eq!(a.len(), 48);
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn secrets_survive_a_round_trip_through_protection() {
        let dir = std::env::temp_dir().join(format!("enclave-secrets-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("db-secrets.bin");
        let s = Secrets { superuser: "a".repeat(48), app: "b".repeat(48), ingest: "c".repeat(48) };
        save_secrets(&path, &s).unwrap();
        let raw = fs::read(&path).unwrap();
        if cfg!(windows) {
            // DPAPI: the passwords are not in the file in the clear.
            assert!(!String::from_utf8_lossy(&raw).contains(&"a".repeat(48)));
        }
        let back = load_secrets(&path).unwrap();
        assert_eq!((back.superuser, back.app, back.ingest), (s.superuser, s.app, s.ingest));
        let _ = fs::remove_dir_all(&dir);
    }
}
