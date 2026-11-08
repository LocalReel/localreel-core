# Windows AppContainer network probe

Status: validated and wired into `PluginRuntime` for `app_container = true`
(phase 3.9).

## Why

Loopback HTTP is ambient network. The probe test already shows a plugin can open
its own raw socket (`raw_socket=connected`). AppContainer is the user-mode
Windows primitive that can deny egress without a driver or admin firewall rules.

## What the example does

`crates/localreel-plugin/examples/appcontainer-probe.rs`:

1. `CreateAppContainerProfile` (or derive an existing SID).
2. `GetAppContainerFolderPath` for that SID.
3. Copy the child executable into the profile folder.
4. Copy `C:\Windows\System32\vcruntime140.dll` next to it. This is required for
   Rust's MSVC binaries; without it the child dies with `0xC0000022`.
5. `CreateProcessW` with `STARTUPINFOEXW` +
   `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, **no capabilities**.
6. Inherit stdin/stdout/stderr and wait.

## Result (this machine)

```text
> appcontainer-probe.exe localreel-probe-8 net-probe.exe
blocked: ... (os error 10013)
```

`10013` is `WSAEACCES`: the container process cannot reach `1.1.1.1:80`.
`cmd.exe /c echo hello` also runs inside the container, proving stdio and process
creation work. A minimal Rust `hello` runs after the VC runtime is copied,
isolating the failure to the missing runtime, not the container.

## Loopback result

A host `TcpListener` on `127.0.0.1:45678` was not reachable from the container:
the child timed out instead of connecting. So AppContainer cannot keep using the
loopback HTTP broker. The stdio broker (phase 3.8) now covers `net.fetch`,
`catalog.write`, `schedule.request` and blobs over the inherited pipe, so no
loopback is needed.

## Limits / next step

- `PluginRuntime` now launches `plugin-sandbox` when `[plugin_runtime]
  app_container = true`; the wrapper copies the install directory and the loader
  runtime into a per-run profile. Tests prove the stdio broker works and a raw
  `1.1.1.1:80` connect is denied.
- Interpreter plugins are rejected (the interpreter is outside the package).
- The wrapper deletes the profile on normal exit; `PluginRuntime::drop` repeats
  the cleanup after a kill. Server startup sweeps profiles whose
  `localreel.pid` marker points at a dead process; a crash before the marker is
  written can still leave one behind.
- Unix/macOS network isolation is still absent.
