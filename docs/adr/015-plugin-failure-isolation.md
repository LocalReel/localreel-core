# ADR-015: A failed plugin degrades, it does not stop the server

- Status: Accepted
- Date: 2026-11-08

## Context

Until now the loader treated every configured plugin as a precondition: the
startup loop called `inspect`, `extract` and `PluginRuntime::start` with `?`, so a
single unreadable package, a handshake timeout or a typo in `[plugins.config]`
made `serve()` return `Err` and the process exit. Every other optional
subsystem — metadata, ffmpeg discovery, file watchers — degrades with a warning
instead.

The failure mode is worst on the desktop: the same process serves the web UI, so
a broken optional provider means the UI never starts, and the operator has no
page in which to switch the plugin off. The only recovery is editing
`localreel.toml` by hand.

At the same time the plugin list reported `running: true` as a literal, so the
one place that could have explained the failure had no way to.

## Decision

1. **Failure isolation is the default.** A plugin that cannot start is recorded
   as a `failed` entry with its error, written to the plugin audit as
   `start.failed`, and the server continues. `disabled` entries are recorded
   too, so the list describes the configuration rather than only successes.
2. **Strictness is opt-in.** `[plugin_runtime] strict = true` restores the old
   "refuse to boot" behaviour for operators who prefer it (a server with a
   required provider).
3. **States are derived, not asserted.** `PluginState` has `running`, `failed`,
   `disabled` and `stopped`; `running` in the JSON is computed from the state.
   A supervisor milestone adds restart counts and transient states on top.
4. **A stopped plugin makes its sources unavailable, not wrong.**
   `StorageRegistry` remembers the kinds the configuration declares; resolving a
   source for a declared-but-not-running kind returns
   `KernelError::StorageUnavailable` (`503`), while an undeclared kind keeps the
   old generic error (`500`).
5. **Stops have a budget.** `[plugin_runtime] stop_timeout_secs` (default 5)
   bounds how long a stop waits for in-flight requests before the process is
   killed. The registry learns how to `unregister` a provider, which is what
   makes a drain possible.

## Consequences

- A misconfigured plugin costs its own functionality and nothing else. The UI
  comes up and explains which package failed and why.
- Adding a plugin can no longer break a running installation at boot, but it can
  now *silently* fail unless someone reads the admin page — hence the audit row
  and the visible state.
- The host gains a mutable plugin set (`RwLock`), which is the shape the
  supervisor needs; the API stays additive (`GET /api/v1/plugins` only gains
  fields).
- Strict mode keeps a real use: NAS/container deployments that would rather
  fail their health check than serve a library whose backend is missing.
