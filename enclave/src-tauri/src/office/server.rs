//! The office server's API (ADR-0031): the window's commands over HTTPS.
//!
//! - `GET  /api/v1/hello` — the build, before anyone signs in.
//! - `POST /api/v1/login` — a PIN for a token; attempts are limited per
//!   profile and per address (`LoginGuard`).
//! - `POST /api/v1/logout` — the token stops working.
//! - `POST /api/v1/call/{cmd}` — one command, its arguments exactly as the
//!   window passes them to `invoke`.
//! - `POST /api/v1/query` — a question, answered as server-sent events:
//!   `token` pieces, then one `result` or `error`.
//!
//! The caller is whoever the bearer token was issued to, never anything
//! in the request body: the same rule as the window's `Session`, and the
//! reason a client needs no database credentials. Only commands listed in
//! `dispatch` exist here — backups, model downloads and adapter files are
//! the server PC's own business.

use anyhow::{Context, Result};
use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{
        sse::{Event, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tracing::{info, warn};

use super::{tls::Identity, Hello};
use crate::{
    commands::{admin, auth, chat, documents, query},
    error::AppError,
    session::{Caller, SessionUser},
    Core,
};

/// A token unused this long stops working (ADR-0031).
pub const IDLE_LIMIT: Duration = Duration::from_secs(12 * 60 * 60);

/// The largest request: an upload's bytes travel as a JSON array of
/// numbers, as they do over `invoke`, so up to ~4 bytes per byte of file.
const BODY_LIMIT: usize = 400 * 1024 * 1024;

/// Signed-in clients: token → user. In memory, so a server restart signs
/// everyone out, as the ADR says.
#[derive(Default)]
pub struct Tokens(Mutex<HashMap<String, Entry>>);

struct Entry {
    user:      SessionUser,
    last_used: Instant,
}

impl Tokens {
    pub fn issue(&self, user: SessionUser, now: Instant) -> String {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).expect("the OS random source");
        let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.retain(|_, e| now.duration_since(e.last_used) < IDLE_LIMIT);
        map.insert(token.clone(), Entry { user, last_used: now });
        token
    }

    /// The token's user, if it is live; using it keeps it live.
    pub fn caller(&self, token: Option<&str>, now: Instant) -> Caller {
        let Some(token) = token else { return Caller(None) };
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        match map.get_mut(token) {
            Some(e) if now.duration_since(e.last_used) < IDLE_LIMIT => {
                e.last_used = now;
                Caller(Some(e.user.clone()))
            }
            Some(_) => {
                map.remove(token);
                Caller(None)
            }
            None => Caller(None),
        }
    }

    pub fn revoke(&self, token: &str) -> Option<SessionUser> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).remove(token).map(|e| e.user)
    }
}

#[derive(Clone)]
struct Ctx {
    core:   Core,
    tokens: Arc<Tokens>,
}

/// A started server: where it listens and its certificate's fingerprint.
pub struct Running {
    pub addr:        SocketAddr,
    pub fingerprint: String,
    pub task:        tokio::task::JoinHandle<()>,
}

/// Listen on `addr` (port 0 picks a free one, for tests) and serve until
/// the task is dropped with the runtime.
pub async fn start(core: Core, identity: &Identity, addr: SocketAddr) -> Result<Running> {
    let acceptor = TlsAcceptor::from(Arc::new(identity.server_config()?));
    let listener = TcpListener::bind(addr).await.with_context(|| format!("listening on {addr}"))?;
    let addr = listener.local_addr()?;
    let router = router(Ctx { core, tokens: Arc::default() });
    let task = tokio::spawn(async move {
        loop {
            let (tcp, peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    warn!("Office server: accept failed: {e}");
                    continue;
                }
            };
            let (acceptor, router) = (acceptor.clone(), router.clone());
            // Each connection on its own task, handshake included: a slow
            // or silent client must not hold up the others.
            tokio::spawn(async move {
                let tls = match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await {
                    Ok(Ok(tls)) => tls,
                    Ok(Err(e)) => return tracing::debug!("TLS handshake with {peer} failed: {e}"),
                    Err(_) => return tracing::debug!("TLS handshake with {peer} timed out"),
                };
                let service = hyper::service::service_fn(move |mut req: hyper::Request<hyper::body::Incoming>| {
                    req.extensions_mut().insert(ConnectInfo(peer));
                    let router = router.clone();
                    async move {
                        use tower::ServiceExt;
                        router.oneshot(req.map(axum::body::Body::new)).await
                    }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(tls), service)
                    .await
                {
                    tracing::debug!("Connection from {peer} ended: {e}");
                }
            });
        }
    });
    info!("Office server listening on {addr}, certificate {}", identity.fingerprint());
    Ok(Running { addr, fingerprint: identity.fingerprint(), task })
}

fn router(ctx: Ctx) -> Router {
    Router::new()
        .route("/api/v1/hello", get(|| async { Json(Hello::this_build()) }))
        .route("/api/v1/login", post(login))
        .route("/api/v1/logout", post(logout))
        .route("/api/v1/call/{cmd}", post(call))
        .route("/api/v1/query", post(ask))
        .layer(DefaultBodyLimit::max(BODY_LIMIT))
        .with_state(ctx)
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get(header::AUTHORIZATION)?.to_str().ok()?.strip_prefix("Bearer ")
}

/// An error as the window would have got it from `invoke`, with a status
/// for logs and proxies.
struct Failure(AppError);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = match self.0.code {
            "not_signed_in" => StatusCode::UNAUTHORIZED,
            "admin_required" | "document_delete_forbidden" => StatusCode::FORBIDDEN,
            "login_throttled" => StatusCode::TOO_MANY_REQUESTS,
            "unknown_command" => StatusCode::NOT_FOUND,
            "internal" => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::UNPROCESSABLE_ENTITY,
        };
        (status, Json(self.0)).into_response()
    }
}

impl From<AppError> for Failure {
    fn from(e: AppError) -> Self {
        Self(e)
    }
}

#[derive(Serialize, Deserialize)]
pub struct LoginReply {
    /// Present when the PIN was right.
    pub token:  Option<String>,
    pub result: auth::LoginResult,
}

async fn login(
    State(ctx): State<Ctx>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(args): Json<auth::LoginArgs>,
) -> Result<Json<LoginReply>, Failure> {
    let user = auth::login(&ctx.core, &args, &peer.ip().to_string()).await?;
    let token = user.as_ref().map(|u| ctx.tokens.issue(u.clone(), Instant::now()));
    Ok(Json(LoginReply { token, result: auth::LoginResult::of(user.as_ref()) }))
}

async fn logout(State(ctx): State<Ctx>, headers: HeaderMap) -> Result<Json<()>, Failure> {
    if let Some(user) = bearer(&headers).and_then(|t| ctx.tokens.revoke(t)) {
        auth::logout(&ctx.core, &user).await?;
    }
    Ok(Json(()))
}

async fn call(
    State(ctx): State<Ctx>,
    Path(cmd): Path<String>,
    headers: HeaderMap,
    Json(args): Json<Value>,
) -> Result<Json<Value>, Failure> {
    let caller = ctx.tokens.caller(bearer(&headers), Instant::now());
    Ok(Json(dispatch(&ctx.core, &caller, &cmd, &args).await?))
}

/// An argument as `invoke` names it: the window writes `{ documentId }`
/// for a parameter `document_id`; a missing one is `null`, which an
/// `Option` takes as `None`.
fn arg<T: DeserializeOwned>(args: &Value, key: &str) -> Result<T, AppError> {
    serde_json::from_value(args.get(key).cloned().unwrap_or(Value::Null)).map_err(|e| {
        AppError::new("bad_arguments", format!("Argument {key}: {e}")).with("name", key)
    })
}

fn json(v: impl Serialize) -> Result<Value, AppError> {
    serde_json::to_value(v).map_err(AppError::of)
}

/// The commands a client may call. Each is the window's own command — the
/// same core function behind `#[tauri::command]` — with the caller from
/// the token.
pub(crate) async fn dispatch(core: &Core, caller: &Caller, cmd: &str, a: &Value) -> Result<Value, AppError> {
    match cmd {
        // The login screen's profile list, before anyone signs in.
        "cmd_list_users" => json(auth::list_users(core).await?),
        // From the window, a profile is created on the login screen (and
        // the first one becomes the administrator). Over the network only
        // an administrator creates profiles: anyone on the office network
        // could otherwise give themselves one, and with it the default
        // department's documents.
        "cmd_create_user" => {
            admin::require_admin(core, caller).await?;
            json(auth::create_user(core, caller, arg(a, "args")?).await?)
        }
        "cmd_current_session" => json(caller.get()),

        "cmd_upload_document" => json(documents::upload_document(core, caller, arg(a, "args")?).await?),
        "cmd_list_documents" => json(documents::list_documents(core, caller).await?),
        "cmd_list_document_tables" => json(documents::list_document_tables(core, caller).await?),
        "cmd_get_job_status" => json(documents::get_job_status(core, caller, arg(a, "jobId")?).await?),
        "cmd_delete_document" => json(documents::delete_document(core, caller, arg(a, "documentId")?).await?),

        "cmd_query" => json(query::query(core, caller, arg(a, "args")?).await?),

        "cmd_list_conversations" => json(chat::list_conversations(core, caller).await?),
        "cmd_get_conversation" => json(chat::get_conversation(core, caller, arg(a, "conversationId")?).await?),
        "cmd_rename_conversation" => json(chat::rename_conversation(core, caller, arg(a, "args")?).await?),
        "cmd_delete_conversation" => json(chat::delete_conversation(core, caller, arg(a, "conversationId")?).await?),

        "cmd_list_departments" => json(admin::list_departments(core, caller).await?),
        "cmd_list_my_departments" => json(admin::list_my_departments(core, caller).await?),
        "cmd_create_department" => json(admin::create_department(core, caller, arg(a, "args")?).await?),
        "cmd_delete_department" => json(admin::delete_department(core, caller, arg(a, "departmentId")?).await?),
        "cmd_set_department_instructions" => {
            json(admin::set_department_instructions(core, caller, arg(a, "args")?).await?)
        }
        "cmd_list_memberships" => json(admin::list_memberships(core, caller).await?),
        "cmd_add_member" => json(admin::add_member(core, caller, arg(a, "args")?).await?),
        "cmd_remove_member" => json(admin::remove_member(core, caller, arg(a, "args")?).await?),
        "cmd_list_audit" => json(admin::list_audit(core, caller, arg(a, "limit")?).await?),
        "cmd_list_adapters" => json(admin::list_adapters(core, caller).await?),

        _ => Err(AppError::new("unknown_command", format!("{cmd} is not available from another computer."))
            .with("name", cmd)),
    }
}

/// A question, streamed: `token` events while the model writes, then the
/// whole `result` (or an `error`). The answer is finished and saved even
/// if the client goes away midway, as in the window.
async fn ask(State(ctx): State<Ctx>, headers: HeaderMap, Json(a): Json<Value>) -> Result<Response, Failure> {
    let caller = ctx.tokens.caller(bearer(&headers), Instant::now());
    caller.require()?;
    let args: query::QueryArgs = arg(&a, "args")?;
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    tokio::spawn(async move {
        let piece = tx.clone();
        let result = query::query_stream(&ctx.core, &caller, args, move |token| {
            let _ = piece.send(event("token", &serde_json::json!({ "token": token })));
        })
        .await;
        let _ = tx.send(match result {
            Ok(r) => event("result", &r),
            Err(e) => event("error", &e),
        });
    });
    let stream = tokio_stream::wrappers::UnboundedReceiverStream::new(rx);
    use tokio_stream::StreamExt;
    Ok(Sse::new(stream.map(Ok::<_, Infallible>)).into_response())
}

fn event(name: &str, data: &impl Serialize) -> Event {
    Event::default().event(name).data(serde_json::to_string(data).unwrap_or_else(|_| "null".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(name: &str) -> SessionUser {
        SessionUser { id: uuid::Uuid::new_v4(), username: name.into(), is_admin: false }
    }

    #[test]
    fn a_token_names_its_user_until_revoked_or_idle() {
        let tokens = Tokens::default();
        let t0 = Instant::now();
        let alice = tokens.issue(user("alice"), t0);
        assert_eq!(alice.len(), 64);
        assert_eq!(tokens.caller(Some(&alice), t0).require().unwrap().username, "alice");
        assert!(tokens.caller(Some("forged"), t0).get().is_none());
        assert!(tokens.caller(None, t0).get().is_none());

        // Use keeps it alive; twelve idle hours end it.
        let later = t0 + IDLE_LIMIT - Duration::from_secs(1);
        assert!(tokens.caller(Some(&alice), later).get().is_some());
        assert!(tokens.caller(Some(&alice), later + IDLE_LIMIT).get().is_none());

        let bob = tokens.issue(user("bob"), t0);
        assert_eq!(tokens.revoke(&bob).unwrap().username, "bob");
        assert!(tokens.caller(Some(&bob), t0).get().is_none());
    }

    #[test]
    fn arguments_are_read_by_the_names_invoke_uses() {
        let a = serde_json::json!({ "documentId": "6f1c3a52-3c5e-4b8e-9d55-1b2f3c4d5e6f", "limit": null });
        let id: uuid::Uuid = arg(&a, "documentId").unwrap();
        assert_eq!(id.to_string(), "6f1c3a52-3c5e-4b8e-9d55-1b2f3c4d5e6f");
        assert_eq!(arg::<Option<i64>>(&a, "limit").unwrap(), None);
        assert_eq!(arg::<uuid::Uuid>(&a, "jobId").unwrap_err().code, "bad_arguments");
    }
}
