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
  - `localreel-core` — server workspace (kernel/media/api/server), migrations,
    ADRs, CI.
  - `localreel-web` — React UI.
  - `localreel-format` — standalone format crate (backup/artifact naming). Not a
    build dependency of `localreel-core` yet; it becomes one when the backup
    API is implemented, either as a published crate or a workspace crate.
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
- `localreel-format` can be published independently later, but v1 does not
  block on it.
- Adding a repository is a product decision, not a default.
