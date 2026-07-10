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

/// Name of the embedding model row this sidecar registers in
/// `embedding_models` (ADR-0007). Change this if you swap in a different
/// embedding GGUF — but the dimension must stay 768 to match the
/// `vector(768)` column on `chunk_embeddings`.
const EMBEDDING_MODEL_NAME: &str = "nomic-embed-text-v1.5";
const EMBEDDING_MODEL_DIMENSION: i32 = 768;

/// Shared HTTP client for the llama-server sidecar(s).
/// Managed as Tauri state after `LlmClient::spawn()`.
///
/// Two separate llama-server processes run, not one: `base_url` (port 8080)
/// serves the resident chat/completion model (Qwen 2.5 7B) plus LoRA
/// adapters, and `embed_base_url` (port 8081, `Option` because it's fine for
/// chat to work without it) serves a *separate* small embedding model. They
/// must be separate because `chunk_embeddings.embedding` is a fixed
/// `vector(768)` (ADR-0007) and the chat model's native hidden dimension is
/// nowhere near 768 — reusing it for `/embeddings` would produce
/// wrong-sized (and low-quality) vectors.
///
/// `adapter_index` maps an adapter's on-disk path (as stored in
/// `department_adapters.adapter_path`) to the integer id llama-server
/// assigned it when loading (CLAUDE.md invariant #5 — the `lora` payload
/// must use *this* id, never the row's DB UUID).
#[derive(Clone)]
pub struct LlmClient {
    pub http: Client,
    pub base_url: String,
    pub embed_base_url: Option<String>,
    adapter_index: Arc<RwLock<HashMap<String, u32>>>,
}

impl LlmClient {
    /// Spawn both llama-server sidecars, preload every currently-active
    /// LoRA adapter on the chat one, and wait until each is healthy. The
    /// embedding sidecar is best-effort: if it can't start (no model file
    /// yet, still on Stage 1 of setup), chat keeps working and only
    /// ingestion fails, with a clear error, until it's fixed.
    pub async fn spawn(app: &AppHandle, pool: &PgPool) -> Result<Self> {
        let base_url = "http://127.0.0.1:8080".to_string();
        let http = Client::new();

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
        let mut args: Vec<String> = vec![
            "--port".into(), "8080".into(),
            "--lora-init-without-apply".into(),
            // --n-gpu-layers 999: offload as many layers as fit into VRAM;
            // llama.cpp clips to what actually fits, so this is safe on any
            // GPU (falls back toward CPU if none/small).
            "--n-gpu-layers".into(), "999".into(),
            // Model path is resolved from app data dir at runtime.
            // Placeholder: users configure via the Admin page.
            "--model".into(), "models/base.gguf".into(),
        ];
        for path in &adapter_paths {
            args.push("--lora".into());
            args.push(path.clone());
        }

        let shell = app.shell();
        let (_rx, _child) = shell
            .sidecar("llama-server")
            .context("llama-server sidecar not found — did you run fetch-sidecar.ps1?")?
            .args(args)
            .spawn()
            .context("Failed to spawn llama-server")?;

        wait_for_health(&http, &base_url, 120).await.context("llama-server (chat) did not become ready")?;
        info!("llama-server (chat) ready at {base_url}");

        let embed_base_url = match Self::spawn_embedding_sidecar(app, &http, pool).await {
            Ok(url) => Some(url),
            Err(e) => {
                warn!(
                    "Embedding sidecar unavailable, chat will still work but ingestion cannot embed \
                     documents until this is fixed: {e:#}"
                );
                None
            }
        };

        let client = Self { http, base_url, embed_base_url, adapter_index: Arc::new(RwLock::new(HashMap::new())) };
        if let Err(e) = client.refresh_adapter_index().await {
            warn!("Could not read adapter index from sidecar: {e:#}");
        }
        Ok(client)
    }

    /// Spawn the second llama-server, loaded with a small dedicated
    /// embedding model on a different port, and register it in
    /// `embedding_models` (ADR-0007) so `ingest/mod.rs`'s "active model"
    /// lookup finds it. Returns the embedding server's base URL on success.
    async fn spawn_embedding_sidecar(app: &AppHandle, http: &Client, pool: &PgPool) -> Result<String> {
        let base_url = "http://127.0.0.1:8081".to_string();

        let shell = app.shell();
        let (_rx, _child) = shell
            .sidecar("llama-server")
            .context("llama-server sidecar not found — did you run fetch-sidecar.ps1?")?
            .args([
                "--port", "8081",
                "--embedding",
                "--n-gpu-layers", "999",
                // Place a small embedding GGUF (e.g. nomic-embed-text-v1.5,
                // ~80-150MB quantized) here — separate from the chat model.
                "--model", "models/embed.gguf",
            ])
            .spawn()
            .context("Failed to spawn embedding llama-server")?;

        wait_for_health(http, &base_url, 60).await.context("embedding llama-server did not become ready")?;
        info!("llama-server (embedding) ready at {base_url}");

        // Exactly one active embedding model at a time — ingest/mod.rs picks
        // whichever row has is_active = true. Deactivate any other first so
        // re-registering on restart doesn't leave two active rows.
        let mut tx = pool.begin().await?;
        sqlx::query("UPDATE embedding_models SET is_active = false").execute(&mut *tx).await?;
        sqlx::query(
            r#"
            INSERT INTO embedding_models (name, dimension, provider, is_active)
            VALUES ($1, $2, 'llama.cpp', true)
            ON CONFLICT (name) DO UPDATE SET is_active = true, dimension = EXCLUDED.dimension
            "#,
        )
        .bind(EMBEDDING_MODEL_NAME)
        .bind(EMBEDDING_MODEL_DIMENSION)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(base_url)
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

    /// POST /embeddings on the *embedding* sidecar (never the chat one —
    /// see the struct doc comment on why they're separate processes).
    pub async fn embed(&self, content: &str) -> Result<Vec<f32>> {
        let embed_base_url = self
            .embed_base_url
            .as_deref()
            .context("Embedding model not available — the embedding llama-server sidecar isn't running")?;

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
            .post(format!("{embed_base_url}/embeddings"))
            .json(&EmbedReq { content })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.embedding)
    }
}

/// Poll `{base_url}/health` until it responds successfully or `max_attempts`
/// (at 500ms apart) are exhausted.
async fn wait_for_health(http: &Client, base_url: &str, max_attempts: u32) -> Result<()> {
    let health_url = format!("{base_url}/health");
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        if attempts > max_attempts {
            anyhow::bail!("server at {base_url} did not become ready after {} attempts", max_attempts);
        }
        match http.get(&health_url).send().await {
            Ok(r) if r.status().is_success() => return Ok(()),
            _ => tokio::time::sleep(std::time::Duration::from_millis(500)).await,
        }
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
