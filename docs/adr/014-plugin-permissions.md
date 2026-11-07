# ADR-014: Plugin permissions are enforced, not documented

- Status: Accepted
- Date: 2026-11-07

## Context

Until now a plugin declared `permissions: ["network"]` and the host checked the
spelling. The sidecar then ran as an ordinary child process: it inherited the
host's environment, including `LOCALREEL_DATA_DIR` and `LOCALREEL_CONFIG`, so it
could open the database directly and read the whole library. The permission list
was decoration, and the admin UI had nothing truthful to show.

The lesson comes from the previous prototype (`vtxdeo`): loading plugins as Root
at startup was treated as "security" when it was the opposite. A capability the
host never checks is a claim, not a guarantee.

## Decision

Keep the sidecar model — no OS sandbox, no in-process foreign code — and make
the declarations real where the host actually controls the boundary:

1. **Cleared environment.** `Command::env_clear()` plus an explicit whitelist
   (`PATH`, `HOME`/`USERPROFILE`, `LANG`, `LC_ALL`, `TEMP`/`TMP`, and on Windows
   `SYSTEMROOT`/`WINDIR`). The host's own paths are never inherited.
2. **Own working directory.** `<data>/plugins/<id>/`.
3. **Only its own roots.** `LOCALREEL_PLUGIN_ROOTS` carries the source URIs whose
   `kind` equals the plugin id — not the library.
4. **A host proxy for the network.** `POST /api/v1/plugin-proxy`, authenticated
   with the token the host generated for that sidecar, requires `network`, and is
   the only sanctioned way out. It has a timeout and a size cap.
5. **An audit trail.** `plugin_audit` records starts, allowed requests and
   refusals; the admin page shows the effective permissions and recent activity.
6. **Effective permissions.** A `storage` plugin always has `storage:read`; the
   host reports the effective set, so an older manifest keeps loading while the
   UI still tells the truth about what the plugin can do.

## Consequences

- A plugin that declares nothing still runs and still scans files — but it cannot
  reach the library through the host, and everything it does through the proxy is
  attributed and audited.
- Hosts and plugins now share a contract change: plugins that want the network
  must call the proxy instead of opening sockets themselves. That is a real
  migration cost for plugin authors and the reason the protocol version did not
  change: the old manifests stay valid, they simply get no proxy.
- Refusing honestly beats blocking silently: the `403` from the proxy is a
  documented, audited answer rather than an unexplained connection failure.
