# Release checklist

A release is a zip built from a clean `localreel-core` checkout plus the web
build. The server embeds `localreel-web/dist` at compile time.

## 1. Preconditions

- [ ] `Cargo.toml` workspace version and `localreel-web/package.json` version
      agree (currently `0.1.0`).
- [ ] If migrations changed, they are additive and tested against a copy of an
      existing DB.
- [ ] `localreel-web/dist` is **not** stale relative to `src/`.
- [ ] License decision recorded (currently TBD by the repository owner).

## 2. Automated checks

```powershell
cd localreel-core
.\scripts\release-check.ps1
```

This runs:

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`
- Web `npm test` (resume helper unit tests)
- Web `npm run build` (includes `tsc`)
- `npm run test:browser` (headless Chrome + mock API + 1450s WebM fixture;
  runs in CI, no core build/ffmpeg needed)

Optional real-server browser smoke (requires a running LocalReel server and
Chrome):

```powershell
cd localreel-web
$env:LOCALREEL_BASE_URL = 'http://127.0.0.1:3317'
$env:CHROME_PATH = 'C:\...\chrome.exe'
npm run test:resume
```

Expected: `{"ok":true,...,"expected":165,"actual":165,...}` for a library whose
first item has saved progress.

## 3. Build the package

```powershell
cd localreel-core
.\scripts\package.ps1 -BuildWeb -FfmpegDir '.tools\ffmpeg'
```

- Default profile: `release`.
- Output: `dist/localreel-<version>-x64.zip` and `.sha256`.
- `-FfmpegDir` is optional; without it the package uses PATH/discovery.
- Record the SHA256 from the script output.

## 4. Verify the artifact on a clean directory

- [ ] Extract the zip into a new directory.
- [ ] Optional Linux: `./scripts/package-linux.sh --build-web`; extract the
      tarball on a Linux host and run `./localreel --version` / `doctor`.
      WSL2 Ubuntu build/runtime is verified here (health + embedded UI); bare
      metal and ffmpeg playback are still worth checking.
- [ ] Optional Docker: `docker compose build`; start with a loopback-only
      port, scan `/media`, play one file, and verify `/data` persistence after
      `docker compose restart` (not yet verified in this workspace).
- [ ] `.\localreel.exe --version` prints the expected version.
- [ ] `.\localreel.exe doctor` shows `web: embedded`, DB ok, and the expected
      ffmpeg status.
- [ ] Start `.\localreel.exe serve`, open `http://127.0.0.1:3001`, confirm the UI
      (not the API placeholder) loads.
- [ ] Run `.\install.ps1 -NoShortcuts -InstallDir <temp>` from the extracted
      folder; confirm the binary/config/`install.json` land there and an upgrade
      over existing `data/`/`localreel.toml` preserves them.
- [ ] Run `.\uninstall.ps1 -InstallDir <temp> -Force`; confirm program files and
      shortcuts are removed while `data/`/`localreel.toml` remain; repeat with
      `-RemoveData` in a scratch install.
- [ ] Register a scratch task (`.\install-task.ps1 -TaskName 'LocalReel Scratch'
      -InstallDir <temp>`), verify action/working directory, then uninstall it.
- [ ] MSI: `.\scripts\build-msi.ps1`; silent install `/qn`, check exe +
      Start Menu shortcut + `--version`, then silent uninstall and confirm a
      data marker survives. MSI is unsigned.
- [ ] Add/scan a small source; confirm thumbnail and playback.
- [ ] Export a backup and re-import it in a second data dir (`--dry-run` first).
- [ ] Check `/api/v1/health` and `/api/v1/admin/health`.

## 5. Release notes

Include:

- version and commit;
- user-visible changes (library/reels/admin/backup/cache/archive/remap);
- DB migration note;
- ffmpeg requirement and whether a bundle is included;
- known limitations:
  - single-user, loopback only;
  - no HLS output, no plugins;
  - no installer/signing (zip only);
  - archived media hidden by default;
  - browser resume smoke runs against the mock UI in CI; the real-server
    variant remains a manual check;
- SHA256 of the zip.

## 6. Rollback

- Keep the previous zip and a DB backup.
- Stop the server; restore `data/localreel.db`; start the old binary.
- Migrations are forward-only; restoring the DB is the supported rollback.

## Verified acceptance (2026-11-01, Windows MSI)

The final per-user MSI (`localreel-0.1.0-x64.msi`, 2.29 MiB) was installed with
`msiexec /qn` and exercised against the real 12-file library:

- health `ok`, ffmpeg available, CSP/security headers present;
- scan: 12 media, 12 durations, 12 thumbnails (h264/aac);
- playback: direct, `Range` 206;
- state saved at 165s;
- backup export written;
- silent uninstall removed exe/Start Menu shortcut and kept
  `data/localreel.db` (media rows remain).

The portable zip for this build has SHA256
`0621deec2d6a2cd8dc549c31683e8ebc36c170557d2a1b809a2e406fb1f96030`.

## Definition of done

- [ ] Checks green on Windows and Linux CI.
- [ ] Package starts and serves the embedded UI on a clean machine.
- [ ] Backup/restore drill passed.
- [ ] Release notes and SHA256 published.
