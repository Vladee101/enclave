# ADR-0003: Bundle llama-server as a Tauri sidecar

- Status: Accepted
- Date: 2026-06-29
- Deciders: Vlad (Softwarean)

## Context

The system needs local LLM inference with per-request LoRA adapter selection
(see ADR-0004). There are two broad ways to obtain local inference: depend on a
runtime the user installs separately, or ship our own inside the app.

## Decision

Bundle llama.cpp's `llama-server` as a Tauri sidecar binary. The Rust core talks
to it over localhost HTTP. Adapters are preloaded at startup with
`--lora-init-without-apply`, and each request selects the active adapters with a
`lora: [{ id, scale }]` field.

## Alternatives considered

- **Require Ollama** — turns the product into a client of software the user must
  install and manage, adding a setup step and an external dependency. Ollama's
  LoRA hot-swap support has also trailed llama.cpp's per-request adapter
  selection, which is central to ADR-0004. Rejected.
- **Rust-native inference (candle / mistral.rs)** — attractive for a pure-Rust
  stack, but carries more integration risk and, at decision time, less mature
  LoRA-serving than llama.cpp. Revisit later. Rejected for now.
- **Cloud inference** — excluded by ADR-0002. Rejected.

## Consequences

**Positive**
- The app is self-contained: one install, no external runtime.
- Per-request adapter selection with sub-20ms swap latency, because the base
  model stays resident in memory and only the adapter pointer changes.

**Trade-offs**
- Larger application bundle.
- We own packaging and shipping the sidecar binary for each target platform.
