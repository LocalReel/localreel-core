# Packaging LocalReel

LocalReel itself is a single Rust binary, but media probing/thumbnails/transcodes
use `ffmpeg` and `ffprobe`. The web UI is compiled into the binary by
`localreel-api/build.rs` when `../localreel-web/dist` exists at build time.

## Layout

A portable Windows package can look like this:

```text
localreel/
  localreel.exe
  localreel.toml          # optional
  ffmpeg/
    bin/
      ffmpeg.exe
      ffprobe.exe
  data/                   # created on first run
```

`localreel.exe` does not need to be started from this directory; the ffmpeg
lookup is relative to the executable.

## ffmpeg lookup order

`MediaOps::discover` tries, in order:

1. Explicit `media.ffmpeg` / `media.ffprobe` paths from `localreel.toml` or
   `LOCALREEL_FFMPEG` / `LOCALREEL_FFPROBE`, when they contain a directory
   component and pass `<binary> -version`.
2. `LOCALREEL_FFMPEG_DIR` (`ffmpeg.exe`, `ffmpeg.cmd`, `ffmpeg.bat`, plus
   `bin/`).
3. Next to `localreel.exe`; then `ffmpeg/`, `ffmpeg/bin/`, `tools/ffmpeg/bin/`,
   `bin/` below it.
4. The same subdirectories below the current working directory.
5. The configured names unchanged (the defaults `ffmpeg`/`ffprobe` resolve via
   `PATH`).

If both binaries are not found, the server still starts: scanning, thumbnails,
probing and transcoding are disabled, and `/api/v1/health` reports
`ffmpeg: false`. Run `localreel doctor` for the resolved paths and a hint.

## Web assets

`localreel-api/build.rs` walks `../localreel-web/dist` and embeds files with
`include_bytes!`. Rebuild order after changing the UI:

```powershell
cd localreel-web
npm run build
cd ../localreel-core
cargo build -p localreel-server
```

If the UI was never built, the binary still runs and serves the API plus a
placeholder page. `localreel doctor` prints `web: embedded`, `web: disk (...)`,
or `web: missing`.

## Tests without real ffmpeg

`MediaOps` supports `.cmd`/`.bat` harnesses on Windows (invoked through
`cmd.exe`). The kernel test `probe_runs_a_fake_cmd_harness` uses this to test
JSON parsing and process handling without a real ffmpeg install.
