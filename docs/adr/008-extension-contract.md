# ADR-008: Extension contract

- Status: Accepted — sidecar storage plugins (WASM still out of scope)
- Date: 2026-11-01 (updated 2026-11-04)

## Context

The vtx plugin system inverted its own security model (plugin code ran as root
before capabilities were known), used one all-imports WIT world (ABI lockstep),
and made the HTTP gateway the only interface (no headers/body/user). It also
required an auth plugin before the admin API worked.

A local-first single-binary product does not need a general WASM platform. It
does need a way to add storage providers (WebDAV, SMB, object storage) without
putting third-party code into the server process.

## Decision

- The in-process seams stay traits: `Storage` (stat/list/open_range/walk) is the
  provider contract used by scanning and streaming.
- Third-party code runs as a **sidecar process**, never in the host:
  1. The package is a zip with `manifest.json` at its root; id, API version,
     kind, entry point and permissions are validated **before** extraction or
     execution.
  2. The host extracts into its own directory (path-confined, no symlinks,
     bounded size), starts the entry program with a per-run token and JSON
     config, and waits for one `ready` line with a bounded timeout.
  3. All calls go over loopback HTTP with the token; the protocol is documented
     in `docs/plugin-protocol.md`.
  4. `PluginStorage` implements the `Storage` trait for such a plugin, so the
     scanner and the HTTP layer cannot tell a plugin from a local directory.
- Protocol version is explicit (`localreel.plugin/v1`). A newer plugin is
  rejected, not best-effort interpreted.
- Plugins receive no host database handle and no capabilities beyond the
  configuration the operator supplies.
- WASM/tiered worlds remain out of scope; a future runtime must satisfy the same
  manifest-first, capability-explicit rules.

## Consequences

- A storage provider can be developed, versioned and shipped independently.
- The host and plugin can recover from each other's crashes; the token bounds
  local misuse.
- Remote providers still rely on the host materialising bytes for probe,
  thumbnail and transcode; that pipeline is a separate concern from the storage
  contract.
- The sidecar adds a process per plugin and a handshake; for a single-user local
  app this is an acceptable trade for isolation.
