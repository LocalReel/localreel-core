# Windows MSI

`localreel.wxs` is a WiX v5 source for a per-user MSI:

- installs to `%LOCALAPPDATA%\LocalReel`;
- Start Menu shortcut under `Programs\LocalReel`;
- installs the exe, config example, README, `install-task.ps1` and two docs;
- uninstall removes the shortcut/program files but leaves `data/` and
  `localreel.toml` (they are not MSI components).

## Build

```powershell
# requires WiX 5 CLI (not v7 OSMF): dotnet tool install --global wix --version 5.0.2
cd localreel-core
.\scripts\package.ps1 -Profile release -SkipBuild
.\scripts\build-msi.ps1
# dist/localreel-<version>-x64.msi
```

`build-msi.ps1` runs `wix build`, then performs an administrative extract
(`msiexec /a`) and verifies `localreel.exe` is present. The MSI is not signed.

## Install / uninstall

```powershell
msiexec /i .\dist\localreel-0.1.0-x64.msi
msiexec /x .\dist\localreel-0.1.0-x64.msi
```

Verified in this workspace:

- silent install (`/qn`) → `%LOCALAPPDATA%\LocalReel\localreel.exe` +
  Start Menu shortcut;
- `localreel --version` printed 0.1.0;
- silent uninstall removed the program and shortcut;
- a `data/marker.txt` created after install survived uninstall (MSI leaves
  untracked data).

The MSI is per-user and unsigned; SmartScreen may warn. For a machine-wide
install or code signing, a different packaging decision is required.
