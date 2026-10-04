use crate::error::AppError;
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Row};
use tauri::State;
use uuid::Uuid;

use crate::{
    Core,
    audit::{self, event},
    session::{Caller, Session, SessionUser},
};

#[derive(Serialize, Deserialize, FromRow, Clone, Debug)]
pub struct UserInfo {
    pub id:       Uuid,
    pub username: String,
    pub is_admin: bool,
}

fn hash_pin(pin: &str) -> Result<String, AppError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pin.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(AppError::of)
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
pub async fn cmd_list_users(core: State<'_, Core>) -> Result<Vec<UserInfo>, AppError> {
    list_users(&core).await
}

pub async fn list_users(core: &Core) -> Result<Vec<UserInfo>, AppError> {
    sqlx::query_as::<_, UserInfo>("SELECT id, username, is_admin FROM users ORDER BY username")
        .fetch_all(&core.admin_pool)
        .await
        .map_err(AppError::of)
}

/// Create a new local user profile with a PIN and membership in the shared
/// default department — all in a single admin_pool transaction.
#[derive(Deserialize)]
pub struct CreateUserArgs {
    pub username: String,
    pub pin:      String,
}

#[tauri::command]
pub async fn cmd_create_user(
    core:    State<'_, Core>,
    session: State<'_, Session>,
    args:    CreateUserArgs,
) -> Result<UserInfo, AppError> {
    create_user(&core, &session.caller(), args).await
}

pub async fn create_user(core: &Core, caller: &Caller, args: CreateUserArgs) -> Result<UserInfo, AppError> {
    let pin_hash = hash_pin(&args.pin)?;

    let mut tx = core.admin_pool.begin().await.map_err(AppError::of)?;

    // Bootstrap: whoever creates a profile while no admin exists yet becomes
    // the admin (CLAUDE.md task 11). Deliberately "no admin exists" rather
    // than "the users table is empty" — the latter only ever fires on a
    // truly fresh database and permanently never fires again once any user
    // row exists (which is exactly what happened here: `is_admin` was added
    // by a later migration onto a users table that already had rows, so
    // nobody could ever become admin under the empty-table check).
    let existing_admins: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE is_admin = true")
        .fetch_one(&mut *tx)
        .await
        .map_err(AppError::of)?;
    let is_admin = existing_admins == 0;

    let user = sqlx::query_as::<_, UserInfo>(
        "INSERT INTO users (username, email, password_hash, is_admin) VALUES ($1, $2, $3, $4) RETURNING id, username, is_admin",
    )
    .bind(&args.username)
    .bind(format!("{}@local", args.username))
    .bind(&pin_hash)
    .bind(is_admin)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| match e.as_database_error().and_then(|d| d.code()) {
        Some(code) if code == "23505" => {
            AppError::new("username_taken", format!("A profile named {} already exists.", args.username))
                .with("name", &args.username)
        }
        _ => AppError::of(e),
    })?;

    // The shared default department (migrations/010, ADR-0016) — the only
    // membership a new profile gets. Departments are access grants, and
    // this command runs from the login screen unauthenticated, so any
    // other membership comes from an administrator (cmd_add_member).
    let dept_id: Uuid = sqlx::query_scalar("SELECT id FROM departments WHERE is_default")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| format!("default department missing: {e}"))?;

    sqlx::query(
        "INSERT INTO department_members (user_id, department_id) VALUES ($1, $2)",
    )
    .bind(user.id)
    .bind(dept_id)
    .execute(&mut *tx)
    .await
    .map_err(AppError::of)?;

    // Profiles are created from the login screen, usually with nobody
    // signed in — then the new user is recorded as creating themselves.
    let actor = caller.get().map(|u| u.id).unwrap_or(user.id);
    audit::record(
        &mut tx,
        Some(actor),
        Some(dept_id),
        event::USER_CREATED,
        serde_json::json!({ "user_id": user.id, "username": user.username, "is_admin": user.is_admin }),
    )
    .await
    .map_err(AppError::of)?;

    tx.commit().await.map_err(AppError::of)?;

    Ok(user)
}

/// Verify a PIN and return the user session if valid.
/// Uses admin_pool: reading pin_hash before a user context is established.
#[derive(Deserialize)]
pub struct LoginArgs {
    pub user_id: Uuid,
    pub pin:     String,
}

#[derive(Serialize, Deserialize)]
pub struct LoginResult {
    pub ok:       bool,
    pub user_id:  Option<Uuid>,
    pub username: Option<String>,
    pub is_admin: Option<bool>,
}

impl LoginResult {
    pub fn of(user: Option<&SessionUser>) -> Self {
        match user {
            Some(u) => Self { ok: true, user_id: Some(u.id), username: Some(u.username.clone()), is_admin: Some(u.is_admin) },
            None => Self { ok: false, user_id: None, username: None, is_admin: None },
        }
    }
}

#[tauri::command]
pub async fn cmd_login(
    core:    State<'_, Core>,
    session: State<'_, Session>,
    args:    LoginArgs,
) -> Result<LoginResult, AppError> {
    // A failed attempt on any profile ends the previous session: the
    // screen is being handed to someone else.
    session.clear();
    let user = login(&core, &args, "window").await?;
    if let Some(u) = &user {
        session.set(u.clone());
    }
    Ok(LoginResult::of(user.as_ref()))
}

/// Check a PIN: the user on success, `None` on a wrong PIN or an unknown
/// profile. `from` names where the attempt came from — "window", or the
/// client's address on the office server — for the attempt limit
/// (`LoginGuard`, ADR-0031), which counts per profile and per origin.
pub async fn login(core: &Core, args: &LoginArgs, from: &str) -> Result<Option<SessionUser>, AppError> {
    let keys = [format!("user:{}", args.user_id), format!("from:{from}")];
    core.logins.check(&keys, std::time::Instant::now())?;

    let row = sqlx::query(
        "SELECT id, username, password_hash, is_admin FROM users WHERE id = $1",
    )
    .bind(args.user_id)
    .fetch_optional(&core.admin_pool)
    .await
    .map_err(AppError::of)?;

    let Some(r) = row else {
        core.logins.failed(&keys, std::time::Instant::now());
        return Ok(None);
    };

    let stored: String = r.get("password_hash");
    if !verify_pin(&args.pin, &stored) {
        core.logins.failed(&keys, std::time::Instant::now());
        record_on_admin_pool(core, Some(args.user_id), event::LOGIN_FAILED, serde_json::json!({ "from": from })).await?;
        return Ok(None);
    }
    core.logins.succeeded(&keys);

    let user = SessionUser {
        id:       r.get("id"),
        username: r.get("username"),
        is_admin: r.get("is_admin"),
    };
    record_on_admin_pool(core, Some(user.id), event::LOGIN, serde_json::json!({ "from": from })).await?;
    Ok(Some(user))
}

/// End the session in the core. Every user-scoped command fails with
/// "Not signed in." from here until the next successful `cmd_login`.
#[tauri::command]
pub async fn cmd_logout(
    core:    State<'_, Core>,
    session: State<'_, Session>,
) -> Result<(), AppError> {
    if let Some(user) = session.get() {
        logout(&core, &user).await?;
    }
    session.clear();
    Ok(())
}

pub async fn logout(core: &Core, user: &SessionUser) -> Result<(), AppError> {
    record_on_admin_pool(core, Some(user.id), event::LOGOUT, serde_json::json!({})).await
}

/// Who the core considers signed in. The frontend restores its state from
/// this on startup instead of keeping its own copy, so the UI can never
/// show a user the core does not have a session for.
#[tauri::command]
pub async fn cmd_current_session(session: State<'_, Session>) -> Result<Option<SessionUser>, AppError> {
    Ok(session.get())
}

/// Login/logout run before or after a user context exists, so their audit
/// rows go through admin_pool — the same pool that verifies the PIN.
async fn record_on_admin_pool(core: &Core, user_id: Option<Uuid>, event_type: &str, payload: serde_json::Value) -> Result<(), AppError> {
    let mut conn = core.admin_pool.acquire().await.map_err(AppError::of)?;
    audit::record(&mut conn, user_id, None, event_type, payload)
        .await
        .map_err(AppError::of)
}
