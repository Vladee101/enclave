//! Reset a local profile's PIN (development / recovery tool).
//!
//! PINs are stored only as argon2 hashes, so a forgotten one cannot be
//! recovered — only replaced. This runs outside the app, through the
//! privileged `ADMIN_DATABASE_URL`, the same path provisioning uses.
//!
//!   cargo run --example reset_pin -- <username>
//!
//! The new PIN is read from stdin (it is echoed; run it on your own
//! machine). The reset is recorded in `audit_log`.

use std::io::{self, BufRead, Write};

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, SaltString, rand_core::OsRng},
};
use uuid::Uuid;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let username = std::env::args().nth(1).ok_or_else(|| anyhow::anyhow!("usage: reset_pin <username>"))?;
    let url = std::env::var("ADMIN_DATABASE_URL").map_err(|_| anyhow::anyhow!("ADMIN_DATABASE_URL must be set"))?;
    let pool = sqlx::PgPool::connect(&url).await?;

    let user_id: Option<Uuid> = sqlx::query_scalar("SELECT id FROM users WHERE username = $1")
        .bind(&username)
        .fetch_optional(&pool)
        .await?;
    let Some(user_id) = user_id else {
        let names: Vec<String> = sqlx::query_scalar("SELECT username FROM users ORDER BY username")
            .fetch_all(&pool)
            .await?;
        anyhow::bail!("no profile named {username:?}; existing profiles: {names:?}");
    };

    let pin = read_line("New PIN: ")?;
    anyhow::ensure!(!pin.is_empty(), "PIN must not be empty");
    anyhow::ensure!(read_line("Repeat PIN: ")? == pin, "PINs do not match; nothing changed");

    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string();

    let mut tx = pool.begin().await?;
    sqlx::query("UPDATE users SET password_hash = $1 WHERE id = $2")
        .bind(&hash)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO audit_log (user_id, event_type, payload) VALUES ($1, 'pin_reset', $2)")
        .bind(user_id)
        .bind(serde_json::json!({ "via": "examples/reset_pin" }))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    println!("PIN for {username} reset. Sign in with the new PIN.");
    Ok(())
}

fn read_line(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}
