# Install LocalReel on Windows

LocalReel is a portable zip. You can run it in place, or use the included
`install.ps1` to copy it under `%LOCALAPPDATA%` and create shortcuts.

## Option A — one-click from the extracted zip

```powershell
# extract localreel-<version>-x64.zip, then from the extracted folder:
.\install.ps1
```

Defaults:

- installs to `%LOCALAPPDATA%\LocalReel`
- creates `%APPDATA%\Microsoft\Windows\Start Menu\Programs\LocalReel.lnk`
- starts in place if the folder already contains `localreel.exe`

Then open the Start Menu shortcut (or run
`%LOCALAPPDATA%\LocalReel\localreel.exe serve`) and browse
<http://127.0.0.1:3001>.

## Option B — from the repository

```powershell
cd localreel-core
.\scripts\install.ps1 -ZipPath .\dist\localreel-0.1.0-x64.zip
```

The script accepts either a zip or `-SourceDir <extracted-folder>`.

## Options

```powershell
.\install.ps1 `
  -InstallDir 'D:\Apps\LocalReel' `
  -DesktopShortcut `
  -StartOnLogin
```

| Switch | Effect |
|---|---|
| `-InstallDir` | Target directory (default `%LOCALAPPDATA%\LocalReel`) |
| `-DesktopShortcut` | Also create a Desktop shortcut |
| `-StartOnLogin` | Put a shortcut in the Startup folder (console window at login) |
| `-NoShortcuts` | Copy files only; useful for automation/tests |
| `-StartMenuDir` / `-DesktopDir` / `-StartupDir` | Override shortcut targets |

`-StartOnLogin` starts the **server** at login, not a headless service; close the
console window to stop it. For a real service, use Task Scheduler/NSSM with
`localreel.exe serve`.

## What the installer preserves

`install.ps1` copies the new binary/docs/tools over the install directory and
**never deletes** `data/` or `localreel.toml`. On a first install it copies
`localreel.toml.example` to `localreel.toml`; on upgrade it leaves your config
alone. `install.json` records the installed version, source and shortcuts.

## MSI

A per-user MSI is available (WiX 5):

```powershell
msiexec /i .\dist\localreel-0.1.0-x64.msi
msiexec /x .\dist\localreel-0.1.0-x64.msi
```

It installs to `%LOCALAPPDATA%\LocalReel`, creates a Start Menu shortcut, and
leaves `data/`/`localreel.toml` untouched on uninstall. See
[installer/README.md](../installer/README.md). The MSI is unsigned.

## Start automatically

Use the included Task Scheduler helper (recommended; supports restart-on-failure
and no console window):

```powershell
.\install-task.ps1
```

Or re-run the installer with `-StartOnLogin` for a Startup-folder shortcut.
See [service.md](service.md) for logon vs boot, logs and removal.

## Uninstall

From the install directory (or the extracted package), run:

```powershell
.\uninstall.ps1
# keeps data/ and localreel.toml
```

The script removes shortcuts recorded in `install.json`, stops a server that is
running from the same install directory only with `-StopServer`, and keeps
`data/` plus `localreel.toml` by default. Options:

```powershell
.\uninstall.ps1 -InstallDir 'D:\Apps\LocalReel' -Force
.\uninstall.ps1 -InstallDir 'D:\Apps\LocalReel' -RemoveData -Force
.\uninstall.ps1 -InstallDir 'D:\Apps\LocalReel' -StopServer -Force
```

Without `-Force`, it asks for confirmation. `-RemoveData` deletes progress and
config as well. Back up `data/` first if unsure.

## Verify

```powershell
cd $env:LOCALAPPDATA\LocalReel
.\localreel.exe --version
.\localreel.exe doctor
```

`doctor` should print `web: embedded`, the data directory and the resolved
ffmpeg. See [operations.md](operations.md) for first-run source setup and
[release-checklist.md](release-checklist.md) for release verification.
