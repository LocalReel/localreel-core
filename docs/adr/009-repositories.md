# ADR-009: Repositories and versioning

- Status: Accepted
- Date: 2026-11-01

## Context

The vtx ecosystem split into eight repositories, five of which existed to build
and distribute plugins. For B1 (no plugin runtime) that split is premature; it
adds cross-repo path dependencies and release coordination without an
independent consumer.

## Decision

- Product name: **LocalReel**. Repositories use the `localreel-` prefix.
- v1 repositories:
  - `localreel-core` — server workspace (kernel/media/api/server) plus the
    `localreel-format` crate for the shared backup/artifact schema, migrations,
    ADRs, CI.
  - `localreel-web` — React UI.
- `localreel-format` was extracted early, but with no independent consumer the
  crate now lives in `localreel-core/crates/localreel-format`; the standalone
  repository is archived.
- Reserved names, created only when a real consumer exists:
  `localreel-cli`, `localreel-protocol`, `localreel-sdk`,
  `localreel-ffmpeg-release`, `localreel-toolchains`, `localreel-link`.
- Versioning: one product version for the app (`0.x` until the v2 API/DB are
  stable). Crates in `localreel-core` share the workspace version. Do not
  invent per-crate versions or tag-injected manifest versions.
- The old implementation and the vtx snapshot live under `.tmp/` and are
  reference-only.

## Consequences

- CI for `localreel-core` is a normal single-repo workspace; no cross-repo
  checkout tricks.
- The backup format is a workspace crate, so schema changes and migrations land
  in the same commit and release.
- Adding a repository is a product decision, not a default.
