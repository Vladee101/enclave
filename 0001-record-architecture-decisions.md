# ADR-0001: Record architecture decisions

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

This is a portfolio system where the reasoning behind the architecture is at
least as important as the code. Decisions made silently during design are
invisible to a reviewer and easy to forget the motivation for later.

## Decision

Use Architecture Decision Records. Each significant decision gets a numbered
file in `docs/adr`, written in MADR style: context, decision, alternatives
considered, consequences. Accepted records are immutable; a decision that
changes is superseded by a later ADR rather than rewritten.

## Alternatives considered

- **A wiki page** — drifts out of sync with the code and keeps no per-decision
  history. Rejected.
- **Commit messages only** — not discoverable as a set, and the rationale gets
  buried. Rejected.
- **No record** — loses the reasoning, which is the most valuable part here.
  Rejected.

## Consequences

**Positive**
- The decision trail is auditable; a reviewer (or interviewer) can follow the
  thinking end to end.
- Superseding rather than editing preserves the history of why things changed.

**Trade-offs**
- A small, ongoing upkeep cost to write a record when a real decision is made.
