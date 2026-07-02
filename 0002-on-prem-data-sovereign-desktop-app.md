# ADR-0002: On-prem, data-sovereign desktop application

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

The target users are organizations whose documents cannot leave their
environment — regulated industries, GDPR-bound EU companies, teams with
sensitive internal IP. For these buyers, the first and hardest objection to any
AI tool is "where does our data go." The product's reason to exist is that the
answer is *nowhere*: documents, embeddings, the model, and every query stay on
the user's machine or LAN.

## Decision

Ship as a desktop application — a Tauri shell with a React UI and a Rust core —
that runs all inference and storage locally. The default execution path makes no
network calls off the machine.

## Alternatives considered

- **Web SaaS** — would defeat the sovereignty premise entirely. The premise is
  the differentiator, so this is a non-starter. Rejected.
- **Electron desktop** — heavier runtime, larger binaries, and a weaker security
  posture than Tauri's Rust core. Rejected.
- **Cloud LLM API over on-prem data** — data still leaves the environment for
  inference, which is exactly the objection we are answering. Rejected.

## Consequences

**Positive**
- The strongest possible data-residency story, which is the whole pitch.
- Tauri yields small binaries and a Rust core suited to the heavy lifting
  (ingestion, retrieval orchestration, talking to the inference sidecar).

**Trade-offs**
- Distribution and updates are per-machine rather than a single deploy.
- Performance is bounded by the user's hardware — acceptable, and consistent
  with a local-inference audience.
