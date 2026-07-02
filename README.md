# Architecture Decision Records

This directory records the significant architecture decisions for the corporate
RAG + LoRA desktop application, in [MADR](https://adr.github.io/madr/)-style
format. One file per decision, numbered sequentially. Records are immutable once
accepted — a decision that changes is *superseded* by a new ADR rather than
edited in place, so the reasoning trail stays intact.

Each record states the context, the decision, the alternatives that were
weighed and rejected, and the consequences (including the costs we accepted).

## Log

| #    | Title                                                        | Status   |
|------|--------------------------------------------------------------|----------|
| 0001 | Record architecture decisions                                | Accepted |
| 0002 | On-prem, data-sovereign desktop application                  | Accepted |
| 0003 | Bundle llama-server as a Tauri sidecar                        | Accepted |
| 0004 | Compose RAG with per-department LoRA adapters                | Accepted |
| 0005 | PostgreSQL + pgvector as the single datastore                | Accepted |
| 0006 | Hybrid retrieval with Reciprocal Rank Fusion                 | Accepted |
| 0007 | Separate embeddings table with a model registry              | Accepted |
| 0008 | Department-scoped Row-Level Security                          | Accepted |
| 0009 | Denormalize department_id onto chunks and chunk_embeddings   | Accepted |
| 0010 | Asynchronous document ingestion                              | Accepted |
| 0011 | Defer removal of dormant memberships-based RLS policies      | Accepted |
