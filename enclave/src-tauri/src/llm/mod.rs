use anyhow::{Context, Result};
use futures::StreamExt;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Manager};
use tauri_plugin_shell::{
    ShellExt,
    process::{CommandChild, CommandEvent},
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

pub mod adapters;

/// Name of the embedding model row this sidecar registers in
/// `embedding_models` (ADR-0007). Change this if you swap in a different
/// embedding GGUF — but the dimension must stay 768 to match the
/// `vector(768)` column on `chunk_embeddings`.
const EMBEDDING_MODEL_NAME: &str = "nomic-embed-text-v1.5";
const EMBEDDING_MODEL_DIMENSION: i32 = 768;

/// Resolve a model file to an absolute path and confirm it exists, so a
/// missing file fails here with the path in the message instead of as a
/// silent sidecar exit and a 60-second health-check timeout.
///
/// `ENCLAVE_MODELS_DIR` overrides the default `{app_data_dir}/models` —
/// handy in development to keep multi-GB weights out of the app data dir.
/// Relative paths are never passed to the sidecar: they would resolve
/// against the process's working directory, which is not stable once the
/// app is installed.
fn model_path(app: &AppHandle, file_name: &str) -> Result<String> {
    let dir = match std::env::var_os("ENCLAVE_MODELS_DIR") {
        Some(dir) => std::path::PathBuf::from(dir),
        None => app.path().app_data_dir().context("Could not resolve app data dir")?.join("models"),
    };
    let path = dir.join(file_name);
    anyhow::ensure!(path.is_file(), "model file not found: {}", path.display());
    Ok(path.to_string_lossy().into_owned())
}

/// Directory holding `llama-server.exe` *together with* its DLLs.
///
/// Current llama.cpp builds ship a 9 KB launcher exe; the server itself is
/// `llama-server-impl.dll`, and the ggml backends (`ggml-cuda.dll`,
/// `ggml-cpu-*.dll`) are discovered next to the executable. Tauri's
/// `externalBin` copies only the exe into `target/`, which then starts and
/// finds nothing — so the exe is run in place, from this directory.
///
/// Resolution: `ENCLAVE_LLAMA_DIR`, then `{resource_dir}/llama` (installed
/// app; bundling it is part of the open packaging decision, ADR-0014), then
/// `src-tauri/binaries/llama` in debug builds, where `fetch-sidecar.ps1`
/// unpacks the release.
fn llama_server_exe(app: &AppHandle) -> Result<PathBuf> {
    let exe_name = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::var_os("ENCLAVE_LLAMA_DIR") {
        candidates.push(PathBuf::from(dir));
    }
    if let Ok(dir) = app.path().resource_dir() {
        candidates.push(dir.join("llama"));
    }
    if cfg!(debug_assertions) {
        candidates.push(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("binaries").join("llama"));
    }
    candidates
        .iter()
        .map(|dir| dir.join(exe_name))
        .find(|exe| exe.is_file())
        .with_context(|| {
            format!(
                "{exe_name} not found in {:?} — run scripts/fetch-sidecar.ps1 or set ENCLAVE_LLAMA_DIR",
                candidates
            )
        })
}

/// llama-server log threshold: errors only.
///
/// At its default level the embedding server writes ~3 lines per embedded
/// text, and every line crosses the stdout pipe the app drains
/// (`spawn_llama_server`). Measured on 1 000 spreadsheet rows, batches of 32:
/// the same server did 153 texts/s logging to a file and 95/s through the
/// app's pipe. Errors still come through (checked: a missing model file is
/// reported line by line); routine info and per-request lines do not.
const LLAMA_LOG_VERBOSITY: &str = "1";

/// Start one llama-server and keep draining its output into the log.
///
/// The drain is not optional: an unread stdout/stderr pipe fills up and the
/// server blocks on its next log line. Lines go to the `llama` tracing
/// target at debug level (`RUST_LOG=llama=debug` to see them); an exit is
/// logged as a warning since neither server is expected to stop on its own.
fn spawn_llama_server(app: &AppHandle, name: &'static str, args: &[String]) -> Result<CommandChild> {
    let exe = llama_server_exe(app)?;
    let dir = exe.parent().map(PathBuf::from).unwrap_or_default();
    let (mut rx, child) = app
        .shell()
        .command(exe.to_string_lossy().into_owned())
        .current_dir(dir)
        .args(args)
        .spawn()
        .with_context(|| format!("Failed to spawn llama-server ({name}) from {}", exe.display()))?;

    tauri::async_runtime::spawn(async move {
        while let Some(event) = rx.recv().await {
            match event {
                CommandEvent::Stdout(line) | CommandEvent::Stderr(line) => {
                    debug!(target: "llama", "[{name}] {}", String::from_utf8_lossy(&line).trim_end());
                }
                CommandEvent::Terminated(status) => {
                    warn!("llama-server ({name}) exited: code {:?}", status.code);
                }
                _ => {}
            }
        }
    });
    Ok(child)
}

/// Which side of retrieval a text is on. nomic-embed-text is trained with
/// task prefixes and retrieves noticeably worse without them; the prefix is
/// part of the embedding input only, never of the stored chunk text.
#[derive(Clone, Copy, Debug)]
pub enum EmbedKind {
    Query,
    Document,
}

impl EmbedKind {
    fn prefix(self) -> &'static str {
        match self {
            EmbedKind::Query    => "search_query: ",
            EmbedKind::Document => "search_document: ",
        }
    }
}

/// Shared HTTP client for the llama-server sidecar(s).
/// Managed as Tauri state after `LlmClient::spawn()`.
///
/// Two separate llama-server processes run, not one: `base_url` (port 8080)
/// serves the resident chat/completion model (Qwen 2.5 3B) plus LoRA
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
    /// Both server processes, killed by `shutdown()` on app exit — Windows
    /// does not take child processes down with their parent, and a leftover
    /// server keeps its VRAM and its port.
    children: Arc<Mutex<Vec<CommandChild>>>,
}

impl LlmClient {
    /// A client for servers someone else started (the running app's) — for
    /// the command-line examples. No adapters, and `shutdown` stops nothing.
    pub fn connect(base_url: &str, embed_base_url: &str) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.to_string(),
            embed_base_url: Some(embed_base_url.to_string()),
            adapter_index: Arc::default(),
            children: Arc::default(),
        }
    }

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

        info!("Spawning llama-server (chat) with {} adapter(s)…", adapter_paths.len());
        let mut args: Vec<String> = vec![
            "--port".into(), "8080".into(),
            "--lora-init-without-apply".into(),
            // --n-gpu-layers 999: offload as many layers as fit into VRAM;
            // llama.cpp clips to what actually fits, so this is safe on any
            // GPU (falls back toward CPU if none/small).
            "--n-gpu-layers".into(), "999".into(),
            // Explicit, or llama-server sizes the KV cache to fill free VRAM
            // (measured: 59k tokens over 4 slots for a 3B model — beyond its
            // 32k training context, and leaving no room for the embedding
            // server). One desktop user = one slot; 8k covers a 5-chunk
            // prompt plus 768 generated tokens many times over.
            "--ctx-size".into(), "8192".into(),
            "--parallel".into(), "1".into(),
            // Errors only (see LLAMA_LOG_VERBOSITY).
            "--log-verbosity".into(), LLAMA_LOG_VERBOSITY.into(),
            "--model".into(), model_path(app, "base.gguf")?,
        ];
        for path in &adapter_paths {
            args.push("--lora".into());
            args.push(path.clone());
        }

        let children = Arc::new(Mutex::new(Vec::new()));
        children.lock().unwrap_or_else(|e| e.into_inner()).push(spawn_llama_server(app, "chat", &args)?);

        wait_for_health(&http, &base_url, 120).await.context("llama-server (chat) did not become ready")?;
        info!("llama-server (chat) ready at {base_url}");

        let embed_base_url = match Self::spawn_embedding_sidecar(app, &http, pool, &children).await {
            Ok(url) => Some(url),
            Err(e) => {
                warn!(
                    "Embedding sidecar unavailable, chat will still work but ingestion cannot embed \
                     documents until this is fixed: {e:#}"
                );
                None
            }
        };

        let client = Self {
            http,
            base_url,
            embed_base_url,
            adapter_index: Arc::new(RwLock::new(HashMap::new())),
            children,
        };
        if let Err(e) = client.refresh_adapter_index().await {
            warn!("Could not read adapter index from sidecar: {e:#}");
        }
        Ok(client)
    }

    /// Spawn the second llama-server, loaded with a small dedicated
    /// embedding model on a different port, and register it in
    /// `embedding_models` (ADR-0007) so `ingest/mod.rs`'s "active model"
    /// lookup finds it. Returns the embedding server's base URL on success.
    async fn spawn_embedding_sidecar(
        app:      &AppHandle,
        http:     &Client,
        pool:     &PgPool,
        children: &Mutex<Vec<CommandChild>>,
    ) -> Result<String> {
        let base_url = "http://127.0.0.1:8081".to_string();
        let model = model_path(app, "embed.gguf")?;

        let args: Vec<String> = [
            "--port", "8081",
            "--embedding",
            "--n-gpu-layers", "999",
            // nomic-embed-text was trained on 2048-token inputs; chunks
            // are 512 characters, far below that.
            "--ctx-size", "2048",
            "--parallel", "1",
            "--log-verbosity", LLAMA_LOG_VERBOSITY,
            // A small embedding GGUF (nomic-embed-text-v1.5, ~270 MB) —
            // separate from the chat model, see the struct doc.
            "--model", &model,
        ]
        .map(String::from)
        .to_vec();
        children.lock().unwrap_or_else(|e| e.into_inner()).push(spawn_llama_server(app, "embed", &args)?);

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

    /// POST /v1/embeddings on the *embedding* sidecar (never the chat one —
    /// see the struct doc comment on why they're separate processes).
    ///
    /// The OpenAI-compatible endpoint, not the native `/embeddings`: the
    /// native one's shape has changed across llama.cpp releases (current
    /// builds answer `[{"index":0,"embedding":[[…]]}]`, which the old
    /// `{"embedding":[…]}` parsing here rejected), while `/v1/embeddings`
    /// keeps the `{"data":[{"embedding":[…]}]}` contract.
    pub async fn embed(&self, content: &str, kind: EmbedKind) -> Result<Vec<f32>> {
        self.embed_batch(&[content], kind)
            .await?
            .pop()
            .context("embedding server returned no data")
    }

    /// Embed several texts in one request (`input` as an array), returned in
    /// the order given.
    ///
    /// Measured with the app's embedding server settings (nomic-embed-text,
    /// RTX 4050): one text per request 49 chunks/s, 16 per request 138, 32
    /// per request 148, 64 no better — 3× faster ingestion, which is what
    /// makes a 50 000-row spreadsheet a ~6-minute job instead of ~17.
    pub async fn embed_batch<S: AsRef<str>>(&self, contents: &[S], kind: EmbedKind) -> Result<Vec<Vec<f32>>> {
        let embed_base_url = self
            .embed_base_url
            .as_deref()
            .context("Embedding model not available — the embedding llama-server sidecar isn't running")?;

        #[derive(Serialize)]
        struct EmbedReq {
            input: Vec<String>,
        }
        #[derive(Deserialize)]
        struct EmbedResp {
            data: Vec<EmbedData>,
        }
        #[derive(Deserialize)]
        struct EmbedData {
            index: usize,
            embedding: Vec<f32>,
        }
        let input: Vec<String> = contents.iter().map(|c| format!("{}{}", kind.prefix(), c.as_ref())).collect();
        let resp: EmbedResp = self
            .http
            .post(format!("{embed_base_url}/v1/embeddings"))
            .json(&EmbedReq { input })
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::ensure!(
            resp.data.len() == contents.len(),
            "embedding server returned {} vectors for {} inputs",
            resp.data.len(),
            contents.len()
        );
        // The OpenAI contract carries an index per item; order by it rather
        // than trusting the array order.
        let mut data = resp.data;
        data.sort_by_key(|d| d.index);
        Ok(data.into_iter().map(|d| d.embedding).collect())
    }

    /// Wrap a system + user message in the chat template stored in the
    /// model's own GGUF (POST /apply-template), for use as a /completion
    /// prompt.
    ///
    /// Without it an instruct model sees raw text, doesn't know where its
    /// turn ends, and keeps going: measured on Qwen 2.5 3B, the answer
    /// came back followed by a restatement of itself and a stray code
    /// fence. /completion is kept (rather than /v1/chat/completions)
    /// because its `lora` field and SSE frame shape are what the rest of
    /// this client is built on.
    pub async fn apply_template(&self, system: &str, user: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct Resp {
            prompt: String,
        }
        let resp: Resp = self
            .http
            .post(format!("{}/apply-template", self.base_url))
            .json(&serde_json::json!({
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user",   "content": user },
                ]
            }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(resp.prompt)
    }

    /// Kill both llama-server processes. Called on app exit.
    pub fn shutdown(&self) {
        let mut children = self.children.lock().unwrap_or_else(|e| e.into_inner());
        for child in children.drain(..) {
            if let Err(e) = child.kill() {
                warn!("Failed to stop llama-server: {e}");
            }
        }
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
    /// Constrains the output to JSON matching this schema — llama-server
    /// compiles it into a sampling grammar (the table planner, ADR-0022).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,
}

impl Default for CompletionRequest {
    fn default() -> Self {
        Self {
            prompt: String::new(),
            n_predict: 512,
            temperature: 0.7,
            lora: vec![],
            stream: false,
            json_schema: None,
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
