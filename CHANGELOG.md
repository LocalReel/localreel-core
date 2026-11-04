# Changelog

## 0.4.0 — 2026-11-04

The scroll feed becomes real: series-aware, server-ranked, with an endpoint the
UI can autoplay from.

### Added

- **`GET /api/v1/feed`** — ranked view over the library: continue-watching
  first, then unwatched episodes grouped by series in episode order, then
  unwatched single items, then finished items. Extras, missing files and
  archived sources are excluded. Items carry `width`/`height`, `duration`,
  `position`, `favorite`/`liked` and `next_id` (next episode in the series).
- **Series derivation** (`media/series.rs`): `series_key`/`series_title` from
  the directory (season folders prefixed with their parent, extras folders
  recognised), `episode` from release-style names (`[12]`, `S02E05`, `EP05`,
  `第5话`, `- 01v2`, `01 标题`), resolution and CRC tags rejected.
- Migrations `0003_series` + `0004_series_rev`: existing rows are backfilled on
  the next scan when the parser revision changes.

### Fixed

- **Jobs interrupted by a crash stayed `running` forever.** `JobQueue::reclaim_expired`
  existed but the server never called it, so after a crash (or a `kill`) the row
  kept its dead owner and workers — which only claim `queued` rows — never
  touched it again. The server now reclaims expired leases at startup and every
  half lease, and records the error
  `interrupted: the server stopped while this job was running` instead of the
  developer-only `lease expired`. Covered by
  `restart_recovers_a_crashed_job_after_its_lease_expires`; verified by
  killing the dev server mid-transcode and watching the job resolve.
- **Transcoded variants carried a stray `text` data track.** The mp4 muxer
  writes source chapters as an extra chapter track, so a re-encoded episode
  ended up with `video + audio + bin_data`. Transcodes and remuxes now pass
  `-sn -dn -map_chapters -1`; a variant contains exactly one video and one
  audio stream.

## 0.3.0 — 2026-11-02

The core is a component again: product output moved to the `LocalReel` product
repository.

### Added

- `LOCALREEL_WEB_DIR`: `localreel-api/build.rs` embeds the UI from any web
  checkout, so the product can compose components that do not sit next to each
  other.

### Changed

- Installers, packaging scripts, install/uninstall helpers, product docs and
  release notes moved to `LocalReel` (the product repository). This repository
  now only builds and tests the server component; it produces no zip/MSI and
  publishes no release artifacts.
- README points at the product repository for downloads and release status.

### Removed

- `installer/`, `scripts/package.ps1`, `scripts/build-msi.ps1`,
  `scripts/install*.ps1`, `scripts/uninstall.ps1`, `scripts/sign.ps1`,
  `scripts/package-linux.sh`, `scripts/*.sh`, `scripts/fetch-ffmpeg.ps1`,
  `scripts/verify-ffmpeg.ps1`, `docs/install.md`, `docs/packaging.md`,
  `docs/release-checklist.md`, `docs/signing.md`, `docs/linux.md`,
  `docs/docker.md`, `docs/service.md`, `docs/release/`.

## 0.2.0 — 2026-11-02

Faster playback, reliable cancellation, a bilingual UI, and a dev/release split.

### Added

- **Remux fast path**: H.264 sources in MKV/MOV/TS/M2TS/FLV/AVI are rewritten into
  MP4 with `-c:v copy` instead of being re-encoded (audio is copied when the
  browser accepts it, otherwise only the audio is re-encoded). A 60 s clip that
  used to take a full re-encode now finishes in ~0.5 s; a typical movie drops
  from tens of minutes to seconds.
- **Bilingual UI (zh/en)**: every page (Library, Reels, Admin, player) is
  translated, with an English/Simplified Chinese switch. The initial language follows
  `navigator.language` and the choice persists in `localStorage`.
- **Job priorities**: `enqueue_with_priority`; thumbnails run at priority 10 so
  they are no longer starved behind a bulk probe backlog.
- **Development server isolation**: `scripts/dev.ps1` runs a debug server on
  port 3101 with data in `.dev-data/`, separate from an installed release
  (port 3001, `%LOCALAPPDATA%\LocalReel`). Documented in
  [docs/development.md](docs/development.md).

### Fixed

- **Cancelling a running transcode did not stop it.** `POST /jobs/{id}/cancel`
  now latches the cancellation flag even when the worker has not created it yet,
  the worker skips a handler whose job was cancelled before it started, and
  Windows kills the whole process tree (`taskkill /T`) so a wrapper/shim cannot
  leave the real encoder running. Measured: encoder gone in 0.7 s, job
  `canceled`, no partial output file.
- Cancelled/aborted transcodes remove their partial output file.

### Changed

- Thumbnail jobs carry priority 10; probes stay at 0.
- Version bumped to 0.2.0 in the workspace and the web package.

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
- License: Apache-2.0, with `LICENSE` in the core, web and format repositories
  and `license` set in `Cargo.toml`/`package.json`.

### Known limitations

- Single-user, loopback-only; no authentication.
- Windows-first; Linux verified under WSL2, not bare metal.
- No HLS output, plugins/WASM, remote/NAS/S3 in v1.
- MSI is unsigned; no code signing.
- Backup JSON is parsed in memory after upload.
