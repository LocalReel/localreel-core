# ADR-017: Plugins are supervised, not merely started

- Status: Accepted
- Date: 2026-11-08

## Context

ADR-015 made a failed start a reported state instead of a fatal error. That
fixed the bricked-desktop case but left the lifecycle one-shot: after the first
successful start nothing watched the child. If the process died, the provider
stayed registered and every request answered "storage error"; if it hung, health
was never checked again; if the operator wanted to retry, the only tool was
restarting the server.

Two more gaps came from the same root: a stop killed the process while requests
were still streaming through it (`Drop` → `TerminateProcess`), and a host that
crashed left its children running, holding loopback ports. Every reference
plugin spawns without a process group or job object, so "the child dies with the
host" was a convention, not a guarantee.

## Decision

1. **One supervisor thread owns transitions.** Callers only express intents
   (`restart`, `set_enabled`); the thread converges observed state towards the
   desired state. There is exactly one writer, so a start superseded by a newer
   intent cannot race it.
2. **Crashes are retried with backoff** (1, 2, 4, 8, 16 s, five attempts). A
   failure that cannot change — an unreadable package, an invalid manifest, a
   rejected API version — is permanent and is reported immediately instead of
   being retried five times.
3. **Health is checked, not assumed.** `GET /v1/health` runs every 10 s with a
   2 s budget. Three consecutive failures mark the plugin `unhealthy` (the
   process is kept, the UI tells the truth); six replace the process. Three
   health-driven restarts in a row give up and leave the entry `failed`.
4. **Stops drain before they kill.** The provider's gate stops accepting calls,
   the stop waits up to `[plugin_runtime] stop_timeout_secs` (default 5) for
   in-flight requests, then unregisters the provider, closes stdin (the
   documented graceful path), and finally kills the process tree. `open_range`
   readers hold an in-flight guard, so a streaming body counts.
5. **The process tree belongs to the host.** Windows children are assigned to a
   Job Object with `KILL_ON_JOB_CLOSE`; Unix children get their own process
   group and are signalled as a group. A host crash no longer leaves orphans.
6. **Control is administrative and auditable.** `restart`, `enable` and
   `disable` are admin endpoints; `disable?persist=true` rewrites `enabled` in
   the configuration file through `toml_edit` (preserving comments), while the
   default is runtime-only. Every transition writes an audit row (`start`,
   `stop`, `crash`, `start.failed`) and publishes `plugin_state_changed`.
7. **Diagnostics are part of the contract.** The last 64 KiB of a plugin's
   stderr are kept and served at `GET /api/v1/plugins/:id/logs`; audit rows older
   than 30 days are pruned daily.

## Consequences

- A plugin that dies at 3 a.m. is back within a second or two, and the admin page
  says so. A plugin whose remote is down keeps answering — it is its own
  process — but it is reported `unhealthy` rather than silently failing.
- Restart latency is bounded by the plugin's handshake (15 s) plus the drain
  (5 s); the API returns when the intent is accepted, and the UI refreshes on the
  state event.
- Retries cannot paper over a configuration error: the permanent/transient split
  is the difference between "the NAS is rebooting" and "your manifest is wrong".
- The supervisor is one thread, so a plugin that blocks its own startup delays
  other plugins' health checks by the same amount. With the expected plugin
  counts (0–3) that is a deliberate trade: no scheduler, no lock ordering, one
  place to read.
- Writing the configuration file is opt-in per request. A UI click never edits a
  file the operator maintains unless they asked for it.
