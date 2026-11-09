# Linux network namespace sandbox

`[plugin_runtime] app_container = true` maps to a Linux network namespace for
stdio plugins:

```text
unshare -Urn -- <plugin-program> <args...>
```

- `-U` creates a user namespace and `-r` maps the invoking user to root inside
  it, so the plugin can still read its install directory without extra ACLs.
- `-n` creates a fresh network namespace with no route to the host or the
  internet. The stdio broker is the only path out; it uses inherited pipes.
- A missing `unshare` binary fails plugin startup (no ambient-network fallback).
- macOS has no equivalent here and is rejected at config validation.

## Validation status

Validated in WSL (Alpine, Rust 1.88, util-linux `unshare`):

- `unshare -Urn true` succeeds.
- The Rust runtime test
  `appcontainer_denies_the_plugins_own_socket` passes on Linux: the plugin sees
  `net: blocked` for `1.1.1.1:80`.
- `linux_isolation_uses_a_network_namespace` asserts the exact
  `unshare -Urn -- <program> <args>` command.
- `cargo clippy -p localreel-plugin --all-targets -- -D warnings` is clean.

Other loopback-fixture runtime tests in this WSL image fail with connection
refused / HTTP health timeouts. Diagnosis: `netstat` shows the plugin
`LISTEN 127.0.0.1:<port>`, while `wget` in the same shell gets connection
refused; this is Docker Desktop WSL networking, not the netns path. The Windows
AppContainer path has the full runtime/full-chain coverage.

## Limits

- `unshare` must be installed (util-linux or BusyBox).
- The plugin runs as namespace root; the host kernel's user-namespace policy
  still applies.
- There is no per-run profile directory to clean up, unlike AppContainer.
- This isolates the network, not the filesystem: a plugin can still read paths
  its Unix user can read. Use the `fs.read` broker for path-scoped reads.
