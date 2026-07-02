use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row};
use tauri::State;
use uuid::Uuid;

use crate::AppState;

#[derive(Serialize, Deserialize, FromRow, Clone, Debug)]
pub struct UserInfo {
    pub id:       Uuid,
    pub username: String,
    pub is_admin: bool,
}

fn hash_pin(pin: &str) -> Result<String, String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| e.to_string())
}

fn verify_pin(pin: &str, stored_hash: &str) -> bool {
    PasswordHash::new(stored_hash)
        .ok()
        .map(|h| Argon2::default().verify_password(pin.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

/// List all local user profiles (used by the login screen profile picker).
/// Uses admin_pool: app_user has no current_user_id context at this stage.
#[tauri::command]
pub async fn cmd_list_users(state: State<'_, AppState>) -> Result<Vec<UserInfo>, String> {
    sqlx::query_as::<_, UserInfo>("SELECT id, username, is_admin FROM users ORDER BY username")
        .fetch_all(&state.admin_pool)
        .await
        .map_err(|e| e.to_string())
}

/// Create a new local user profile with a PIN, a default department, and an
/// admin membership — all in a single admin_pool transaction.
#[derive(Deserialize)]
pub struct CreateUserArgs {
    pub username: String,
    pub pin:      String,
}

#[tauri::command]
pub async fn cmd_create_user(
    state: State<'_, AppState>,
    args:  CreateUserArgs,
) -> Result<UserInfo, String> {
    let pin_hash = hash_pin(&args.pin)?;

    let mut tx = state.admin_pool.begin().await.map_err(|e| e.to_string())?;

    // The very first profile created on a fresh install becomes the admin
    // (CLAUDE.md task 11's bootstrap flow); everyone after that is a
    // regular user unless promoted by an existing admin.
    let existing_users: i64 = sqlx::query_scalar("SELECT count(*) FROM users")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    let is_admin = existing_users == 0;

    let user = sqlx::query_as::<_, UserInfo>(
        "INSERT INTO users (username, email, password_hash, is_admin) VALUES ($1, $2, $3, $4) RETURNING id, username, is_admin",
    )
    .bind(&args.username)
    .bind(format!("{}@local", args.username))
    .bind(&pin_hash)
    .bind(is_admin)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    let dept_id: Uuid = sqlx::query_scalar(
        "INSERT INTO departments (name, slug) VALUES ($1, $2) RETURNING id",
    )
    .bind(format!("{}'s Department", args.username))
    .bind(format!("{}-dept", args.username.to_lowercase()))
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "INSERT INTO department_members (user_id, department_id) VALUES ($1, $2)",
    )
    .bind(user.id)
    .bind(dept_id)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;

    Ok(user)
}

/// Verify a PIN and return the user session if valid.
/// Uses admin_pool: reading pin_hash before a user context is established.
#[derive(Deserialize)]
pub struct LoginArgs {
    pub user_id: Uuid,
    pub pin:     String,
}

#[derive(Serialize)]
pub struct LoginResult {
    pub ok:       bool,
    pub user_id:  Option<Uuid>,
    pub username: Option<String>,
    pub is_admin: Option<bool>,
}

#[tauri::command]
pub async fn cmd_login(
    state: State<'_, AppState>,
    args:  LoginArgs,
) -> Result<LoginResult, String> {
    let row = sqlx::query(
        "SELECT id, username, password_hash, is_admin FROM users WHERE id = $1",
    )
    .bind(args.user_id)
    .fetch_optional(&state.admin_pool)
    .await
    .map_err(|e| e.to_string())?;

    let empty = LoginResult { ok: false, user_id: None, username: None, is_admin: None };

    match row {
        None => Ok(empty),
        Some(r) => {
            let stored: String = r.get("password_hash");
            if verify_pin(&args.pin, &stored) {
                Ok(LoginResult {
                    ok:       true,
                    user_id:  Some(r.get("id")),
                    username: Some(r.get("username")),
                    is_admin: Some(r.get("is_admin")),
                })
            } else {
                Ok(empty)
            }
        }
    }
}

/// Clear session — stateless in Rust; the frontend clears its store.
#[tauri::command]
pub async fn cmd_logout() -> Result<(), String> {
    Ok(())
}
