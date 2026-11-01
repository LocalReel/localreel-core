# ADR-007: API and UI

- Status: Accepted
- Date: 2026-11-01

## Context

The previous API mixed bare arrays, envelopes, 200-with-error bodies and
English strings; the UI used polling for scans/transcodes and hardcoded
same-origin paths. The vtx gateway passed only method/path/query to plugins and
dropped headers/bodies, which made a real media API impossible.

## Decision

- Versioned base path: `/api/v1`.
- Stable error envelope: `{"error":{"code","message"}}` with correct HTTP
  statuses.
- `GET /api/v1/events` is an SSE stream of `job.*`, `scan.*`, `media.*` events.
  The UI treats events as invalidation hints and refetches authoritative data.
- Loopback is the default trust boundary, enforced by bind validation plus a
  Host/Origin guard. `LOCALREEL_ALLOW_REMOTE=1` is an explicit opt-in.
- The UI is a first-party React app (`localreel-web`), served as static assets
  by the server; it is not a plugin and does not own routing/history in v1.
- Media bytes are served by the host (`/api/v1/media/{id}/stream` later), never
  proxied through an extension runtime.

## Consequences

- The API contract can evolve (`/api/v2`) without breaking old clients.
- The UI stops polling; one event stream drives scan/job/library refreshes.
- Remote access needs an explicit token/auth story before it is enabled; not in
  v1.
