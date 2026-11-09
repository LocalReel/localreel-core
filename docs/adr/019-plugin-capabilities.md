# ADR-019: Capabilities are disclosed, granted per version, and attributed

- Status: Accepted (phase 1; phase 2.1–2.5 broker; phase 3.1 pinning; 3.2 signatures; 3.3 resource ceilings; 3.4 stdio transport; 3.5 key rotation; 3.6 verification records; 3.7 AppContainer primitive; 3.8 stdio broker; 3.9 AppContainer runtime; 3.10 fs.read broker; 3.11 reference storage migration; 3.12 handle-based read verification; 3.13 full host/API chain; 3.14 AppContainer full chain; 3.15 execution-mode descriptor; 3.16 handle-based stat; 3.17 failed-entry sandbox intent; 3.18 Linux netns; 3.19 Linux command test; 3.20 verification hash chain; 3.21 sandbox readiness; 3.22 unshare probe; 3.23 signed chain head; 3.24 audit key ACL; 3.25 Linux fd verification; 3.26 anchor export; 3.27 platform support matrix; 3.28 anchor sidecar file; 3.29 anchor history; 3.30 anchor history rotation; 3.31 package preflight; 3.32 package staging; 3.33 staged package management; 3.34 config install/rollback)
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
    `/api/v1/plugins/verifications`. Rows form a SHA-256 hash chain
    (`/verifications/verify` recomputes it). It is a local audit trail, not a
    public log.
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
    `plugin-sandbox`; a `[[plugins]]` entry can override it. The wrapper copies
    the install directory into a per-run AppContainer profile and starts the
    child with no capabilities. Tests prove the broker works over stdio and a raw
    `1.1.1.1:80` connect is denied. The wrapper and the runtime both delete the
    profile on normal exit/kill; a startup sweep removes profiles whose marker
    PID is no longer alive. Interpreter scripts are rejected.
20. **Phase 3.10 brokers `fs.read`.** A plugin sends `file://` URIs to
    `POST /api/v1/plugin-broker/fs`; the host checks the active `fs.read` grant,
    canonicalises each root and the target, refuses targets outside all roots,
    caps reads at 8 MiB and audits allow/deny. On Windows, `stat` and `read` open
    first and verify the final path from the handle; on Linux they verify
    `/proc/self/fd/<fd>`. `list` and macOS remain path-based.
21. **Phase 3.11 migrates the reference storage plugin.** `fs_mode =
    "broker"` forwards URIs to the fs broker; the AppContainer storage test
    serves stat/list/read this way. Third-party plugins that omit the mode still
    use legacy direct roots until migrated. The HTTP transport remains the
    default; phase 3 continues with Unix isolation and broader migration.
22. **Phase 3.12 proves the full host/API chain.** An API integration test starts
    a real `PluginHost`, a real router, resolves a real `fs.read` grant and calls
    `PluginStorage` against a stdio storage plugin. The test also caught a
    content-type mismatch in the stdio broker proxy (`/fs` and JSON calls are now
    `application/json`, blobs remain octet-stream).
23. **Phase 3.13 combines AppContainer with that chain.** The same test runs with
    `app_container = true` and the `plugin-sandbox` wrapper: the plugin has no
    network capability, reaches the real API only through stdio, and the real
    `fs.read` grant still serves stat/list/read.
24. **Phase 3.14 exposes execution mode.** `PluginDescriptor` reports
    `transport` and `sandbox` so the admin UI can distinguish HTTP/stdio and
    none/AppContainer without reading logs. Failed/disabled entries keep the
    configured sandbox intent when the server resolves it.
25. **Phase 3.18 extends the OS boundary to Linux.** `app_container = true`
    runs stdio plugins under `unshare -Urn`, giving them a fresh network
    namespace with the stdio broker as their only channel. macOS is rejected.
    A Rust runtime test in WSL confirms the plugin sees `net: blocked`; other
    loopback-fixture tests fail in that WSL image due to its networking, not the
    netns path.
26. **Phase 3.20 chains the verification log.** Each verification row hashes the
    previous chain hash plus its fields; `verifications/verify` reports the
    checked count and the first broken row. Legacy rows without a chain hash are
    skipped. This detects local tampering, not a compromised database owner.
27. **Phase 3.21 exposes sandbox readiness.** `GET /api/v1/plugins/sandbox-status`
    returns the platform, whether this build can isolate, whether
    `plugin-sandbox` exists, and whether `unshare` works. The Linux value is a
    cached `unshare -Urn true` probe, not just a PATH lookup. Operators can
    self-check before enabling `app_container`; it is diagnostic, not a grant.
28. **Phase 3.23 signs the chain head.** The host generates an Ed25519 audit key
    under the data directory, signs each new chain head and stores the anchor.
    `verifications/verify` checks both the chain and the latest anchor signature.
    The key is `0600` on Unix; Windows removes inherited ACLs and grants the
    current user. The key is local; an attacker who can read the data directory
    can forge future anchors, so this is not a remote transparency log.
29. **Phase 3.26 exports the anchor for external publication.**
    `GET /api/v1/plugins/verifications/anchor` returns the latest chain hash,
    signature, public key and timestamp. The latest anchor is atomically written
    to `plugin-verification.anchor.json` beside the audit key, and every anchor
    is appended to `plugin-verification.anchors.jsonl`; a backup or external
    publisher does not need the API. The JSONL rotates to `.1` at 4 MiB and
    overwrites the previous `.1`, so long-running hosts keep a bounded window.
    Publishing that history elsewhere is an operator action; the host does not
    push it automatically.
30. **Phase 3.31 preflights a package without installing it.**
    `POST /api/v1/plugins/packages/inspect` reads the manifest and SHA-256
    without extracting or starting the plugin. With `publisher` and `signature`
    it verifies the digest against an active trusted key; the pair must be
    supplied together. The config remains the source of truth, so this is not a
    hot-install API.
31. **Phase 3.32 stages a verified package.** `POST /api/v1/plugins/packages/stage`
    reuses the verification, checks an optional expected digest, copies the
    package to `data/plugins/packages/<id>/<version>.lrplug` and writes a JSON
    metadata sidecar. It does not edit the config or start the plugin; deleting
    a staged version has no runtime effect.
32. **Phase 3.33 manages staged versions.** `GET /api/v1/plugins/packages`
    enumerates the metadata sidecars; `DELETE .../<id>/<version>` removes the
    package and metadata, returning `409 package_in_use` when a configured
    plugin's package path resolves to it. The filesystem remains the only staged
    state; there is no database table to desync.
33. **Phase 3.34 installs a staged version by updating the config.**
    `POST /api/v1/plugins/packages/install` requires verified stage metadata and
    a matching on-disk digest, then updates the matching `[[plugins]]` entry
    (matched by the current package path) or appends a new enabled entry. It
    never hot-swaps the supervisor: the response requires a server restart, so
    the config stays the single source of truth. Installing an older staged
    version is the rollback path.

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
