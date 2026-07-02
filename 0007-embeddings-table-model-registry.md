# ADR-0007: Separate embeddings table with a model registry

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

A chunk's embedding is a function of the chunk *and* the model that produced it.
Embedding models improve, and re-embedding a corpus with a better model is a
realistic future need. The storage design should not make that a destructive
operation, and it must still support a usable ANN index.

## Decision

Store vectors in a `chunk_embeddings` table keyed by `(chunk_id,
embedding_model_id)`, with an `embedding_models` registry recording each model's
name and dimension. Pin the vector column to a fixed dimension (768, the
nomic-embed-text / bge-base class) so a single shared HNSW index is possible.

## Alternatives considered

- **A vector column directly on `chunks`** — re-embedding becomes a destructive
  overwrite, and there is no way to hold two models' vectors at once. Rejected.
- **A bare `vector` column with no fixed dimension** — a single HNSW index then
  cannot be built; you would need one partial index per model. This is the
  documented escape hatch if running multiple embedding dimensions at once ever
  becomes a requirement, but it is not the default. Rejected for now.

## Consequences

**Positive**
- Re-embedding with a new model is additive, not destructive.
- Clear provenance: every vector knows which model produced it.

**Trade-offs**
- The fixed dimension allows only one dimension per shared index (mitigation:
  partial index per model, as above).
- Retrieval pays one extra indexed join from `chunks` to `chunk_embeddings` —
  acceptable.
