# ADR-0005: PostgreSQL + pgvector as the single datastore

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

The system needs relational data (users, roles, departments, documents, audit)
and vector similarity search over embeddings. That can be served by one engine
or by two specialized ones. On a user's machine, every additional engine is one
more thing to install, back up, secure, and keep consistent.

## Decision

Use PostgreSQL with the pgvector extension for everything: relational tables,
vector search, and full-text search, in one database.

## Alternatives considered

- **A dedicated vector DB (Qdrant, Weaviate, Milvus) alongside Postgres** — two
  systems to deploy and secure on-prem, plus cross-store consistency to manage
  between rows and their vectors. "We already run Postgres, we don't want
  another datastore" is also a real enterprise procurement argument. Rejected.
- **SQLite with a vector extension** — lighter, and used in earlier projects,
  but a weaker fit here: this system depends on Row-Level Security (ADR-0008)
  and a mature ANN index, both stronger in Postgres. Rejected.
- **An in-process index such as FAISS** — no transactional store and no RLS, so
  it cannot be the system of record. Rejected.

## Consequences

**Positive**
- One engine, with transactional consistency between rows and their vectors.
- RLS covers relational data and vectors uniformly.
- A smaller on-prem footprint.

**Trade-offs**
- pgvector's ANN is strong but not the fastest option at very large scale —
  acceptable, because per-organization corpora are bounded.
