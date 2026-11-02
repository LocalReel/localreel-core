# Development vs. release

LocalReel deliberately keeps the development server and an installed release
apart. They must never share a database, a port or a cache directory.

| | Development (this repository) | Release (installed app) |
|---|---|---|
| Start with | `.\scripts\dev.ps1` | Start Menu shortcut / `localreel.exe serve` |
| Binary | `target\debug\localreel.exe` | `%LOCALAPPDATA%\LocalReel\localreel.exe` |
| Port | **3101** | **3001** |
| Data | `<repo>\.dev-data\` (git-ignored) | `%LOCALAPPDATA%\LocalReel\data\` |
| Config | env vars from `dev.ps1` | `%LOCALAPPDATA%\LocalReel\localreel.toml` |
| Upgraded by | `git pull` + rebuild | GitHub Release artifact |

## Run the development server

```powershell
cd localreel-core
.\scripts\dev.ps1                 # build (debug) and serve on 3101
.\scripts\dev.ps1 -SkipBuild      # reuse the current binary
.\scripts\dev.ps1 -Port 3102 -DataDir D:\lr-branch   # a second experiment
```

`.dev-data\` holds the dev database, thumbnails and transcodes. Delete it to
start from an empty library:

```powershell
Remove-Item -Recurse -Force .dev-data
```

## Quality gates before pushing

```powershell
.\scripts\release-check.ps1       # fmt + clippy + tests + web tests + web build
```

The same commands run in GitHub Actions (`.github/workflows/ci.yml`) on
`windows-latest` and `ubuntu-latest`.

## Rules

- Never point the dev server at `%LOCALAPPDATA%\LocalReel` and never install a
  release into a checkout.
- Test media should live under `.dev-data` (synthetic fixtures) or be read-only
  references to a library; do not write into a user's media directories.
- A release artifact is built from a tag with `scripts/package.ps1` and
  `scripts/build-msi.ps1`; a dev build is never distributed.
