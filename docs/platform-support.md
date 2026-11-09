# Platform support

What the host actually enforces per platform. "Policy" means the broker/grant
checks; "OS" means a kernel boundary that survives a malicious plugin.

| | Windows | Linux | macOS |
|---|---|---|---|
| `stdio` transport | yes | yes | yes |
| loopback HTTP transport | yes | yes | yes |
| `app_container = true` | AppContainer + Job Object | `unshare -Urn` (netns) | **rejected** |
| OS network isolation | yes (AppContainer) | yes (netns) | no |
| OS filesystem isolation | AppContainer token | no (user namespaces only) | no |
| `fs.read` handle check | `GetFinalPathNameByHandleW` | `/proc/self/fd` | path only |
| Resource ceilings | Job Object memory/processes | `RLIMIT_AS`, no process count | `RLIMIT_AS` |

## Windows

- `app_container = true` requires `transport = "stdio"` and a native executable;
  scripts are rejected.
- The `plugin-sandbox` wrapper must sit next to the server binary (or be named
  by `LOCALREEL_PLUGIN_SANDBOX`).
- The plugin gets no loopback; every broker call goes over stdio.
- AppContainer profiles are cleaned by the wrapper, runtime drop and startup
  sweep.

## Linux

- `app_container = true` requires `unshare` and permission to create user and
  network namespaces. `GET /api/v1/plugins/sandbox-status` runs
  `unshare -Urn true` once and caches the answer.
- The plugin gets a fresh network namespace; the stdio broker is its only
  channel.
- Filesystem is **not** isolated: the plugin can still open any file the Unix
  user can read. `fs.read` is the only sanctioned file path and checks
  `/proc/self/fd/<fd>` for `stat`/`read`; `list` remains path-based.

## macOS

- `app_container = true` is rejected at config validation. There is no
  network-namespace equivalent and the deprecated `sandbox-exec` is not wired
  in.
- Treat macOS plugins as policy-only: use `transport = "stdio"`, grant the
  narrowest capabilities, and do not claim OS isolation in the UI or docs.
- `fs.read` is path-based; no final-path check from the handle.

## HTTP transport

`transport = "http"` is the legacy loopback form. It cannot be combined with
`app_container`; the plugin owns a TCP port and can open its own sockets. Use
`stdio` for anything not fully trusted.
