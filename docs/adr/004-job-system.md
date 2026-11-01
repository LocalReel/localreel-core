# ADR-004: Job system

- Status: Accepted
- Date: 2026-11-01

## Context

The previous app had two bespoke job models: persisted scan jobs with an
in-memory status mutex, and an in-memory `HashMap<i64, JobState>` for transcodes
with no id, progress, cancellation or restart recovery. The vtx prototype had a
lease-based `sys_jobs` table but hard-wired two job types, never let plugins use
it, and had a poison-job requeue loop.

## Decision

- One persistent queue: `sys_jobs` (kind, payload JSON, payload version,
  dedupe key, status, progress, attempts, lease, timestamps).
- States: `queued`, `running`, `succeeded`, `failed`, `canceled`.
- Claim uses `BEGIN IMMEDIATE`; workers renew a lease via heartbeat.
- Retry has exponential backoff and is bounded by `attempts`; expired leases
  increment attempts and eventually fail, so a poison job cannot loop forever.
- Cancellation is cooperative: queued jobs are canceled directly; running jobs
  get an in-process flag observed by the handler.
- `dedupe_key` makes "probe this media" enqueueing idempotent while a job is
  queued/running.
- Every transition publishes an event consumed by the SSE endpoint.
- Handlers are registered by the media crate; the kernel never knows what a
  scan or transcode is.

## Consequences

- Scan/probe/thumbnail/transcode all share one observable, restart-safe model.
- The UI can subscribe to progress instead of polling.
- The queue is intentionally simple (no adaptive concurrency, priorities are
  stored but unused in v1). Add complexity only when a measured need exists.
