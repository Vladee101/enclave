//! The office server over the network (ADR-0031): a real server on a free
//! port, real clients pinned to its certificate, the database under it.
//!
//! What it proves is the ADR's security claim — a client cannot be anyone
//! but the user whose PIN it gave:
//! - no token, no data; a token shows its own user's documents and chats
//!   only, whatever the request says;
//! - a token stops working at sign-out;
//! - only listed commands exist, and a profile is created over the network
//!   by an administrator only;
//! - a client pinned to another certificate refuses the server;
//! - wrong PINs are throttled, the right PIN included, and recorded.
//!
//! Like `rls_validation`: TEST_ADMIN_URL / TEST_APP_URL, a dedicated
//! database recreated each run; skipped without them unless
//! ENCLAVE_REQUIRE_DB_TESTS is set.

use enclave_lib::{
    commands::auth::{self, CreateUserArgs},
    office::{client::Remote, server, tls::Identity},
    session::{Caller, FREE_FAILURES},
    Core,
};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

const TEST_DB_NAME: &str = "enclave_office_test";

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

fn code(e: &Value) -> &str {
    e.get("code").and_then(Value::as_str).unwrap_or("<no code>")
}

fn titles(docs: &Value) -> Vec<String> {
    let mut t: Vec<String> = docs.as_array().unwrap().iter().map(|d| d["filename"].as_str().unwrap().to_string()).collect();
    t.sort();
    t
}

async fn sign_in(remote: &Remote, user: Uuid, pin: &str) -> Value {
    remote.login(&json!({ "user_id": user, "pin": pin })).await.expect("login answers")
}

#[tokio::test]
async fn office_server_keeps_each_client_to_its_own_user() -> Result<(), Box<dyn std::error::Error>> {
    let (Ok(admin_url), Ok(app_url)) = (std::env::var("TEST_ADMIN_URL"), std::env::var("TEST_APP_URL")) else {
        if std::env::var_os("ENCLAVE_REQUIRE_DB_TESTS").is_some() {
            panic!("ENCLAVE_REQUIRE_DB_TESTS is set but TEST_ADMIN_URL / TEST_APP_URL are not — office_server cannot run");
        }
        eprintln!("TEST_ADMIN_URL / TEST_APP_URL not set — skipping office_server.");
        return Ok(());
    };

    let root = PgPool::connect(&with_database(&admin_url, "postgres")).await?;
    sqlx::query(&format!("DROP DATABASE IF EXISTS {TEST_DB_NAME} WITH (FORCE)")).execute(&root).await?;
    sqlx::query(&format!("CREATE DATABASE {TEST_DB_NAME}")).execute(&root).await?;
    root.close().await;
    let admin_pool = PgPool::connect(&with_database(&admin_url, TEST_DB_NAME)).await?;
    sqlx::migrate!("../migrations").run(&admin_pool).await?;
    let app_pool = PgPool::connect(&with_database(&app_url, TEST_DB_NAME)).await?;

    let blobs = std::env::temp_dir().join(format!("enclave-office-blobs-{}", Uuid::new_v4()));
    let core = Core {
        app_pool,
        admin_pool: admin_pool.clone(),
        llm: None,
        blob_root: blobs.clone(),
        logins: Default::default(),
    };

    // Profiles as the login screen makes them: the first is the admin.
    let mk = |name: &str, pin: &str| CreateUserArgs { username: name.into(), pin: pin.into() };
    let boss = auth::create_user(&core, &Caller(None), mk("boss", "9999")).await?;
    let alice = auth::create_user(&core, &Caller(None), mk("alice", "1111")).await?;
    let bob = auth::create_user(&core, &Caller(None), mk("bob", "2222")).await?;
    assert!(boss.is_admin && !alice.is_admin && !bob.is_admin);

    let hr: Uuid = sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('HR', 'hr') RETURNING id")
        .fetch_one(&admin_pool).await?;
    let sales: Uuid = sqlx::query_scalar("INSERT INTO departments (name, slug) VALUES ('Sales', 'sales') RETURNING id")
        .fetch_one(&admin_pool).await?;
    for (user, dept, title) in [(alice.id, hr, "salaries.xlsx"), (bob.id, sales, "price-list.pdf")] {
        sqlx::query("INSERT INTO department_members (user_id, department_id) VALUES ($1, $2)")
            .bind(user).bind(dept).execute(&admin_pool).await?;
        sqlx::query(
            "INSERT INTO documents (department_id, title, file_hash, mime_type, byte_size, status, uploaded_by) \
             VALUES ($1, $2, $3, 'application/octet-stream', 1, 'ready', $4)",
        )
        .bind(dept).bind(title).bind(format!("hash-{title}")).bind(user)
        .execute(&admin_pool).await?;
    }
    let alices_chat: Uuid = sqlx::query_scalar("INSERT INTO chats (user_id, title) VALUES ($1, 'Зарплаты') RETURNING id")
        .bind(alice.id).fetch_one(&admin_pool).await?;

    let identity = Identity::generate()?;
    let running = server::start(core.clone(), &identity, "127.0.0.1:0".parse()?).await?;
    let address = format!("https://127.0.0.1:{}", running.addr.port());

    // 1. No token: the login screen's list, nothing else.
    let anyone = Remote::new(&address, &running.fingerprint)?;
    let users = anyone.call("cmd_list_users", &json!({})).await.expect("the profile list is public");
    assert_eq!(users.as_array().unwrap().len(), 3);
    let e = anyone.call("cmd_list_documents", &json!({})).await.unwrap_err();
    assert_eq!(code(&e), "not_signed_in");
    assert_eq!(anyone.current_session().await.unwrap(), Value::Null);

    // 2. Bob signs in and sees Sales only — asking as Alice changes nothing.
    let bobs = Remote::new(&address, &running.fingerprint)?;
    assert_eq!(sign_in(&bobs, bob.id, "2222").await["ok"], true);
    assert_eq!(bobs.current_session().await.unwrap()["username"], "bob");
    assert_eq!(titles(&bobs.call("cmd_list_documents", &json!({})).await.unwrap()), ["price-list.pdf"]);
    let forged = bobs.call("cmd_list_documents", &json!({ "user_id": alice.id, "userId": alice.id })).await.unwrap();
    assert_eq!(titles(&forged), ["price-list.pdf"]);
    let e = bobs.call("cmd_get_conversation", &json!({ "conversationId": alices_chat })).await.unwrap_err();
    assert_eq!(code(&e), "conversation_not_found");
    let e = bobs.call("cmd_delete_conversation", &json!({ "conversationId": alices_chat })).await.unwrap_err();
    assert_eq!(code(&e), "conversation_not_found");

    // Alice, on her own client at the same time, sees HR and her chat.
    let alices = Remote::new(&address, &running.fingerprint)?;
    assert_eq!(sign_in(&alices, alice.id, "1111").await["ok"], true);
    assert_eq!(titles(&alices.call("cmd_list_documents", &json!({})).await.unwrap()), ["salaries.xlsx"]);
    let chats = alices.call("cmd_list_conversations", &json!({})).await.unwrap();
    assert_eq!(chats.as_array().unwrap().len(), 1);
    assert!(bobs.call("cmd_list_conversations", &json!({})).await.unwrap().as_array().unwrap().is_empty());

    // 3. Only listed commands; administration stays an administrator's.
    let e = bobs.call("cmd_backup_create", &json!({ "path": "C:\\x.zip" })).await.unwrap_err();
    assert_eq!(code(&e), "unknown_command");
    let e = bobs.call("cmd_list_departments", &json!({})).await.unwrap_err();
    assert_eq!(code(&e), "admin_required");
    let e = anyone.call("cmd_create_user", &json!({ "args": { "username": "mallory", "pin": "0000" } })).await.unwrap_err();
    assert_eq!(code(&e), "not_signed_in", "nobody signed in may not create a profile over the network");
    let e = bobs.call("cmd_create_user", &json!({ "args": { "username": "mallory", "pin": "0000" } })).await.unwrap_err();
    assert_eq!(code(&e), "admin_required");
    let bosses = Remote::new(&address, &running.fingerprint)?;
    assert_eq!(sign_in(&bosses, boss.id, "9999").await["ok"], true);
    let carol = bosses.call("cmd_create_user", &json!({ "args": { "username": "carol", "pin": "3333" } })).await.unwrap();
    assert_eq!(carol["is_admin"], false);

    // 4. A question with no model streams back its error, coded.
    let e = bobs.ask(&json!({ "requestId": "r1", "args": { "query": "Сколько стоит?" } }), |_| {}).await.unwrap_err();
    assert_eq!(code(&e), "model_unavailable");
    let e = anyone.ask(&json!({ "requestId": "r2", "args": { "query": "?" } }), |_| {}).await.unwrap_err();
    assert_eq!(code(&e), "not_signed_in");

    // 5. Sign-out ends the token on the server, not just on the client.
    let token_holder = Remote::new(&address, &running.fingerprint)?;
    sign_in(&token_holder, bob.id, "2222").await;
    bobs.logout().await.unwrap();
    let e = bobs.call("cmd_list_documents", &json!({})).await.unwrap_err();
    assert_eq!(code(&e), "not_signed_in");
    assert!(token_holder.call("cmd_list_documents", &json!({})).await.is_ok(), "another session of Bob's stays");

    // 6. A client pinned to another certificate does not talk to this server.
    let other = Identity::generate()?;
    let misled = Remote::new(&address, &other.fingerprint())?;
    let e = misled.call("cmd_list_users", &json!({})).await.unwrap_err();
    assert_eq!(code(&e), "server_identity_changed");

    // 7. Wrong PINs: free up to the limit, then paused — the right PIN too.
    let guesser = Remote::new(&address, &running.fingerprint)?;
    for _ in 0..FREE_FAILURES {
        assert_eq!(sign_in(&guesser, alice.id, "0000").await["ok"], false);
    }
    let e = guesser.login(&json!({ "user_id": alice.id, "pin": "1111" })).await.unwrap_err();
    assert_eq!(code(&e), "login_throttled");
    let failed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM audit_log WHERE event_type = 'login_failed' AND user_id = $1 AND payload->>'from' = '127.0.0.1'",
    )
    .bind(alice.id)
    .fetch_one(&admin_pool)
    .await?;
    assert_eq!(failed, FREE_FAILURES as i64);

    running.task.abort();
    let _ = std::fs::remove_dir_all(&blobs);
    Ok(())
}
