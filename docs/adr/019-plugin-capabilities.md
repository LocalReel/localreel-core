# ADR-019: Capabilities are disclosed, granted per version, and attributed

- Status: Accepted (phase 1; phase 2.1–2.5 broker; phase 3.1 pinning; 3.2 signatures; 3.3 resource ceilings; 3.4 stdio transport; 3.5 key rotation; 3.6 verification records; 3.7 AppContainer primitive; 3.8 stdio broker; 3.9 AppContainer runtime; 3.10 fs.read broker; 3.11 reference storage migration; 3.12 handle-based read verification)
- Date: 2026-11-08

## Context

The v1 plugin contract has two permissions (`storage:read`, `network`). They are
useful declarations, but they answer "may this plugin try?" rather than "what did
the host actually allow, for which version, and who approved it?".

The CMS/collector probe made the gap concrete:

- a collector needs an allowlisted HTTP client, host-held credentials, a
  schedule, and a catalog write path — none of which a single `network` flag can
  describe;
- a plugin can open its own socket, so `network` is an audit label today, not an
  enforced capability;
- the host proxy (GET-only, 30 s, 32 MiB, no headers) is too narrow for a real
  collector, which pushes the plugin to bypass the host entirely;
- an upgrade can add a permission with no re-consent, and an old row cannot tell
  which version the operator approved.

## Decision

1. **The host owns the capability vocabulary.** A capability is a name plus a
   typed, bounded scope (`net.fetch`, `secret.use`, `schedule.request`,
   `catalog.write`, `fs.read`, `blob.read`, `blob.write`). The host publishes the
   title, description, tier and audit level; a plugin cannot describe its own
   safety.
2. **The manifest requests; it does not grant.** `capabilities` is an array of
   `{name, scope}`. Unknown names, malformed scopes, duplicates and the
   forbidden list (`process.spawn`, `host.database`, `net.listen`, ...) are
   rejected before extraction. Old `permissions` remain valid and are shown as
   `legacy` requests.
3. **A grant is per plugin version.** `plugin_grants` stores the normalized
   scope, digest, tier, grantor, timestamps and revocation. An upgrade with a new
   capability requires a new grant; revoking a capability takes effect without
   restarting the host (the broker checks active grants per call).
4. **Audit is the attribution format.** `plugin_audit` gains capability, scope
   digest, result, bytes, duration, request/job id and user id. Lifecycle events
   keep working. The host writes the row; the plugin cannot.
5. **Phase 1 is honest about enforcement.** The vocabulary, grants, admin API and
   audit are real; the broker that executes the capabilities is phase 2, and OS
   isolation is phase 3. Until then a grant is a disclosed intent, not a
   privilege that has been exercised. The protocol document says so.
6. **Phase 2.1 enforces `net.fetch`.** The plugin calls
   `POST /api/v1/plugin-broker/fetch` with its per-run token; the host checks the
   active grant for the exact version, the scope (hosts/methods/bytes/timeout/
   redirects/rate), the SSRF policy and the header allowlist, writes the audit row
   and only then returns the body.
7. **Phase 2.2 adds `secret.use`.** The plugin names a handle; the host checks the
   active `secret.use` grant, the handle/host scope and the injection rule, reads
   the value from the host store, injects it, audits handle+host (never the
   value) and forbids redirects so the credential cannot leave the scope. The
   value is never returned by any API.
8. **Phase 2.3 adds `catalog.write(draft)`.** The plugin submits normalized
   titles; the host validates counts/fields/URLs, checks the active grant and
   upserts `plugin_catalog_drafts`. `publish` is rejected: promotion into the
   browsable `cms_*` tables is a reviewed admin action. Play URLs are untrusted
   draft data and must be validated again before playback.
9. **Phase 2.4 adds `schedule.request`.** The host stores one schedule per plugin
   version, bounds the interval by the grant, wakes due rows with
   `GET /v1/schedule?payload=...`, advances the next run even on failure, and
   audits `schedule.run`. The plugin cannot run its own timer through this
   capability.
10. **Phase 2.5 adds `blob.read`/`blob.write`.** The host owns the namespace,
   per-blob cap (16 MiB) and total quota; names cannot traverse paths. Reads and
   writes are separate grants; deletes need write. Blob contents are not written
   to audit rows.
11. **Phase 3.1 pins the package artifact.** A configured `sha256` is checked
    before inspect/extract; a mismatch is a permanent start failure.
12. **Phase 3.2 adds publisher signatures.** `[[publishers]]` stores trusted
    Ed25519 public keys; a plugin may name `publisher` and `signature` over the
    package digest. The supervisor verifies before extraction; the config file is
    the trust anchor. A transparency log is not implemented.
13. **Phase 3.3 adds resource ceilings.** `[plugin_runtime] max_memory_mb` and
    `max_processes` bound one plugin tree: Windows applies both to the Job Object,
    Unix applies `RLIMIT_AS` (process count is not enforced on Unix yet). This is
    resource containment, not network isolation.
14. **Phase 3.4 adds a stdio transport.** `entry.transport = "stdio"` uses
    newline-delimited JSON-RPC over stdin/stdout; the plugin listens on nothing.
    The storage adapter and SDK support both transports. This removes the need
    for a listening port, not the ability to open one: OS network isolation is
    still required.
15. **Phase 3.5 adds key rotation/revocation.** Several `[[publishers]]` entries
    may share an id; only keys with `revoked = false` and a future `not_after`
    verify. Config validation requires at least one active key for a publisher a
    plugin references. This is configuration lifecycle, not a transparency log.
16. **Phase 3.6 records package verification.** `plugin_package_verifications`
    stores plugin/version, package digest, publisher, key fingerprint, result and
    time. The supervisor appends before extraction; admins read it at
    `/api/v1/plugins/verifications`. It is a local audit trail, not a public log.
17. **Phase 3.7 validates the Windows network primitive.** An AppContainer child
    with no capabilities cannot connect out (`WSAEACCES 10013`) and cannot reach
    host loopback (timed out against a live listener). The prototype copies the
    child and `vcruntime140.dll` into the profile folder. It is not wired into
    `PluginHost` yet; the HTTP broker must move to inherited stdio or a named pipe
    first.
18. **Phase 3.8 moves the broker onto stdio.** `type=broker.request` frames on
    the plugin's stdout are proxied by the host to its own broker API (only
    `/api/v1/plugin-broker/` paths, host token added). `BrokerClient` selects it
    with `LOCALREEL_PLUGIN_BROKER=stdio`; scheduled callbacks use the same pipe.
    This is what lets an AppContainer plugin use `net.fetch` without loopback.
19. **Phase 3.9 wires AppContainer into the runtime.** `[plugin_runtime]
    app_container = true` launches stdio/native plugins through
    `plugin-sandbox`, which copies the install directory into a per-run
    AppContainer profile and starts the child with no capabilities. Tests prove
    the broker works over stdio and a raw `1.1.1.1:80` connect is denied. The
    wrapper and the runtime both delete the profile on normal exit/kill; a
    startup sweep removes profiles whose marker PID is no longer alive.
    Interpreter scripts are rejected.
20. **Phase 3.10 brokers `fs.read`.** A plugin sends `file://` URIs to
    `POST /api/v1/plugin-broker/fs`; the host checks the active `fs.read` grant,
    canonicalises each root and the target, refuses targets outside all roots,
    caps reads at 8 MiB and audits allow/deny. On Windows, `read` verifies the
    final path from the opened handle, closing the symlink-swap TOCTOU for reads;
    `stat`/`list` and non-Windows reads remain path-based.
21. **Phase 3.11 migrates the reference storage plugin.** `fs_mode =
    "broker"` forwards URIs to the fs broker; the AppContainer storage test
    serves stat/list/read this way. Third-party plugins that omit the mode still
    use legacy direct roots until migrated. The HTTP transport remains the
    default; phase 3 continues with Unix isolation and broader migration.

## Consequences

- The admin page can answer "what does this plugin ask for, what did we grant,
  for which version, and when was it revoked" before any broker exists.
- Adding a capability is a host-side schema change plus tests, not a new string
  in a list.
- The UI must render host-owned descriptions and risk tiers; it must not trust
  plugin-supplied prose.
- Phase 2 can implement `net.fetch`/`secret.use` against a stable grant object,
  and phase 3 can replace the loopback transport without changing the grant or
  audit model.
- Old storage plugins keep loading; their `storage:read`/`network` permissions
  appear as legacy requests until they declare typed capabilities.
