//! An office client's core (ADR-0031): the window's commands, forwarded to
//! the server. The client holds no database credentials, files or models
//! — only the server's address, its certificate's fingerprint and, after
//! sign-in, a token, which stays here in the core and never reaches the
//! webview.
//!
//! The window calls `cmd_remote_call` with the command and arguments it
//! would have passed to `invoke` (`src/api.ts`); answers stream back as
//! the same `llm-token:<id>` events the window listens to locally.

use futures::StreamExt;
use serde_json::Value;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, State};

use super::{server::LoginReply, tls, Hello};
use crate::error::AppError;

pub struct Remote {
    http:    reqwest::Client,
    server:  String,
    token:   Mutex<Option<String>>,
    /// Set once the server has been found to be this build.
    checked: Mutex<bool>,
}

impl Remote {
    pub fn new(server: &str, fingerprint: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .use_preconfigured_tls(tls::pinned_client_config(fingerprint))
            .connect_timeout(Duration::from_secs(5))
            .build()?;
        Ok(Self {
            http,
            server: server.trim_end_matches('/').to_string(),
            token: Mutex::new(None),
            checked: Mutex::new(false),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/v1/{path}", self.server)
    }

    fn token(&self) -> Option<String> {
        self.token.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_token(&self, token: Option<String>) {
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = token;
    }

    fn post(&self, path: &str) -> reqwest::RequestBuilder {
        let req = self.http.post(self.url(path));
        match self.token() {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    /// A transport failure in the window's terms: the pin did not match,
    /// or the server could not be reached.
    fn unreachable(&self, e: reqwest::Error) -> AppError {
        let mut chain = String::new();
        let mut source: Option<&dyn std::error::Error> = Some(&e);
        while let Some(s) = source {
            chain.push_str(&format!("{s}: "));
            source = s.source();
        }
        if chain.contains(tls::IDENTITY_CHANGED) {
            AppError::new(
                "server_identity_changed",
                format!("The server at {} presented a different certificate than the one this computer was connected with.", self.server),
            )
            .with("server", &self.server)
        } else {
            AppError::new("server_unreachable", format!("Cannot reach the Enclave server at {}: {chain}", self.server))
                .with("server", &self.server)
        }
    }

    /// The server's JSON, or its error as the window would have got it.
    async fn reply(&self, resp: Result<reqwest::Response, reqwest::Error>) -> Result<Value, Value> {
        let resp = resp.map_err(|e| to_value(self.unreachable(e)))?;
        let ok = resp.status().is_success();
        let body: Value = resp.json().await.map_err(|e| to_value(self.unreachable(e)))?;
        if ok {
            Ok(body)
        } else {
            Err(body)
        }
    }

    /// The server must be this build (ADR-0031): checked before the first
    /// command, and again after a failure, never cached as failed.
    async fn check_version(&self) -> Result<(), Value> {
        if *self.checked.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(());
        }
        let resp = self.http.get(self.url("hello")).send().await.map_err(|e| to_value(self.unreachable(e)))?;
        let theirs: Hello = resp.json().await.map_err(|e| to_value(self.unreachable(e)))?;
        let ours = Hello::this_build();
        if theirs != ours {
            return Err(to_value(
                AppError::new(
                    "server_version_mismatch",
                    format!(
                        "The server runs Enclave {} (schema {}), this computer {} (schema {}). Update both to the same version, the server first.",
                        theirs.version, theirs.migration, ours.version, ours.migration
                    ),
                )
                .with("server", theirs.version)
                .with("here", ours.version),
            ));
        }
        *self.checked.lock().unwrap_or_else(|e| e.into_inner()) = true;
        Ok(())
    }

    pub async fn call(&self, cmd: &str, args: &Value) -> Result<Value, Value> {
        self.check_version().await?;
        let result = self.reply(self.post(&format!("call/{cmd}")).json(args).send().await).await;
        if let Err(e) = &result {
            // The server forgot the token (a restart, twelve idle hours):
            // drop it, so the window's next session check shows sign-in.
            if e.get("code").and_then(Value::as_str) == Some("not_signed_in") {
                self.set_token(None);
            }
        }
        result
    }

    pub async fn login(&self, args: &Value) -> Result<Value, Value> {
        self.check_version().await?;
        self.set_token(None);
        let reply = self.reply(self.http.post(self.url("login")).json(args).send().await).await?;
        let reply: LoginReply = serde_json::from_value(reply).map_err(|e| to_value(AppError::of(e)))?;
        self.set_token(reply.token);
        Ok(serde_json::to_value(reply.result).unwrap_or_default())
    }

    pub async fn logout(&self) -> Result<Value, Value> {
        if self.token().is_some() {
            let result = self.reply(self.post("logout").send().await).await;
            self.set_token(None);
            result?;
        }
        Ok(Value::Null)
    }

    pub async fn current_session(&self) -> Result<Value, Value> {
        if self.token().is_none() {
            return Ok(Value::Null);
        }
        self.call("cmd_current_session", &Value::Null).await
    }

    /// A question: each `token` event to `on_token`, then the result.
    pub async fn ask(&self, args: &Value, on_token: impl Fn(&str)) -> Result<Value, Value> {
        self.check_version().await?;
        let resp = self.post("query").json(args).send().await.map_err(|e| to_value(self.unreachable(e)))?;
        if !resp.status().is_success() {
            return Err(resp.json().await.map_err(|e| to_value(self.unreachable(e)))?);
        }
        let mut body = resp.bytes_stream();
        let mut buffer = String::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|e| to_value(self.unreachable(e)))?;
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some(end) = buffer.find("\n\n") {
                let block: String = buffer.drain(..end + 2).collect();
                let Some((name, data)) = parse_event(&block) else { continue };
                let data: Value = serde_json::from_str(&data).unwrap_or(Value::Null);
                match name.as_str() {
                    "token" => on_token(data.get("token").and_then(Value::as_str).unwrap_or_default()),
                    "result" => return Ok(data),
                    "error" => return Err(data),
                    _ => {}
                }
            }
        }
        Err(to_value(AppError::new(
            "server_unreachable",
            format!("The server at {} stopped answering midway.", self.server),
        )
        .with("server", &self.server)))
    }
}

fn to_value(e: AppError) -> Value {
    serde_json::to_value(e).unwrap_or_default()
}

/// One server-sent event block: its name and its data lines joined.
fn parse_event(block: &str) -> Option<(String, String)> {
    let (mut name, mut data) = (None, Vec::new());
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            name = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data.push(v.strip_prefix(' ').unwrap_or(v));
        }
    }
    Some((name?, data.join("\n")))
}

/// Every window command on a client. Sign-in, sign-out and the session
/// check are the client's own (the token lives here); a question streams;
/// everything else is forwarded as it is.
#[tauri::command]
pub async fn cmd_remote_call(app: AppHandle, remote: State<'_, Remote>, cmd: String, args: Value) -> Result<Value, Value> {
    match cmd.as_str() {
        "cmd_login" => remote.login(args.get("args").unwrap_or(&Value::Null)).await,
        "cmd_logout" => remote.logout().await,
        "cmd_current_session" => remote.current_session().await,
        "cmd_query_stream" => {
            let event = format!("llm-token:{}", args.get("requestId").and_then(Value::as_str).unwrap_or_default());
            remote
                .ask(&args, |token| {
                    let _ = app.emit(&event, serde_json::json!({ "token": token }));
                })
                .await
        }
        _ => remote.call(&cmd, &args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_sent_events_are_read_by_name() {
        assert_eq!(
            parse_event("event: token\ndata: {\"token\":\"При\"}\n\n"),
            Some(("token".into(), "{\"token\":\"При\"}".into()))
        );
        assert_eq!(parse_event(": keep-alive\n\n"), None);
    }
}
