# ADR-002: Process model

- Status: Accepted
- Date: 2026-11-01

## Context

The vtx prototype was a WASM-component kernel with a plugin for every
responsibility. It required wasmtime, a plugin ABI, a multi-language toolchain
and a five-registry release process before any product behavior existed. The
plugin ABI changed three major versions in three weeks and the runtime database
was never exercised.

## Decision

- B1 is a **single binary, single process, modular monolith**.
- Internal boundaries are enforced by four crates:
  `web -> api -> media -> kernel`.
- No WASM runtime, no dynamic plugin loading, no catch-all HTTP gateway in v1.
- Extension seams are internal traits (ADR-008). A future extension runtime is
  a separate, additive decision (ADR-002 is not silently amended).

## Consequences

- Fast iteration and debuggability; one `cargo run`, one log stream.
- The product can ship before the extension ecosystem exists.
- The boundaries are conventions plus crate dependencies; they must be reviewed,
  not assumed. Kernel/media/API import rules are part of code review.
