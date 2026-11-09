# 020 - Self-update through a signed manifest

Status: accepted

## Context

The console has shown "there is a newer release" since 0.5.5, but the download is
a link and the upgrade is manual. A per-user installation lives under
`%LOCALAPPDATA%\Programs\LocalReel`, needs no elevation, and already ships
`localreel-setup.exe`, which knows how to stop, replace and register itself.

The reason it was never automated is the one the old card stated: silently
replacing a binary is a supply-chain hole. GitHub's release JSON is a
convenience, not an authority; TLS only proves who served the bytes.

## Decision

1. **A signed manifest is the authority.** `update.json` describes one release
   (version, notes, per-platform assets with size and SHA-256) and is signed
   with ed25519. `update.json.sig` holds the detached signature, hex encoded.
   The public key is compiled into the binary
   (`crates/localreel-update/pubkey.txt`); the private half exists only as the
   `UPDATE_SIGNING_KEY` secret of the release workflow.
2. **Verification happens before anything is offered.** The server fetches the
   manifest and its signature, verifies both, refuses a version that is not
   newer or one the installation is too old to jump to, and only then downloads
   the asset — HTTPS only, size-capped, written to `.part`, checked against the
   manifest size and digest, and renamed only when both match.
3. **The installer does the swap, not the server.** Only a per-user setup can
   replace itself without an administrator. The server spawns the verified
   installer with `/S /wait /launch` and stops itself; the setup waits for the
   process to exit (Windows will not replace a running executable), installs,
   and starts the program again.
4. **Nothing runs without a click.** The default is check-and-offer. There is no
   background install, no channel switching, and no downgrade.
5. **Unsupported installations say so.** An MSI needs elevation and a portable
   copy has no installer: both keep the release link and explain why the button
   is missing instead of failing after a download.

## Consequences

- A mirror or an internal server can host updates (`[update] manifest`), but it
  cannot forge one: verification is against the compiled-in key.
- Rotating the signing key means shipping a release that carries the new public
  key, then switching the secret. Until that release is installed, older
  binaries reject manifests signed with the new key — which is the intended
  behaviour, not a bug.
- The update path adds no dependency: `ed25519-dalek`, `sha2`, `ureq` and
  `serde` were already in the workspace for the plugin platform.
- Rollback is deliberately out of scope for this version. Migrations are
  forward-only, so a downgrade would need the previous database too; the
  console keeps the release link for that case.
- The first release that understands the manifest is 0.5.6, and the manifest's
  `min_version` reflects it.

## Alternatives considered

- **Trust the GitHub API only** (the 0.5.5 behaviour): no signature, no
  verified download; a compromised account or a malicious mirror wins.
- **WinSparkle / Squirrel / Omaha**: a second runtime and a different packaging
  model for a decision this small; the primitive we need was already in the
  workspace.
- **Replace files in place from the server**: impossible on Windows while the
  executable is mapped, and it would duplicate the installer's work (shortcuts,
  registration, uninstall entry) with a worse failure mode.
