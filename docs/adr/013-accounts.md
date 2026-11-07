# ADR-013: Accounts and per-user state

- Status: Accepted
- Date: 2026-11-07

## Context

LocalReel assumed exactly one implicit user: one library, one `media_state` row
per item, and no authentication at all (ADR-007). That worked for a single
desktop, but the product now also targets a household server (a NAS or a small
always-on machine) that several people reach over the LAN from their own
devices.

Two consequences follow from sharing one library:

1. Watch progress, likes and favourites are personal. Two people watching the
   same series must not overwrite each other.
2. A server reachable from the network cannot be anonymous. The previous
   answer — "bind loopback only" — is not available to a NAS deployment.

## Decision

- The database owns accounts: `users` (username, Argon2id password hash,
  `admin`/`user` role, enabled flag) and `sessions` (opaque token, CSRF token,
  expiry).
- The accounts migration creates the **owner** account (id 1) with an empty
  password. An empty password means "not configured yet": authentication always
  fails, and the API reports `setup_required` until the first-run screen sets
  one. An upgraded database hands every existing state row to the owner, so
  watch history survives.
- Sessions are opaque random tokens stored in the database, not signed cookies,
  so an operator can end a session by disabling the account or resetting the
  password. The cookie is `HttpOnly` and `SameSite=Lax`; a second, readable
  cookie carries the CSRF token that every state-changing request must echo.
- `media_state` is keyed by `(media_id, user_id)`. Feeds, `Continue`/`Favorites`
  filters and the state endpoints take the signed-in account; the library
  itself stays shared.
- The HTTP surface is split into public (health, auth), authenticated
  (library, playback, own state, events) and administrator (sources, scans,
  jobs, backups, accounts) groups. The administrator group checks the role,
  not just the session.
- Administrators manage accounts. The owner can be neither demoted, disabled
  nor deleted; a password change ends that account's other sessions.
- The loopback default and the explicit remote opt-in stay: accounts make a
  LAN bind possible, they do not make it the default.

## Consequences

- One migration changes the meaning of progress: every read or write of
  `media_state` must name an account. Code paths without a user (legacy import,
  CLI restore) use the owner explicitly.
- Backup export/import and restore are scoped to the calling account for now;
  a household-wide backup needs a format version that names accounts.
- No external identity provider, no OAuth, no per-object permissions: a viewer
  sees the whole library and only their own state. Multi-user is about
  identity and progress, not about hiding media.
- The first-run screen becomes mandatory for a new install and for an upgraded
  database that has never had a password; the server still starts, but every
  API call is unauthorized until setup completes.
