# ADR-008: Extension contract

- Status: Accepted (v1: no runtime; seams only)
- Date: 2026-11-01

## Context

The vtx plugin system inverted its own security model (plugin code ran as root
before capabilities were known), used one all-imports WIT world (ABI lockstep),
and made the HTTP gateway the only interface (no headers/body/user). It also
required an auth plugin before the admin API worked.

## Decision

- v1 does **not** load `.vtx` or any third-party code. It defines internal
  seams only:
  1. `SourceProvider` — list/stat/open-range for a source kind.
  2. `MetadataProvider` — probe/title/artwork enrichment.
  3. `TranscodePreset` — named, typed transcode profiles.
  4. UI panels — later, if a real need appears.
- If an extension runtime is added later, it must be **manifest-first**:
  `{id, version, protocol_version, capabilities, entrypoint, migrations}` in
  the package metadata is read before any guest code runs.
- Worlds are split by capability tier (`source-v1`, `metadata-v1`,
  `transcode-v1`), not one all-imports world.
- Extensions get their own SQLite file (`data/ext/<id>.db`), never DDL rights on
  the main database.
- No catch-all HTTP routing. Extension routes are explicit and cannot touch
  media byte streams.
- A sidecar process contract is an acceptable cheaper alternative to WASM for
  trusted local extensions.

## Consequences

- The product ships without the ABI/versioning burden.
- When extensions are added, the security boundary is enforceable because the
  manifest is known before code execution.
- The kernel/media modules must keep their seams clean even while no runtime
  exists; otherwise the future ABI will be designed around accidental coupling.
