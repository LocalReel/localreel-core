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

## Status

Version 0.1.0: usable local-first server with the web UI, scans/metadata/
thumbnails, direct playback, Reels resume, Admin, backup/restore, archive/remap,
cache GC, Windows zip/MSI, Linux tarball and Docker. Windows is the primary
target; Linux and Docker were verified under WSL2. Public v1.0 release is
blocked by the license decision and code signing; see the readiness report.

The full delivery plan (milestones, work breakdown, estimates, risks, next
sprint) is in [`docs/roadmap/v2-plan.md`](docs/roadmap/v2-plan.md).

## Release

- [0.1.0 release notes](docs/release/v0.1.0.md)
- [v1.0 readiness / blockers](docs/release/v1.0-readiness.md)
- [CHANGELOG.md](CHANGELOG.md)

## License

TBD by the repository owner. The v2 plan's default is Apache-2.0, but no
`LICENSE` file exists yet; see
[docs/release/v1.0-readiness.md](docs/release/v1.0-readiness.md) for the
release blocker.
