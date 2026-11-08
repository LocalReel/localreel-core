# Windows AppContainer network probe

Status: validated inside an example; **not yet wired into `PluginHost`**.

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

## Limits / next step

- Loopback to the host broker is **not** tested yet. AppContainer normally blocks
  loopback too; a broker over a named pipe (or an explicit capability) would be
  needed before isolated plugins can use `net.fetch` / `secret.use`.
- The plugin install directory (not just one exe) must be copied or ACL-granted,
  and write paths/profiles cleaned up.
- Integration belongs in `platform.rs` behind a config switch, with the probe
  test flipped to assert denial rather than documenting the bypass.
