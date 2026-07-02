# ADR-0006: Hybrid retrieval with Reciprocal Rank Fusion

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

Dense vector search captures semantic similarity but stumbles on exact tokens —
part numbers, person names, acronyms, error codes — that carry a lot of meaning
in enterprise documents. Lexical full-text search nails those exact terms but
misses paraphrase and synonymy. Each alone leaves a class of queries poorly
served.

## Decision

Run both retrievers and fuse them. A dense leg uses pgvector cosine ANN over
`chunk_embeddings`; a lexical leg uses Postgres `tsvector` full-text over
`chunks.content_tsv`. The two ranked lists are combined with Reciprocal Rank
Fusion in the Rust core.

## Alternatives considered

- **Dense only** — fails on exact identifiers, which are common and important in
  this domain. Rejected.
- **Lexical only** — no semantic recall; misses relevant passages that share no
  surface terms. Rejected.
- **Weighted blend of raw scores** — cosine distance and full-text rank are on
  incomparable scales, so a score blend needs fragile normalization and tuning.
  RRF is rank-based and needs none of that. Rejected.

## Consequences

**Positive**
- Robust across query types, from "find the clause about X" to "doc 4471-B".
- RRF is simple and essentially parameter-free.

**Trade-offs**
- Two indexes to maintain: HNSW on the embedding and GIN on the tsvector.
- A small amount of fusion logic lives in the application rather than the query.
