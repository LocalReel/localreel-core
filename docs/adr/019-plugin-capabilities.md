# ADR-019: Capabilities are disclosed, granted per version, and attributed

- Status: Accepted (phase 1)
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
