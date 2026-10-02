# Enclave — Session Handoff

On-premises RAG + LoRA desktop app (portfolio piece). This file is the
current state for a new session: what works, how to run it, what bites,
what is open. The *why* is in `docs/adr/`; the rules are in `CLAUDE.md`.
Verify anything here against the code before relying on it.

## TL;DR

Feature-complete for a single machine and shipped as a Windows installer.

- **Runs end to end on a live model.** Upload (PDF / DOCX / XLSX / XLS / ODS
  / TXT / MD) → ingestion → hybrid search with citations → streamed answers
  from Qwen3-4B; table questions compute exact numbers from a validated plan
  (ADR-0022, 0023).
- **Department isolation is the database's job** (RLS, ADR-0008/0021), and
  since migration 017 the department copied onto chunks and table rows must
  match the parent's (ADR-0012).
- **Self-contained:** the app runs its own PostgreSQL 18 + pgvector
  (ADR-0014); the llama.cpp engine and both models download on first run for
  the machine's GPU (ADR-0024, 0025). NSIS installer, per user, ~22 MB.
- **Backups:** one archive with the database and files; a restore is checked,
  staged, and swapped in at the next start (ADR-0026).
- **UI in Russian and English**; errors from the core carry codes the UI
  words (`src-tauri/src/error.rs`, `src/i18n/`).

## Layout

- Project: `C:\Users\vlade\Desktop\enclave-anti\enclave` (the nested one).
  The parent `enclave-anti/` holds docs, CLAUDE.md and the git root.
- Core: `src-tauri/src/` — `commands/` (thin Tauri wrappers), `retrieval/`,
  `ingest/`, `tables/` (plans + aggregates), `llm/` (sidecars, model and
  engine downloads), `db/` (pools, RLS helper, embedded PostgreSQL),
  `backup.rs`, `error.rs`.
- Schema: `migrations/001-018`, applied at every start. `db/schema.sql` is
  historical reference only — **the schema is what the migrations build.**
- Frontend: `src/` — pages, `components/`, `i18n/{en,ru}.ts` (typed: a key
  missing in one fails `tsc`).

## Running

**Default (what users get):** no env vars → embedded PostgreSQL in
`%APPDATA%\com.softwarean.enclave\pgdata`, passwords sealed with DPAPI in
`db-secrets.bin`. Needs `src-tauri/binaries/pg` (`scripts/fetch-postgres.ps1`,
reads a local PostgreSQL 18 install, builds pgvector with MSVC).

```bash
cd enclave && pnpm tauri dev
```

**Server mode (development data in Docker):** container `enclave-db`
(`pgvector/pgvector:pg16`, host port 5433). Set all three or none:

```bash
ADMIN_DATABASE_URL=postgres://postgres:yourpass@localhost:5433/enclave
APP_DATABASE_URL=postgres://enclave_app:change_me_in_production@localhost:5433/enclave
INGEST_DATABASE_URL=postgres://ingest_worker:change_me_in_production@localhost:5433/enclave
```

**Tests** (Docker up):

```bash
TEST_ADMIN_URL=postgres://postgres:yourpass@localhost:5433/enclave \
TEST_APP_URL=postgres://enclave_app:change_me_in_production@localhost:5433/enclave \
ENCLAVE_REQUIRE_DB_TESTS=1 ENCLAVE_REQUIRE_BACKUP_TEST=1 cargo test --all-targets
```

Without the env vars the DB tests skip silently; `ENCLAVE_REQUIRE_*` makes
them fail instead. `backup_roundtrip` needs `binaries/pg` (skipped in CI).
CI runs on PostgreSQL 16 and 18.

**Table-question eval** (model servers up — e.g. a running dev app — and
the 50 000-row registry ingested for the case file's user):
`ADMIN_DATABASE_URL=… APP_DATABASE_URL=… cargo run --example table_eval`.
Run it after any change to the planner prompt, the plan checks or the
model; last run 13 / 13 on Qwen3-4B.

**Installer:** `powershell -File .\scripts\build-installer.ps1` →
`target\release\bundle\nsis\Enclave_0.1.0_x64-setup.exe`. Bundle settings
live in `src-tauri/tauri.bundle.json`, merged only by that script.

## Things that bite

- **Processes started from a Claude session run in its MSIX sandbox:**
  their writes to AppData land in `Packages\Claude_…\LocalCache`, not the
  real profile. Installing or testing the installed app must be done by the
  user from Explorer.
- **A running Enclave locks `target\debug\enclave.exe`**, so `cargo test`
  fails to link; use a separate `CARGO_TARGET_DIR`.
- **`sqlx::migrate!` embeds migrations at compile time**; `build.rs` reruns
  on `../migrations`, so a new migration needs a rebuild, not just a restart.
- **Restart is refused in dev builds** (a restarted process loses the Vite
  server → white window). Close and `pnpm tauri dev` again.
- **Tauri plugin versions:** the npm package and the crate must match
  major.minor (`tauri-plugin-dialog` is pinned `~2.7` on both sides), and
  CI installs with `--frozen-lockfile`.
- **The trimmed PostgreSQL has no `psql`.** Use `docker exec enclave-db psql`
  or a full local install for poking at databases.
- **Docker DB drift:** it was once hand-built from `db/schema.sql`, so it has
  extra columns and a duplicate HNSW index no migration creates. Code that
  works only there is a bug; add startup statements to
  `tests/schema_contract.rs`.
- **Stopping a backgrounded `pnpm tauri dev` leaves children running**
  (`enclave.exe`, both `llama-server.exe`, Vite on 1420); stop them too, or
  the next run fails on the port.
- **git push over this network** fails intermittently with TLS handshake
  errors; retry.

## Open

- **Clean-machine check: done** (ADR-0027, `docs/clean-machine-check.md`,
  VM via `scripts/make-test-vm.ps1`). Not covered: AMD/Intel with a real
  Vulkan driver, NVIDIA older than CUDA 12.4. VirtualBox here needs the
  Windows hypervisor and Memory integrity off (NEM hangs Windows setup) —
  turn both back on afterwards.
- **Code signing** (SmartScreen warns about an unknown publisher).
- **LoRA adapters:** wired end to end, never exercised with a real adapter.
- **OCR of scans: done** with Windows' own recognizer (ADR-0028). Open:
  tables on scans stay text (no calculations), mixed PDFs (some pages
  scanned) are read by their text layer only; Tesseract if real scans
  prove too much for Windows OCR.
- **Backups:** no encryption, no schedule (ADR-0026).
- **Model download errors** arrive as events with English text; not coded.

## Working style that works

- The user writes in Russian; docs (README, ADRs) are Russian, code and
  CLAUDE.md English. Terminology, decided: the UI says «отдел», the Russian
  docs and the schema «департамент» / `departments`.
- Commit and push only on the user's explicit go-ahead; they review first.
- Downloads (models, binaries, source archives) only after saying what,
  from where and how big.
- Numbers in docs come from measurements made in the session; a claim
  about a test means the test was seen failing without the change.
