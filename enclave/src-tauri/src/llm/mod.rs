use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::Arc;
use tauri::AppHandle;
use tauri_plugin_shell::ShellExt;
use tokio::sync::RwLock;
use tracing::{info, warn};

pub mod adapters;

/// Shared HTTP client for the llama-server sidecar.
/// Managed as Tauri state after `LlmClient::spawn()`.
///
/// `adapter_index` maps an adapter's on-disk path (as stored in
/// `department_adapters.adapter_path`) to the integer id llama-server
/// assigned it when loading (CLAUDE.md invariant #5 — the `lora` payload
/// must use *this* id, never the row's DB UUID).
#[derive(Clone)]
pub struct LlmClient {
    pub http: Client,
    pub base_url: String,
    adapter_index: Arc<RwLock<HashMap<String, u32>>>,
}

impl LlmClient {
    /// Spawn the llama-server sidecar, preload every currently-active
    /// LoRA adapter, and wait until the server is healthy.
    pub async fn spawn(app: &AppHandle, pool: &PgPool) -> Result<Self> {
        let base_url = "http://127.0.0.1:8080".to_string();

        // Preload adapters at startup (--lora-init-without-apply loads them
        // into memory without activating any; per-request selection happens
        // via the `lora` field on /completion, resolved through
        // refresh_adapter_index below).
        //
        // department_adapters is a pure junction (department_id, adapter_id,
        // scale, is_default) — the file path and is_active flag live on the
        // separate `adapters` catalog table it references, so this joins
        // through it rather than reading columns that live on the junction
        // in the (unused) migrations/002 design.
        let adapter_paths: Vec<String> = sqlx::query_scalar(
            r#"
            SELECT DISTINCT a.file_path
            FROM department_adapters da
            JOIN adapters a ON a.id = da.adapter_id
            WHERE a.is_active = true
            "#,
        )
        .fetch_all(pool)
        .await
        .context("Failed to load active adapter paths")?;

        // Tauri sidecar path is declared in tauri.conf.json under bundle.externalBin.
        // The binary name must match the pattern: binaries/llama-server-<target-triple>
        info!("Spawning llama-server sidecar with {} adapter(s)…", adapter_paths.len());
        let shell = app.shell();
        let mut args: Vec<String> = vec![
            "--port".into(), "8080".into(),
            "--lora-init-without-apply".into(),
            // Model path is resolved from app data dir at runtime.
            // Placeholder: users configure via the Admin page.
            "--model".into(), "models/base.gguf".into(),
        ];
        for path in &adapter_paths {
            args.push("--lora".into());
            args.push(path.clone());
        }

        let (_rx, _child) = shell
            .sidecar("llama-server")
            .context("llama-server sidecar not found — did you run fetch-sidecar.ps1?")?
            .args(args)
            .spawn()
            .context("Failed to spawn llama-server")?;

        // Wait for the server to be ready (up to 60 s).
        let http = Client::new();
        let health_url = format!("{base_url}/health");
        let mut attempts = 0u32;
        loop {
            attempts += 1;
            if attempts > 120 {
                anyhow::bail!("llama-server did not become ready after 60 s");
            }
            match http.get(&health_url).send().await {
                Ok(r) if r.status().is_success() => break,
                _ => {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
            }
        }
        info!("llama-server ready at {base_url}");

        let client = Self { http, base_url, adapter_index: Arc::new(RwLock::new(HashMap::new())) };
        if let Err(e) = client.refresh_adapter_index().await {
            warn!("Could not read adapter index from sidecar: {e:#}");
        }
        Ok(client)
    }

    /// Query the sidecar for the adapters it actually has loaded and cache
    /// path → sidecar-assigned-id. Call again after adapters are added or
    /// removed from `department_adapters` and the sidecar is restarted.
    pub async fn refresh_adapter_index(&self) -> Result<()> {
        #[derive(Deserialize)]
        struct AdapterEntry {
            id: u32,
            path: String,
        }

        let entries: Vec<AdapterEntry> = self
            .http
            .get(format!("{}/lora-adapters", self.base_url))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let mut index = self.adapter_index.write().await;
        index.clear();
        for e in entries {
            index.insert(e.path, e.id);
        }
        info!("Adapter index refreshed: {} adapter(s) loaded.", index.len());
        Ok(())
    }

    /// The sidecar's integer id for an adapter file path, if it is loaded.
    /// Returns `None` (rather than guessing) when the sidecar hasn't loaded
    /// that path — callers must skip the adapter rather than send a
    /// fabricated id.
    pub async fn adapter_id_for_path(&self, path: &str) -> Option<u32> {
        self.adapter_index.read().await.get(path).copied()
    }

    /// POST /completion  (non-streaming, returns full text).
    pub async fn complete(&self, req: &CompletionRequest) -> Result<String> {
        #[derive(Deserialize)]
        struct Resp {
            content: String,
        }
        let resp: Resp = self
            .http
            .post(format!("{}/completion", self.base_url))
            .json(req)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.content)
    }

    /// POST /completion with `stream: true`. llama-server responds with an
    /// SSE stream of `data: {"content": "...", "stop": bool, ...}\n\n`
    /// frames; `on_token` is called with each chunk's text as it arrives,
    /// and the full concatenated answer is returned once `stop` is seen.
    pub async fn complete_stream(
        &self,
        req: &CompletionRequest,
        mut on_token: impl FnMut(&str),
    ) -> Result<String> {
        #[derive(Deserialize)]
        struct Chunk {
            content: String,
            #[serde(default)]
            stop: bool,
        }

        let mut stream_req = req.clone();
        stream_req.stream = true;

        let resp = self
            .http
            .post(format!("{}/completion", self.base_url))
            .json(&stream_req)
            .send()
            .await?
            .error_for_status()?;

        let mut full = String::new();
        let mut buf = String::new();
        let mut byte_stream = resp.bytes_stream();

        while let Some(chunk) = byte_stream.next().await {
            buf.push_str(&String::from_utf8_lossy(&chunk?));

            // SSE frames are separated by a blank line.
            while let Some(pos) = buf.find("\n\n") {
                let frame: String = buf.drain(..pos + 2).collect();
                let Some(data) = frame.trim_start().strip_prefix("data: ") else {
                    continue;
                };
                let Ok(parsed) = serde_json::from_str::<Chunk>(data.trim()) else {
                    continue;
                };

                full.push_str(&parsed.content);
                on_token(&parsed.content);

                if parsed.stop {
                    return Ok(full);
                }
            }
        }

        Ok(full)
    }

    /// POST /embeddings  (returns a flat f32 vector).
    pub async fn embed(&self, content: &str) -> Result<Vec<f32>> {
        #[derive(Serialize)]
        struct EmbedReq<'a> {
            content: &'a str,
        }
        #[derive(Deserialize)]
        struct EmbedResp {
            embedding: Vec<f32>,
        }
        let resp: EmbedResp = self
            .http
            .post(format!("{}/embeddings", self.base_url))
            .json(&EmbedReq { content })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.embedding)
    }
}

/// Request body for POST /completion.
/// The `lora` field carries per-request adapter selection (ADR-0003, 0004).
#[derive(Serialize, Clone, Debug)]
pub struct CompletionRequest {
    pub prompt: String,
    pub n_predict: i32,
    pub temperature: f32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lora: Vec<LoraEntry>,
    pub stream: bool,
}

impl Default for CompletionRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            n_predict: 512,
            temperature: 0.7,
            lora: vec![],
            stream: false,
        }
    }
}

/// One LoRA adapter entry in the llama-server request payload.
/// `id` is the 0-based index of the adapter as loaded by the sidecar.
/// `scale` maps directly from `department_adapters.scale` (ADR-0004).
#[derive(Serialize, Clone, Debug)]
pub struct LoraEntry {
    pub id: u32,
    pub scale: f32,
}
