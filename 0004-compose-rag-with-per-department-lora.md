# ADR-0004: Compose RAG with per-department LoRA adapters

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

Retrieval-augmented generation and fine-tuning are often presented as competing
choices. They solve different problems. RAG injects fresh, factual, retrievable
knowledge into the context at query time. LoRA adapts *behavior* — house tone,
domain jargon, structured-output formats — by shifting the model's weights with
a small trained adapter. An enterprise tool wants both: current facts and a
department-appropriate voice.

## Decision

Compose them. One base model stays resident in memory. Retrieval (ADR-0006)
supplies the factual context; the querying user's department determines which
LoRA adapter(s) shape the response, swapped in per request (ADR-0003). Adapter
assignments live in `department_adapters`, whose `scale` maps directly onto the
inference request payload.

## Alternatives considered

- **RAG only** — no per-department control over voice or output format. Rejected.
- **A full fine-tuned model per department** — storage and compute blow up, you
  lose hot-swapping, and the model's facts go stale between trainings. Rejected.
- **Prompt-only personas** — weaker and less reliable than a trained adapter,
  and they consume context budget on every request. Rejected.

## Consequences

**Positive**
- A single base model serves many departments cheaply.
- Clean separation of concerns: RAG owns *what it knows*, LoRA owns *how it
  answers*.

**Trade-offs**
- The app *serves and swaps* adapters; it does not *train* them. Training is an
  offline pipeline (GPU + dataset); the app ships with a few demo adapters and a
  bring-your-own-adapter path. This boundary is stated plainly so it is not
  oversold.
- Requests using different adapter sets cannot be batched together — negligible
  at single-user desktop scale.
