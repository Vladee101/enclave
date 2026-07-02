# Enclave — Session Handoff

On-premises RAG + LoRA desktop app (portfolio piece). This file captures the
current state so a new chat can continue without re-deriving anything.

## TL;DR

The app **builds and runs**. The full shell works: create profile, login/logout,
document upload (with sha-256 dedup), and the async ingestion pipeline running
end to end. Inference is **not** wired yet — the llama-server sidecar is a
placeholder, so ingestion jobs fail gracefully with "sidecar unavailable." The
active task is **Plan A: install the real llama-server** and get embeddings +
generation working.

## Stack & layout

- Tauri 2 + React + Vite + TypeScript, Rust core, PostgreSQL 16 + pgvector,
  bundled llama-server sidecar (currently a stub).
- **Real project directory: `~/OneDrive/Desktop/enclave-anti/enclave`** (the
  NESTED one). Run all commands from there.
- ⚠️ There is a stale `~/OneDrive/Desktop/enclave` folder and also the parent
  `enclave-anti/` — both are NOT the project. Repeatedly landing one directory
  too high has caused "file not found" confusion. Always confirm with `pwd`;
  you want it to end in `enclave-anti/enclave`.

## Infrastructure / how to run

**Database (Docker):**
- Container `enclave-db`, image `pgvector/pgvector:pg16`, port mapped **5433:5432**
  (5433 on host, deliberately not 5432 to avoid clashing with the `geolock-pg`
  PostGIS container).
- Database `enclave`. Schema is applied by `sqlx::migrate!("../migrations")`
  against `ADMIN_DATABASE_URL` on every app startup (`db/mod.rs`), i.e. from
  `enclave/migrations/*.sql`, currently 001-005. **`db/schema.sql` (and its
  copy at `enclave/db/schema.sql`) is NOT applied anywhere and is not the
  live schema** — it was the originally-provided reference design (different
  table/column names throughout: `memberships`+`roles`, `password_hash`,
  `title`, `file_hash`, etc.) and diverged from what actually got built.
  Treat `enclave/migrations/*.sql` as the only source of truth for the DB
  schema; `db/schema.sql` is historical reference material only.
- Roles: `app_user`/`enclave_app` (RLS-enforced query path, non-superuser),
  `ingest_worker` (BYPASSRLS, non-superuser — the *only* role the ingestion
  worker connects as, since migrations/005), and the `postgres` superuser
  (used as the admin/provisioning role and to run migrations).

**Env vars — must be set in the same terminal that runs the app, every session:**
```bash
export ADMIN_DATABASE_URL="postgres://postgres:<superuser-pass>@localhost:5433/enclave"
export APP_DATABASE_URL="postgres://app_user:change-me@localhost:5433/enclave"
export INGEST_DATABASE_URL="postgres://ingest_worker:change_me_in_production@localhost:5433/enclave"
npm run tauri dev
```
`INGEST_DATABASE_URL` is new (migrations/005_role_alignment_and_columns.sql):
the ingestion worker no longer reuses `admin_pool` (which connects as the
`postgres` superuser) — it now connects as its own `ingest_worker` role
(BYPASSRLS, non-superuser, no DDL rights), per CLAUDE.md invariant #3. Run
migrations at least once with `ADMIN_DATABASE_URL` set before starting the
app so the `ingest_worker` role exists.
⚠️ These vanish when the terminal closes (set with `export`, session-scoped).
This has bitten us 3×. **Pending quality-of-life fix:** add `dotenvy` + a
`.env` in `src-tauri/` so they persist (not yet done).

**Toolchain:** rustc **1.91.1**. Kept sqlx on 0.8 specifically to avoid needing
rustc 1.94 (sqlx 0.9 requires it). Do not bump sqlx without bumping rustc.

## Fixes applied this session (so you know the code's current state)

Dependency pins (in `src-tauri/Cargo.toml` / lockfile) — **correcting stale
notes that used to be here**: an earlier version of this file claimed a
`pgvector` crate pin and `time = "=0.3.51"`. Neither is true of the current
tree — verify against `Cargo.toml`/`Cargo.lock` directly rather than trusting
this file's memory of past sessions:
- There is **no `pgvector` crate dependency**. Embeddings are bound as plain
  `Vec<f32>`/`&[f32]`; the `real[] -> vector` assignment cast pgvector
  registers on the extension side handles the conversion at the SQL layer
  (`retrieval/mod.rs`, `ingest/mod.rs`). If a `Vector` wrapper type is added
  later, re-pin per the sqlx-compatibility note that used to be here.
- `time` is currently pinned to **0.3.36** (`Cargo.toml`), not 0.3.51.
- `argon2` (PIN hashing) and `sha2` (file hashing) are present.

State management (`src-tauri/src/lib.rs`):
- Defined `AppState { app_pool, admin_pool }`, built in `.setup()` via
  `tauri::async_runtime::block_on`, registered with `app.manage(...)`.
- `LlmClient` is **optional**: `init` returns `Option<llm::LlmClient>` and it is
  managed as `Option<LlmClient>`. This is the "Plan B" that lets the app boot
  without a real sidecar.

Column-name mismatches: **this used to be a live bug** — the Rust code was
patched (in an earlier session) against a hand-altered dev database to
expect `email`/`password_hash`/`slug`/`title`/`file_hash`/`error`, but the
checked-in `enclave/migrations/002_core_tables.sql` still defined
`pin_hash`/no email/no slug/`filename`/no file_hash/`error_text`. That meant
a *fresh* database (new machine, CI, anyone else cloning this) would fail
immediately with "column does not exist" on login/upload/admin — none of it
was actually reproducible. **This is now fixed properly**: rather than
re-patching the code again, `enclave/migrations/005_role_alignment_and_columns.sql`
renames/adds the columns so a fresh migrate produces exactly what the code
queries:
- `pin_hash` → `password_hash`; `email` added (backfilled `username@local`)
- `users.is_admin` added (see "Admin authorization" below)
- `departments.slug` added (backfilled from `name`)
- `filename` → `title` on `documents`; `file_hash` added (unique per
  department, backfilled with a synthetic placeholder for any pre-existing
  rows — real uploads always compute a genuine sha-256)
- `queued_at` → `created_at`, `error_text` → `error` on `ingestion_jobs`

Rust struct fields are still named `filename` in a couple of DTOs
(`DocumentInfo`, `JobStatus`) — those alias in SQL (`title AS filename`),
which is intentional and doesn't need to change.

Robustness:
- `cmd_get_job_status`: `fetch_one` → `fetch_optional`, returns `Option<JobStatus>`.
- Frontend null guards: `src/hooks/useJobPoller.ts` (`JobStatus | null` +
  `if (!status) return`) and `src/pages/Documents.tsx` (`if (!job) return` in the
  `forEach`). These fixed a black-screen crash after the Option change.
- Ingestion worker (`ingest/jobs.rs`) fetches `Option<LlmClient>`; if `None`, it
  marks the job `failed` with a clear message instead of panicking.
- **Startup reaper** in `run_job_loop`: on boot, resets any `running` job back to
  `queued` (orphans from a crashed previous run). Runs once before the loop.

## Known-good verification

`SELECT status, count(*) FROM ingestion_jobs GROUP BY status;` shows all jobs in
`failed` (expected — no model yet), none stuck in `running`. Upload → hash →
insert → enqueue → claim → graceful-fail works end to end.

## Open issues

Resolved this session:
1. ~~`Admin.tsx:32` console error — `file_path`/`adapter_path` mismatch~~ —
   **fixed**: `department_adapters` always had the column named `adapter_path`;
   `commands/admin.rs` was querying a nonexistent `file_path`. Both
   `cmd_list_adapters` and `cmd_add_adapter` were broken by this (not just
   listing, as previously noted here).
2. **No RLS-scoped admin path** — `cmd_create_department`/`cmd_add_adapter`/
   `cmd_create_user` had zero authorization checks (any session could call
   them), and `cmd_list_departments` leaked every department in the org to
   every user regardless of membership. **Fixed**: `users.is_admin` (first
   user created becomes admin), `require_admin()` gates the two mutation
   commands, and `cmd_list_my_departments` (RLS-scoped, migrations/005) is
   what the Documents-page picker uses now instead of the admin-only listing.
3. **Ingestion ran as the `postgres` superuser** (`ingest_pool` was a clone of
   `admin_pool`) — violated CLAUDE.md invariant #3. **Fixed**: dedicated
   `ingest_worker` role + `INGEST_DATABASE_URL` (see env vars above).
4. **LoRA adapter ids were fabricated** via `ROW_NUMBER()` over
   `department_adapters` rows instead of the sidecar's real loaded-adapter
   index — would misassign adapters once real inference is wired up,
   especially when one adapter file serves multiple departments. **Fixed**:
   `LlmClient::refresh_adapter_index()` reads `GET /lora-adapters` from the
   sidecar and resolves by path; `spawn()` now also loads active
   `department_adapters.adapter_path` rows as `--lora` args at boot.
5. **Blob store was never implemented** — uploaded bytes were hashed then
   discarded; `ingest_document` used a hardcoded placeholder string.
   **Fixed**: `cmd_upload_document` writes to `{app_data_dir}/blobs/{file_hash}`;
   `ingest_document` reads it back (still lossy-UTF-8 decoded — real
   PDF/DOCX parsing is still future work, same as before).

Still open (deferred, not blocking Plan A):
6. **Stale status badge** — uploaded docs show `PENDING` in the UI even though the
   DB has them as `failed`. The poller only tracks jobs from the current session;
   docs present on reload aren't polled. Deferred: once the model works, docs get
   re-uploaded and this becomes moot (badge will show real `ready`/`failed`).
7. **Swallowed frontend errors** — upload failures go to the browser console, not
   the UI. Cosmetic; worth surfacing later.
8. **`.env` file** not set up (see env vars above — now three vars to remember).
9. **Not committed to git.** This is the root cause behind issues 1-5 above:
   schema drift and stale doc claims went unnoticed because nothing was ever
   diffed or reviewed. Strongly recommend `git init` + an initial commit
   before making further changes.

## NEXT: Plan A — install real llama-server

Replaces the 27-byte placeholder at
`src-tauri/binaries/llama-server-x86_64-pc-windows-msvc.exe`.

**Hardware:** Acer Nitro V — i7-13620H, **RTX 4050 Laptop (6 GB VRAM)**, 16 GB RAM.
→ CUDA build. 6 GB VRAM ceiling means a ~7B model at Q4 (~4.5 GB), mostly in VRAM
with some layers spilling to RAM. Qwen 2.5 7B Q4 matches what the code assumes.

**Downloads (from github.com/ggml-org/llama.cpp/releases, latest release):**
- `llama-<build>-bin-win-cuda-x64.zip` (the server + tools)
- `cudart-llama-bin-win-cuda-*-x64.zip` (CUDA DLLs — **required**; without them
  llama-server.exe starts and silently does nothing). Extract into the SAME
  folder as the exe.
- A base model GGUF (~7B Q4, e.g. Qwen 2.5) placed where the code expects it:
  `models/base.gguf`.

**Sidecar spawn (current, in `src-tauri/src/llm/mod.rs`):**
- Base URL `http://127.0.0.1:8080`, health-checked at `/health`.
- Args: `--port 8080 --lora-init-without-apply --model models/base.gguf`, plus
  one `--lora <adapter_path>` per row currently in `department_adapters`
  where `is_active = true` (read from the DB at spawn time). After the
  health check passes, `refresh_adapter_index()` calls `GET /lora-adapters`
  on the sidecar and caches path→id so `llm/adapters.rs` can resolve the
  *real* sidecar id per request instead of guessing one.

**⚠️ CRITICAL GAP for ingestion:** the spawn has **no `--embedding` flag**, so the
server won't serve `/v1/embeddings`. Ingestion needs embeddings, so even after the
binary + base model are in and generation works, **ingestion will still fail until
embedding support is added** (either `--embedding` + a pooling flag on this
server, or a second small embedding model/server). Solve this right after the
server boots.

**Recommended approach:** test `llama-server.exe` **standalone from the command
line first** (with the model, by hand) before touching the Tauri wiring — so any
failure is isolated to the binary/model, not the integration. Then wire, then
solve embeddings.

## Working style that's been effective

- One step at a time; wait for output before the next step. No "if X then Y"
  branching in instructions.
- When a runtime error says "column X does not exist," the schema is right and the
  code is wrong — rename in the code (SQL/`.get()`/`.bind()` only).
- The DevTools console (F12 in the app window) is where frontend errors surface;
  the terminal is where Rust/DB errors surface. Check both.
