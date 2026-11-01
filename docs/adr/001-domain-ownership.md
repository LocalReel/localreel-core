# ADR-001: Domain ownership

- Status: Accepted
- Date: 2026-11-01

## Context

The vtx prototype pushed the media domain into plugins while keeping
`videos`/`sys_scan_roots` in the kernel. The domain ended up split between the
kernel and plugins, owned by neither. The previous LocalReel monolith owned the
domain completely but had no seams.

## Decision

- The **kernel** owns generic capabilities only: configuration, SQLite,
  migrations, job queue, events, storage, typed media operations, auth guard.
- The **media** crate owns the media domain: sources, media metadata, state,
  scan history, artifacts, playback policy. It owns its schema and migrations.
- The **API** crate is a thin adapter: parse/validate, call the domain, map
  errors. No SQL, no ffmpeg, no domain policy.
- The **server** crate only wires modules and handles process lifecycle.
- "Video", "source", "thumbnail" and "transcode" must not appear in kernel
  types.

## Consequences

- Any new media feature changes `localreel-media` (or the API), not the kernel.
- The kernel can be reused for other domains without dragging the media schema
  along.
- Extension seams are added deliberately (ADR-008), not by making everything a
  plugin.
