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
```

Default bind: `127.0.0.1:3001`. Data lives in `./data` unless
`LOCALREEL_DATA_DIR` is set.

## Status

Phase 0/1: kernel skeleton is being built. The media domain, API and UI are
ported in the next phases; see `docs/adr/` for the decisions that constrain
them.

The full delivery plan (milestones, work breakdown, estimates, risks, next
sprint) is in [`docs/roadmap/v2-plan.md`](docs/roadmap/v2-plan.md).

## License

TBD by the repository owner.
