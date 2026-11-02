# localreel-core

The LocalReel server: a single binary that indexes local media, serves a web UI,
and runs scan/probe/thumbnail/transcode work through a persistent job queue.

This is the **B1** implementation: a modular monolith with strict internal
boundaries, not a WASM plugin host. Extension seams are defined as traits and
stay out of the critical path until the product is stable.

## Workspace

```text
crates/localreel-kernel/   generic capabilities: config, db, jobs, events,
                           storage (VFS), media-ops (typed ffmpeg)
crates/localreel-media/    first-party media domain: sources, scan, metadata,
                           state, search, playback policy, variants
crates/localreel-api/      HTTP /api/v1, SSE events, static assets, loopback guard
crates/localreel-server/   the `localreel` binary: wiring, migrations, serve
migrations/               referenced by the kernel/media crates
docs/adr/                 architecture decision records
```

Dependency direction is one-way:

```text
web -> api -> media -> kernel
```

The kernel must never know about videos; the media crate must never write HTTP
handlers or spawn `ffmpeg` directly.

## Commands

```powershell
# development server (isolated: port 3101, data in .dev-data/)
.\scripts\dev.ps1

# from this repository
cargo run -p localreel-server -- serve
cargo run -p localreel-server -- migrate
cargo run -p localreel-server -- doctor

# discover media under an already configured source (walk/fingerprint only)
cargo run -p localreel-server -- scan --source-id 1

# import a *copy* of a LocalReel v1 database (dry-run first)
cargo run -p localreel-server -- import C:\backup\localreel.db --dry-run
cargo run -p localreel-server -- import C:\backup\localreel.db
```

Configuration is loaded from `localreel.toml` (or `LOCALREEL_CONFIG`) and then
`LOCALREEL_*` environment variables; see `localreel.toml.example`.

Default bind: `127.0.0.1:3001`. Data lives in `./data` unless
`LOCALREEL_DATA_DIR` is set.

A development checkout and an installed release are kept apart on purpose: the
dev script uses port 3101 and `.dev-data/`, while the installed app uses 3001 and
`%LOCALAPPDATA%\LocalReel`. See [docs/development.md](docs/development.md).

## Status

Version 0.3.0: usable local-first server with a bilingual (中文/EN) web UI,
scans/metadata/thumbnails, direct playback plus an H.264 remux fast path,
Reels resume, Admin, backup/restore, archive/remap, cache GC, and a
single-binary build with the UI embedded. Windows is the primary target; Linux
and Docker were verified under WSL2.

This repository is **a component, not the product**. Installers, the MSI/zip
artifacts, release notes and download instructions live in the product
repository [`LocalReel`](https://github.com/LocalReel/LocalReel). The core
builds a binary; the product composes the binary with the web UI and docs.

The full delivery plan (milestones, work breakdown, estimates, risks, next
sprint) is in [`docs/roadmap/v2-plan.md`](docs/roadmap/v2-plan.md).

## Release

- Component changelog: [CHANGELOG.md](CHANGELOG.md)
- Product downloads and release notes:
  [`LocalReel/LocalReel`](https://github.com/LocalReel/LocalReel)
- v1.0 readiness / blockers:
  [`LocalReel/LocalReel/docs/release/v1.0-readiness.md`](https://github.com/LocalReel/LocalReel/blob/main/docs/release/v1.0-readiness.md)

## License

Apache-2.0 — see [`LICENSE`](LICENSE). Copyright 2026 LocalReel contributors.

ffmpeg is **not** bundled: the server discovers `ffmpeg`/`ffprobe` on PATH, next
to the binary, in `ffmpeg/bin`, or via `LOCALREEL_FFMPEG_DIR`. Any ffmpeg you
install keeps its own license (GPL or LGPL depending on the build you choose).
