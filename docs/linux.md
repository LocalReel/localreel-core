# Linux packaging and install

Linux is a secondary target: the code is portable and CI runs `cargo test` on
Ubuntu, but the Windows zip remains the primary distribution.

Verified in this workspace (WSL2, Ubuntu 24.04, x86_64, 2026-11-01):

- Linux release build (`cargo build --release`) and `cargo clippy -D warnings`
  pass; one Windows-only `OsStr` import warning was fixed.
- `localreel --version` / `doctor`: `web: embedded`.
- Installed Ubuntu ffmpeg/ffprobe 6.1.1 and scanned the real 12-file library via
  `/mnt/i`: 12 media, 12 durations, 12 thumbnails (h264/aac, 1280x720).
- direct playback and `Range: bytes=0-1023` returned `206`.
- packaged release served `/api/v1/health` (`ok`) and `/` (`200`, embedded UI).
- tarball (3.3 MB) + `install.sh` / `uninstall.sh` lifecycle passed, including
  preserving `data/` and `localreel.toml`.
- installed Chrome 156 + Node 22 in WSL and ran the real-server browser resume
  test: `expected=165, actual=165, duration=1449.970068, readyState=1` for
  `S01E12.mp4`.

This is not a bare-metal desktop test; GPU/desktop integration and 4K/HDR
hardware decode on Linux are still unverified.

## Build a tarball

From `localreel-core` on Linux:

```bash
# the server embeds localreel-web/dist at compile time
./scripts/package-linux.sh --build-web
```

Output:

```text
dist/localreel-<version>-x86_64.tar.gz
dist/localreel-<version>-x86_64.tar.gz.sha256
```

Options:

```text
--version <v>        override the workspace version
--out <dir>          output directory
--skip-build         package target/release/localreel as-is
--ffmpeg-dir <dir>   copy ffmpeg/ffprobe into ffmpeg/bin
--allow-non-linux    structural test only (does not make the binary run)
```

Prerequisites: Rust stable, Node/npm if `--build-web`, and (optionally) ffmpeg.
The web UI is embedded in the binary; no separate `dist` is needed at runtime.

## Install

Extract the tarball and run:

```bash
tar -xzf localreel-<version>-x86_64.tar.gz
cd localreel-<version>-x86_64

./install.sh                    # ~/.local/share/localreel + app menu entry
./install.sh --start-on-login   # also ~/.config/autostart/localreel.desktop
./install.sh --install-dir /opt/localreel --no-desktop
```

`install.sh` keeps an existing `data/` and `localreel.toml`; it only creates
`localreel.toml` on first install. Start with:

```bash
~/.local/share/localreel/localreel serve
# http://127.0.0.1:3001
```

The desktop entry runs the server in a terminal so you can see logs. For a
headless/service setup, use a systemd **user** unit:

```ini
# ~/.config/systemd/user/localreel.service
[Unit]
Description=LocalReel media server
After=network.target

[Service]
WorkingDirectory=%h/.local/share/localreel
ExecStart=%h/.local/share/localreel/localreel serve
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now localreel
```

## Uninstall

```bash
./uninstall.sh --yes                 # keeps data/ and localreel.toml
./uninstall.sh --remove-data --yes   # deletes everything
./uninstall.sh --stop-server --yes   # stop a process matching this install
```

It removes the app-menu and autostart entries and preserves data/config by
default.

## Verify (when a Linux machine is available)

```bash
./localreel --version
./localreel doctor     # web: embedded, ffmpeg resolved, DB path
./localreel serve &
curl -fsS http://127.0.0.1:3001/api/v1/health
```

Then open the UI, add a source, scan, and play one file. Do not expose the
server beyond loopback without an authentication layer.

See [operations.md](operations.md) for backup/upgrade and
[packaging.md](packaging.md) for ffmpeg discovery.
