# Docker

The repository ships a multi-stage Dockerfile and Compose file. Status:
**verified end-to-end in WSL2 Ubuntu 24.04** (native dockerd, not Docker
Desktop):

- `dockerd` 29.1.3, buildx 0.30.1, compose 2.40.3.
- Base images pulled from Docker Hub via registry mirrors
  (`docker.m.daocloud.io`, `docker.1ms.run`); plain Docker Hub metadata was
  blocked on this network.
- `docker build --build-context web=../localreel-web -t localreel:test .`
  succeeded: Node web build, Rust release build, Debian runtime with ffmpeg.
- `docker compose up -d --build` succeeded (image `localreel:dev`); container
  health was `ok`, ffmpeg available.
- Container media test with the real 12-file library mounted read-only:
  12 media, 12 durations, 12 thumbnails, direct playback and
  `Range: bytes=0-1023` → `206`.
- `docker restart` preserved `/data`: media_count=12 and saved position=165.
- Rust base is now **1.85** (a dependency uses edition 2024); the Dockerfile
  stage is `web-build` to avoid colliding with the named `web` context, and the
  extra `# syntax=` frontend dependency was removed.

Docker Desktop on Windows was not used (service needs administrator rights);
the evidence above is WSL2 dockerd + Linux containers.

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

- Production image build was verified in WSL2; Windows Docker Desktop itself
  was not exercised.
- The runtime image includes Debian ffmpeg 5.1 (larger image, codec set differs
  from a custom ffmpeg build).
- No GPU acceleration, HLS output or authentication.
- Bind-mounted media must be readable by uid 10001.
