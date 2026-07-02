# ADR-0008: Department-scoped Row-Level Security

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

The central risk in a multi-department RAG system is cross-department leakage:
the retriever surfacing a passage the asking user is not cleared to see. If
isolation is enforced only in application code, a single missing `WHERE` clause
in any retrieval path leaks data, and the database cannot save you.

## Decision

Enforce department isolation in PostgreSQL with Row-Level Security. Every
department-scoped table carries `department_id`. Policies key off a
per-transaction session variable (`app.current_user_id`) and a membership-check
function. RLS is set to `FORCE` so the table owner is also subject to it, the
application connects as a non-superuser `app_user`, and policies fail closed
(zero rows) when the session variable is unset.

## Alternatives considered

- **Application-layer filtering only** — not defense in depth; one bug leaks
  everything, and the database offers no backstop. Rejected.
- **A schema or database per department** — heavy, complicates shared catalogs
  and cross-department administration, and is painful to migrate. Rejected.
- **A Postgres role per department** — role explosion, and it maps badly onto a
  user who belongs to several departments. Rejected.

## Consequences

**Positive**
- The database is the last line of defense; even a buggy retrieval query cannot
  return rows outside the user's departments.
- Audit and provenance logging are unaffected and sit behind the same boundary.

**Trade-offs**
- Policies must be written and tested with care.
- The app must never connect as a superuser or a `BYPASSRLS` role, or RLS
  silently does nothing.
- The session variable must be transaction-local (`set_config(..., true)`) under
  connection pooling, or one request's identity can leak into the next.
