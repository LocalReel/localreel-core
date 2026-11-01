# Docker

The repository ships a multi-stage Dockerfile and Compose file. Status:
**the production Dockerfile has not been built end-to-end in this environment**.
The Windows Docker Desktop service needs administrator rights, so a native
dockerd was started inside WSL2 instead:

- `dockerd` 29.1.3 works; base images (`node:22-bookworm-slim`,
  `rust:1.83-bookworm`, `debian:bookworm-slim`) were pulled successfully through
  the host proxy.
- `docker build` (BuildKit) still fails resolving Docker Hub metadata because
  the BuildKit resolver does not use the daemon proxy; this is an environment
  network limitation, not a Dockerfile failure.
- A **local runtime smoke image** (Debian + prebuilt Linux binary, no
  multi-stage build) was built with the legacy builder and ran successfully:
  health `ok`, embedded UI `200`, and the `/data` volume survived
  `docker restart`.
- The committed Dockerfile had a real bug: the stage name `web` collided with
  the named build context `web`; it is now `web-build`. The
  `# syntax=docker/dockerfile:1.7` frontend line was removed because it tried
  to pull an extra frontend image.

Do not treat Docker as release-verified until `docker compose up --build`
succeeds on a machine with Docker Desktop/Engine and registry access.

## Build

The web UI lives in the sibling repository. BuildKit named contexts pass it to
the core build context:

```powershell
cd localreel-core
docker build --build-context web=../localreel-web -t localreel:dev .
```

The Dockerfile:

1. builds `localreel-web` with Node 22 (`npm ci` + `vite build`);
2. builds `localreel-server` with Rust, embedding the web `dist`;
3. copies the binary into `debian:bookworm-slim` with `ffmpeg`, `tini`, `curl`;
4. runs as uid 10001, data volume `/data`, port 3001.

## Compose

```powershell
$env:LOCALREEL_MEDIA_DIR = 'D:\Videos'
docker compose up --build
# http://127.0.0.1:3001
```

`docker-compose.yml`:

- publishes `127.0.0.1:3001:3001` (loopback only);
- mounts the media directory read-only at `/media`;
- uses the `localreel-data` named volume for `/data`;
- restarts unless stopped.

Add the media path through the UI/API as `/media` (the container path). The
`ffmpeg`/`ffprobe` binaries are inside the image.

## Security and ports

The server refuses to bind `0.0.0.0` without `LOCALREEL_ALLOW_REMOTE=1`; the
container sets that flag so it can serve through a published port. The in-app
loopback guard is therefore disabled inside the container, and **the host-side
port mapping is the security boundary**. Keep Compose's
`127.0.0.1:3001:3001`; if you publish `3001:3001` on all interfaces, anyone who
can reach the host can use LocalReel. There is no authentication in v1.

## Data and backup

- Data/DB/thumbnails/transcodes: `/data` (named volume).
- Backup from inside the container:
  ```powershell
  docker compose exec localreel localreel backup-export /data/backup.json
  docker compose cp localreel:/data/backup.json .\backup.json
  ```
- Restore is the same two-pass/CLI `restore` flow documented in
  [operations.md](operations.md); the source path in the container must be the
  mounted path (e.g. `/media/...`).

## Operations

```powershell
docker compose logs -f localreel
docker compose restart localreel
docker compose down          # keeps the data volume
docker compose down -v       # deletes data
```

## Known limitations

- Image build not verified here; only `docker compose config` was run.
- The runtime image includes Debian ffmpeg (larger image, codec set differs from
  a custom ffmpeg build).
- No GPU acceleration, HLS output or authentication.
- Bind-mounted media must be readable by uid 10001.
