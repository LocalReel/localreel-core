# ADR-011: Two personas, one binary

- Status: Accepted
- Date: 2026-11-07

## Context

LocalReel ships to two audiences that need opposite launch behaviour:

- a **desktop** user who double-clicks an icon and expects the interface to
  appear, a second click to show the running instance instead of an error, and
  a clear message if something is wrong;
- a **server/NAS** operator who starts the program from systemd, Docker or a NAS
  package and expects a fixed port, no windows, and logs that survive the
  terminal.

Splitting this into two programs would duplicate the server, the web UI and
every future feature, and would let the two halves drift.

## Decision

One binary, one `RunMode` (`desktop` | `server`), resolved once at startup:

1. `--mode <desktop|server>` wins.
2. `LOCALREEL_MODE` is next.
3. Otherwise it is detected: a container (`/.dockerenv`, `KUBERNETES_SERVICE_HOST`,
   `container`), or an SSH session, is always `server`; on Windows a console
   allocation means desktop; on Linux a graphical session (`DISPLAY`,
   `WAYLAND_DISPLAY`) means desktop; on macOS the default is desktop, and the
   LaunchAgent we ship sets `LOCALREEL_MODE=server` explicitly.

The persona only changes launch behaviour, never the API or the data model:

| Behaviour | desktop | server |
|---|---|---|
| Open the UI | yes, unless `--no-open`/`LOCALREEL_NO_OPEN` | never |
| Busy port | try the next 10 ports | fail with an actionable message |
| Second instance | detect it, open its UI, exit 0 | service manager owns restarts |
| Failure output | stay visible, wait for Enter | log and exit non-zero |

`GET /api/v1/health` reports `name` and `mode`, so a launcher can tell a running
LocalReel from any other process on the port without an HTTP client dependency.

## Consequences

- Packaging decides the persona through flags and service units, not through
  code forks; the same binary is the desktop app and the NAS server.
- Detecting "was a human involved" is a heuristic. The explicit override exists
  for every case where the heuristic is wrong, and the effective mode plus its
  source are printed by `localreel doctor`.
- Server mode never invents a port: a reverse proxy, container mapping or
  bookmark stays valid.
