# ADR-0010: Asynchronous document ingestion

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

Ingesting a document — parsing, chunking, embedding each chunk — is slow and can
fail partway through (a malformed file, an embedding call that times out). Doing
this inline on upload would block the UI and offer no retry or visibility into
what went wrong.

## Decision

Run ingestion as background jobs tracked in `ingestion_jobs`
(`queued` / `running` / `succeeded` / `failed`, with attempt count and error
text). Upload returns immediately and the UI polls job status — the same
202-plus-polling shape used elsewhere in the portfolio. `documents.status`
reflects the document's lifecycle from `pending` to `ready` or `failed`.

## Alternatives considered

- **Synchronous ingestion on upload** — blocks the UI, has no retry path, and
  degrades badly on large documents. Rejected.
- **Fire-and-forget with no tracking** — no visibility, no retry, and failures
  vanish silently. Rejected.

## Consequences

**Positive**
- The UI stays responsive; ingestion is retry-able and observable per document.
- Failures are inspectable, with the error captured on the job row.

**Trade-offs**
- Adds a job runner in the Rust core and a polling path in the UI.
- Introduces eventual consistency between "uploaded" and "ready", surfaced
  honestly through `documents.status`.
