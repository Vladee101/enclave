# ADR-0011: Defer removal of dormant memberships-based RLS policies

- Status: Accepted
- Date: 2026-07-02
- Deciders: Vlad (Softwarean)

## Context

While writing `migrations/005_role_alignment_and_columns.sql` (column
alignment, the `ingest_worker` role, RLS on `departments`/`department_members`),
verification against the live dev database showed it is not a clean product
of `migrations/001-004`. `db/schema.sql` — the original reference design,
which was never supposed to go live — had been hand-applied via `psql` at
some earlier point, before migrations ever ran. Because `002_core_tables.sql`
uses `CREATE TABLE IF NOT EXISTS` and `004_rls.sql` guards its `CREATE ROLE`
with an existence check, several objects from that earlier hand-apply
survived instead of being replaced by the migration's own versions: a
`memberships` table (distinct from `department_members`), a `roles` table,
the functions `app_current_user_id()` / `app_user_in_department(uuid)`, and
seven RLS policies built on them, layered alongside the real, currently-used
policies (`is_member_of()` / `department_members`-based). Confirmed directly
via `pg_policies`:

| Table              | Dormant policy             |
|--------------------|-----------------------------|
| `departments`      | `dept_visible`              |
| `documents`        | `doc_dept_isolation`        |
| `chunks`           | `chunk_dept_isolation`      |
| `chunk_embeddings` | `emb_dept_isolation`        |
| `ingestion_jobs`   | `ingestion_dept_isolation`  |
| `conversations`    | `conv_owner`                |
| `query_logs`       | `qlog_owner`                |

`memberships` is confirmed empty (`SELECT count(*) FROM memberships` → 0)
and no application code path writes to it — `department_members` is the
table every command actually reads and writes. So today these seven
policies are inert: Postgres OR's multiple permissive policies for the same
command together, and `app_user_in_department()` always evaluates false
when `memberships` has no matching row for the caller. But they are a live
landmine, not a no-op by design: a future `INSERT` into `memberships` (a
stray migration, a manual fix applied the same way this one was, copy-pasted
admin tooling) would silently widen access on seven tables with no error and
no visible signal, running alongside the real access-control logic that
CLAUDE.md invariant #7 and the `rls_validation` test are meant to guarantee.

Migration 005 originally attempted
`DROP POLICY doc_dept_isolation ON documents; DROP FUNCTION app_user_in_department(uuid);`
as part of its RLS section. The `DROP FUNCTION` failed: the function is
depended on by six more policies across five more tables that the migration
hadn't accounted for.

## Decision

Defer the removal. Migration 005 ships without touching any of the seven
dormant policies or the `app_user_in_department()` / `app_current_user_id()`
functions or the `memberships` / `roles` tables — it only adds what
CLAUDE.md's invariants require (the `ingest_worker` role, column alignment,
RLS on `departments`/`department_members`).

Removing the dormant layer is scoped to a dedicated future migration that
drops one table's dormant policy at a time — suggested order: `documents` →
`chunks` → `chunk_embeddings` → `ingestion_jobs` → `departments` →
`conversations` → `query_logs` — confirming after each drop that the real,
`is_member_of()`/`department_members`-based policy on that table still
produces identical query results (re-run `rls_validation.rs`, plus a manual
spot check per table), before finally dropping `app_user_in_department()`,
`app_current_user_id()`, and the `memberships`/`roles` tables themselves.

## Alternatives considered

- **Drop everything in migration 005** — rejected. The blast radius (seven
  tables, seven policies, two functions, two tables) is large relative to
  what migration 005 is actually for (column alignment plus one new role),
  and a change touching that much RLS surface is exactly the kind of thing
  CLAUDE.md invariant #7 says must be tested with care — not folded into an
  unrelated fix under time pressure.
- **Drop only `doc_dept_isolation` and leave the rest** — rejected. That was
  migration 005's original (failed) attempt. Removing one of the seven
  dormant policies but not the others leaves an inconsistent, partially
  cleaned state without reducing the actual risk — the function and the
  other six policies still exist and are still a landmine.
- **Leave it forever, undocumented** — rejected. `memberships` being empty
  today is not a guarantee it stays that way, and an undocumented landmine
  is worse than a documented one.

## Consequences

**Positive**
- Migration 005 stays scoped and reviewable: it does exactly what CLAUDE.md's
  invariants require, nothing else.
- The dormant policies are confirmed inert today — `rls_validation.rs`
  passes against the live database with both policy sets present, and
  `memberships` has zero rows.
- The cleanup plan (table-by-table, verify-then-drop) is written down before
  anyone forgets why these objects exist.

**Trade-offs**
- The landmine remains live until the dedicated cleanup migration lands.
  Anyone touching RLS on these seven tables should know both policy sets
  exist — `\d <table>` in `psql` (or `pg_policies`) shows all policies on a
  table, dormant and real together — and should not assume a single policy
  per table governs access.
- Schema introspection (`\d documents`, `\d chunks`, etc.) will look
  redundant and confusing to anyone reading the live schema until cleanup
  happens. That confusion now has a documented answer: this ADR.
