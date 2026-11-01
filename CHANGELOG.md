# Changelog

## 0.1.0 — 2026-11-01

First usable LocalReel v2 (B1) build.

### Added

- Rust modular monolith: kernel jobs/DB/config, media domain, thin HTTP API.
- Library, Reels, player and Admin UI; embedded into the server binary.
- Real media scan, ffprobe metadata, thumbnails, direct playback with Range.
- Persistent jobs: scan, probe, thumbnail, transcode, cache GC, restore.
- Sources: add, enable/disable, archive (media retained), remap moved roots.
- Per-media state: liked/favorite/progress; Reels resume.
- Backup v2 export/import; one-command CLI `restore`; streaming upload restore
  job (512 MiB); merge semantics never lower progress.
- Legacy v1 DB import and per-episode state migration.
- Scheduled cache GC with Admin status.
- Security: loopback guard, CSP, nosniff, frame/referrer/permissions headers.
- Windows portable zip + installer scripts + per-user WiX MSI.
- Linux tarball + install/uninstall scripts; Dockerfile/Compose.
- Operations, packaging, Linux, Docker, install and release-check docs.

### Known limitations

- Single-user, loopback-only; no authentication.
- Windows-first; Linux verified under WSL2, not bare metal.
- No HLS output, plugins/WASM, remote/NAS/S3 in v1.
- MSI is unsigned; no code signing.
- Backup JSON is parsed in memory after upload.
