# LocalReel operations manual

This is the practical runbook for a local, single-user LocalReel install. The
server binds loopback by default; it is not an internet-facing service.

## Layout

Default paths are relative to the working directory:

```text
localreel/
  localreel.exe
  localreel.toml            # optional; defaults + LOCALREEL_* env otherwise
  data/
    localreel.db            # SQLite (WAL)
    thumbnails/             # thumb_<uid>.jpg
    transcodes/             # transcode_<uid>_<preset>_....mp4
```

`LOCALREEL_DATA_DIR`, `LOCALREEL_CONFIG`, `LOCALREEL_HOST` and
`LOCALREEL_PORT` override the defaults. `localreel doctor` prints the resolved
paths.

## First run

```powershell
.\localreel.exe doctor
.\localreel.exe serve
# http://127.0.0.1:3001
```

To start automatically at logon on Windows, see
[service.md](service.md) (`install-task.ps1`). The installer's Start Menu
shortcut is manual start only.

Add a source through the UI/API or the Admin page:

```powershell
Invoke-RestMethod -Method Post http://127.0.0.1:3001/api/v1/sources `
  -ContentType application/json -Body '{"uri":"D:\\Videos"}'
Invoke-RestMethod -Method Post http://127.0.0.1:3001/api/v1/sources/1/scan
```

The scan job discovers files; probe/thumbnail jobs follow automatically.
`GET /api/v1/jobs` and `GET /api/v1/events` expose progress.

## Logs and health

- Logs go to **stderr** via `tracing`; there is no file rotation in v1.
  Redirect if running as a service:
  `.\localreel.exe serve 2>> localreel.log`
- `RUST_LOG=info` (default) / `RUST_LOG=debug` changes verbosity.
- `GET /api/v1/health`: status, version, DB, ffmpeg, media count.
- `GET /api/v1/admin/health`: paths, recent jobs, DB/ffmpeg, cache.
- `GET /api/v1/admin/cache`: transcode/thumbnail bytes and counts.
- `GET /api/v1/scan-runs`, `/scan-issues`: scan history and failures.

## ffmpeg

Media probing/thumbnails/transcodes need `ffmpeg` and `ffprobe`.
`doctor` shows the resolved paths. Discovery order is documented in
[packaging.md](packaging.md); in short:

1. explicit config/env paths,
2. `LOCALREEL_FFMPEG_DIR`,
3. next to the exe (`ffmpeg/bin`, `tools/ffmpeg/bin`, `bin`),
4. current directory,
5. `PATH`.

If ffmpeg is missing, the server still starts and the API/UI work; scans add
rows but metadata/playback preparation fails until it is installed. Run
`tools\verify-ffmpeg.ps1` to check an install.

## Backup and restore

Application backup (portable, versioned JSON) is the primary state backup:

```powershell
# without stopping the server
.\localreel.exe backup-export .\backup.json

# restore into a fresh data dir is a two-pass sequence:
.\localreel.exe backup-import .\backup.json --dry-run
.\localreel.exe backup-import .\backup.json   # adds sources; states are missing
# then scan the sources (UI/API scan, or `scan <id>` for discovery only)
.\localreel.exe backup-import .\backup.json   # applies states by uid, then URI
```

HTTP equivalents: `GET /api/v1/backup` and
`POST /api/v1/backup?dry_run=true|false` (32 MiB limit). Admin -> Backup exposes
the same flow.

The backup contains sources and per-media state (uid/path/flags/position). It
does **not** contain media files, thumbnails or transcodes. After importing into
a new machine, scan the sources before importing state; state is matched by
`uid` first, then by URI. Progress is never lowered, flags are OR-ed.

A verified restore drill on the real 12-file library (2026-11-01):

- fresh DB: first import added 1 source, 12 states missing;
- server scan added/probed 12 media + thumbnails;
- second import applied 12 states (0 missing);
- `/media?filter=continue` total=12, E08 position=306.44337;
- direct playback and Range 206 worked after probe.

Note: the CLI `scan` command only discovers rows; use the server/API scan when
you need probe metadata and thumbnails before playback.

Full DB backup: stop the server first, then copy `data/localreel.db` (and
`-wal`/`-shm` if present). Restoring an old DB is a rollback of both schema and
state; back up the current DB first.

## Upgrade

1. Export a backup and copy `data/localreel.db` while stopped.
2. Replace `localreel.exe` (and `ffmpeg/bin` if bundled).
3. Run `.\localreel.exe migrate`; migrations are forward-only and applied on
   `serve` too.
4. Run `.\localreel.exe doctor` and check `/api/v1/health`.
5. Open the UI and spot-check playback.

There are no down migrations. Roll back by restoring the DB and the old binary.

## Maintenance

- Cache GC runs at startup and every `jobs.cache_gc_interval_secs` (default 24h,
  0 disables). It removes transcodes by age/size and keeps anything used in the
  last hour. Admin → Cache can run it manually.
- Archived sources keep their media/state; they are hidden from the default
  source list, cannot be scanned, and their media is hidden from Library/Reels
  unless the `Archived` filter is enabled.
- If a source directory moves, use Admin → Sources → Remap (or
  `POST /api/v1/sources/{id}/remap` / `localreel.exe remap-source`). Preview is
  dry-run; only files present under the new root are rewritten.

## CLI reference

| Command | Purpose |
|---|---|
| `serve` | Run HTTP server (default) |
| `migrate` | Apply DB migrations and exit |
| `doctor` | Config/DB/ffmpeg/web checks |
| `scan <source-id>` | Synchronous discovery for one source |
| `remap-source <id> <uri> [--dry-run]` | Rewrite a moved local root |
| `import <legacy.db> [--dry-run]` | Import a copy of a v1 database |
| `state-import <legacy.db> [--source-id N] [--dry-run]` | Map v1 states by episode |
| `backup-export <out.json>` | Export v2 source/state backup |
| `backup-import <file> [--dry-run]` | Merge v2 backup |

## Troubleshooting

| Symptom | Check / fix |
|---|---|
| UI is API-only placeholder | Build `localreel-web/dist` before `cargo build`, or use the packaged release whose web UI is embedded |
| `ffmpeg: false` in health | Run `doctor`; install ffmpeg or set `LOCALREEL_FFMPEG_DIR` |
| Scan finds 0 files | `GET /scan-runs`; verify source URI/dir exists and is not archived |
| Media shows missing | File moved/deleted; restore it and rescan, or remap the source root |
| Playback stuck on `preparing` | Check `GET /jobs` for failed `transcode`; check ffmpeg and disk space |
| Port already in use | Set `LOCALREEL_PORT` or `server.port` |
| `source is archived` on scan | Restore it in Admin or `PATCH /sources/{id} {"archived":false}` |
| Cache keeps growing | Check `jobs.cache_gc_interval_secs`, run Admin → Run GC, inspect `/admin/cache` |
| DB locked | Ensure only one server process; check `-wal`/`-shm`; do not run two writers |

## Security

- Default host is `127.0.0.1`; the API has a loopback Host/Origin guard.
- Do not set `allow_remote`/`LOCALREEL_ALLOW_REMOTE=1` without an external
  authentication layer (v1 has no users/tokens).
- The server has filesystem access to configured sources; keep the data dir and
  config private.
