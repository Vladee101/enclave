# ADR-0009: Denormalize department_id onto chunks and chunk_embeddings

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

`department_id` is transitively determined by the document: a chunk belongs to a
document, and a document belongs to a department. Strict third normal form keeps
`department_id` only on `documents`. But the RLS predicate (ADR-0008) and the
ANN pre-filter (ADR-0006) both need to evaluate department membership against
the row being scanned — on the hottest path in the system.

## Decision

Copy `department_id` onto `chunks` and `chunk_embeddings`. This is a deliberate,
documented violation of 3NF, accepted for the performance and security
properties it buys.

## Alternatives considered

- **Keep it normalized and traverse to `documents` in every policy and filter** —
  puts a join or correlated subquery on every retrieval, and pgvector
  pre-filtering works best when the filter column is local to the indexed table.
  Rejected.
- **A materialized view carrying the denormalized column** — adds refresh lag
  and another object to manage, for no gain over storing the column directly.
  Rejected.

## Consequences

**Positive**
- The RLS predicate reduces to a single indexed membership-check call.
- ANN search can pre-filter by department efficiently, with the column local to
  the embedding row.

**Trade-offs**
- The redundancy must be kept consistent. The ingestion pipeline sets it from
  the parent document; an optional `BEFORE INSERT/UPDATE` trigger can have the
  database enforce it instead of trusting application code.
- Recorded here so the denormalization reads as a deliberate engineering choice,
  not an oversight.
