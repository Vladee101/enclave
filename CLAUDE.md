# Enclave — Corporate RAG + LoRA desktop app

> Working name "Enclave" (placeholder — rename freely). The name should evoke
> data staying sealed on-premises, which is the entire point of the product.

## What this file is

A build brief for the coding agent. **The source of truth for *why* is
`docs/adr/`** — twenty-three accepted Architecture Decision Records (0001–0010, 0014–0025; 0011 superseded by 0021). This file says *what
to build, in what order, and which invariants must never be broken.* When a
decision here seems arbitrary, the matching ADR explains it. Do not contradict
an ADR; if reality forces a change, write a new ADR that supersedes the old one
(ADR-0001 — records are immutable).

## Product in one paragraph

A desktop application that runs an entire corporate knowledge assistant locally:
documents, embeddings, the LLM, and every query stay on the user's machine or
LAN (ADR-0002). It answers questions over the organization's own documents (RAG)
in a department-appropriate voice (per-department LoRA adapters, ADR-0004), and
guarantees that a user in one department can never retrieve another
department's content — enforced in the database, not the app (ADR-0008).

## Stack

- **Shell/UI:** Tauri 2 + React + Vite + TypeScript.
- **Core:** Rust. Async on tokio. DB via `sqlx`. Vectors via `pgvector`.
  HTTP via `reqwest`.
- **Datastore:** PostgreSQL 16+ with the `pgvector` extension. One database for
  relational data, vectors, and full-text (ADR-0005).
- **Inference:** llama.cpp `llama-server`, bundled as a Tauri sidecar
  (ADR-0003). One resident base model (Qwen3-4B, ADR-0024 — downloaded on first run) + hot-swappable LoRA
  adapters selected per request.
- **Embeddings:** a local 768-dim model (e.g. `nomic-embed-text`). The dimension
  must match `vector(768)` in the schema.

## Existing artifacts (provided — place, don't rewrite)

| File                  | Destination                          | Notes |
|-----------------------|--------------------------------------|-------|
| `schema.sql`          | `db/schema.sql` (or `migrations/`)   | Tables, pgvector/tsvector indexes, RLS helpers + policies, roles, seed. |
| `docs/adr/*`          | `docs/adr/`                          | Eleven ADRs + index. Read before changing anything structural. |
| `retrieval.rs`        | `src-tauri/src/retrieval.rs`         | Hybrid search + RRF fusion + LoRA-aware generation + provenance logging. |
| `ingest.rs`           | `src-tauri/src/ingest.rs`            | Background ingestion worker (claims jobs, extract → chunk → embed → write). |
| `rls_isolation.rs`    | `src-tauri/tests/rls_isolation.rs`   | Integration test proving cross-department isolation. |

## Hard invariants — DO NOT VIOLATE

1. **The query path connects as `app_user`** (NOSUPERUSER, NOBYPASSRLS). Never a
   superuser, never a BYPASSRLS role. Connecting with elevated privileges
   silently disables every RLS policy (ADR-0008).
2. **Set `app.current_user_id` transaction-local** — `set_config('app.current_user_id', $id, true)`
   — inside the *same transaction* as every user-scoped query, and re-set it in
   each new transaction. It is the identity RLS keys off; unset = zero rows.
3. **Only the ingestion worker may use `ingest_worker`** (BYPASSRLS). Because it
   bypasses RLS, it is solely responsible for stamping the correct
   `department_id` onto `chunks` and `chunk_embeddings` (ADR-0009). No other
   component touches that role.
4. **Never hold a DB transaction open across an LLM or embedding HTTP call.**
   Both `retrieval.rs` and `ingest.rs` already follow this; preserve it.
5. **The `lora` payload uses the server's integer adapter id**, resolved through
   the path→id map (`LlamaClient::refresh_adapter_index`), never the DB UUID.
6. **Embedding dimension == `vector(768)` == the `embedding_models` row.** A
   mismatch fails at insert; that is the intended failure point.
7. **RLS is the security boundary.** The explicit `department_id` filters in the
   retrieval SQL are for scope and ANN speed only — never rely on them for
   isolation. The `rls_isolation` test exists to enforce this distinction.
   Policies compute membership **once per query** (ADR-0021):
   `department_id = ANY ((SELECT current_user_department_ids())::uuid[])` —
   never a per-row function call; under RLS every search scans the
   department's rows, so the per-row cost of a policy is the query's cost.
8. **Two pools, two roles.** `app_pool` (app_user) for all user-facing work;
   `ingest_pool` (ingest_worker) only for the worker.
9. **Documents are deleted only through `delete_document()`, departments only
   through `delete_department()`** (ADR-0015, ADR-0017): a document by its
   uploader or an admin, a department by an admin and never the default one —
   checked in the database. `app_user` has no DELETE on
   `documents`, may UPDATE only `status`/`updated_at`, and cannot write
   `chunks`/`chunk_embeddings` at all. Do not grant these back to make a
   feature work — add a SECURITY DEFINER function with its own check.
10. **`app_user` writes only what the app writes on `app_pool`** (ADR-0018):
    INSERT on `documents`, `ingestion_jobs`, `audit_log`, and UPDATE of
    `documents.status`/`updated_at`. Everything else is read-only for it.
    A new write through `app_pool` needs an explicit GRANT in the migration
    that introduces it — default privileges no longer hand out writes, and
    `rls_validation` section 9 fails if a forbidden write comes back.

11. **Table calculations compile a validated plan, never model-written SQL**
    (ADR-0022). The planner's output is data: it is constrained by a JSON
    schema built from the table's own columns, checked against the catalog
    (`tables::plan::parse_plan`), and compiled into an aggregate whose SQL
    text comes only from fixed fragments — column positions and values are
    bind parameters. It runs on `app_pool` inside the caller's
    identity-scoped transaction, so RLS decides the rows. `sheet_tables` /
    `sheet_rows` are written only by the ingestion worker (department_id
    stamped as for chunks) and purged with their document by trigger. A plan
    that fails falls back to retrieval, never to an approximate number.
    A plan the user picked in a clarification (ADR-0023) arrives from the
    client and gets exactly the same treatment: its table is loaded by id
    under RLS (`load_candidate`), the plan goes through `parse_plan`, the
    SQL through the same compiler. Never trust it more than model output.

## Build tasks (in order)

1. **Scaffold** Tauri 2 + React + Vite + TS. Confirm the dev shell runs.
2. **`Cargo.toml`** deps: `sqlx` (features: postgres, runtime-tokio, uuid,
   macros), `pgvector` (feature: sqlx), `reqwest` (json), `serde`/`serde_json`,
   `uuid` (v4), `async-trait`, `anyhow`, `tokio` (full). Exact set is listed at
   the top of `retrieval.rs`.
3. **Database provisioning.** Create the two roles (`app_user` from `schema.sql`;
   `ingest_worker` per the header comment in `ingest.rs`). Apply `schema.sql` as
   the initial migration. Provide a migration runner (sqlx-cli or a small
   startup migration step). See "PostgreSQL packaging" below — this is the
   hardest part.
4. **Connection pools.** Build `app_pool` (app_user) and `ingest_pool`
   (ingest_worker) at startup; pass them to the right components.
5. **Wire the core.** Place `retrieval.rs` and `ingest.rs` under
   `src-tauri/src/`, expose them from `lib.rs`.
6. **Tauri commands** (thin wrappers; keep logic in the core modules):
   - `authenticate(username, password)` → sets up the session user id.
   - `ask(query)` → `retrieval::answer_query(app_pool, …)`; returns answer +
     `sources` for citation display.
   - `upload_document(file)` → compute sha-256, write to the blob store, insert
     a `documents` row (status `pending`), enqueue an `ingestion_jobs` row.
   - `job_status(document_id)`, `list_documents(department_id)`.
   - Admin: manage departments, memberships, adapters, `department_adapters`.
     These run via a privileged path (provisioning is outside app_user's RLS
     reach by design).
7. **Start the worker** as a background tokio task on app launch:
   `IngestionWorker::new(ingest_pool, embedder, extractor, cfg).run()`.
8. **Sidecar.** Bundle `llama-server`; on startup load the base model and the
   adapters (`--lora-init-without-apply`), then call
   `LlamaClient::refresh_adapter_index`.
9. **Embedder.** Wire `LlamaEmbedder` (or equivalent) to the embeddings
   endpoint; register the matching `embedding_models` row and pass its id to the
   worker config.
10. **Frontend:** auth screen; chat view that renders `sources` as numbered
    citations; document manager with upload + job-status polling; admin views
    (departments, memberships, adapter assignment with per-assignment scale).
11. **Bootstrap:** a first-run flow that creates the initial admin user and
    department through the privileged path.
12. **Tests/CI:** wire `rls_isolation`. CI must provision PostgreSQL+pgvector and
    set `TEST_ADMIN_URL` (superuser/BYPASSRLS) and `TEST_APP_URL` (app_user), or
    the test silently no-ops.
13. **Write the three pending ADRs** (see Open decisions).

## Content-addressed blob store

Source bytes live at `{data_dir}/blobs/{file_hash}` (sha-256). This is why
`documents` stores `file_hash` and no path. Upload: hash → write blob → insert
document → enqueue job. The worker reads `{blob_root}/{file_hash}`.

## PostgreSQL packaging (biggest risk — decide early)

A self-contained desktop app needs a local PostgreSQL **with pgvector**, which
is non-trivial to ship because pgvector is a compiled extension. Options:
bundle a prebuilt Postgres+pgvector for each target OS; manage a local data dir
with a vendored server; or require the user to point at an existing local
Postgres. This choice is consequential enough to warrant its own ADR before
implementation.

## Verification

- `cargo test --test rls_isolation` with both env vars set must pass. This is the
  proof that ADR-0008 holds; treat a failure as a release blocker.
- Smoke flow: create two departments + users → upload a doc to each → confirm
  each user's chat only ever cites their own department's sources.

## Open decisions (write these ADRs)

- **0012 — Ingestion worker trust boundary.** Why ingestion runs as a BYPASSRLS
  role and what that obligates (department_id stamping). Extends ADR-0008.
- **0013 — Content-addressed blob storage.** The `{blob_root}/{file_hash}` store
  and the dedup it enables.
- ~~**0014 — PostgreSQL distribution.**~~ Decided and implemented
  (ADR-0014, `db/embedded.rs`, `scripts/fetch-postgres.ps1`): with no
  `*_DATABASE_URL` set the app runs a trimmed PostgreSQL 18 + self-built
  pgvector itself — `postgres.exe` spawned without a console, 127.0.0.1 on
  a free port, generated role passwords sealed with DPAPI. Still open:
  bundling it into the installer, a clean-machine check.
- **The schema is what the migrations build.** The dev database was once
  hand-built from `db/schema.sql` and still has columns no migration
  creates; code that relies on one works there and fails on every new
  install (it happened: `embedding_models.provider`, fixed by 016). A
  statement the app runs at startup belongs in `tests/schema_contract.rs`.
- (Also note, not necessarily an ADR) the `token_count` chars/4 heuristic is a
  deliberate simplification, commented in-line; revisit if it bites. Ingestion
  retries are no longer a simplification: transient (HTTP) failures are
  requeued with backoff up to 3 attempts (`ingest/jobs.rs`, migrations/008).
