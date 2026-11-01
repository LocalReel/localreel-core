# Run LocalReel automatically (Windows)

The portable install creates a Start Menu shortcut but does not start a
background service. Two supported options:

## Option A — Task Scheduler (recommended)

Register a scheduled task that runs `localreel.exe serve` in the install
directory (so `localreel.toml` and `data/` resolve there):

```powershell
# from the extracted package / install directory
.\install-task.ps1
```

Or from the repository against a custom install:

```powershell
.\scripts\install-task.ps1 -InstallDir 'D:\Apps\LocalReel'
```

Defaults:

- Task name `LocalReel Server`
- Trigger: **at logon** of the current user (no admin required)
- Action: `<install>\localreel.exe serve`, working directory `<install>`
- Restart up to 3 times at 1-minute intervals; no execution time limit
- If the task is already registered, it is replaced

Start it immediately:

```powershell
.\install-task.ps1 -StartNow
```

Inspect / stop / remove:

```powershell
Get-ScheduledTask -TaskName 'LocalReel Server' | Get-ScheduledTaskInfo
Stop-ScheduledTask -TaskName 'LocalReel Server'
.\install-task.ps1 -Uninstall
.\install-task.ps1 -Uninstall -StopServer   # also stop a matching process
```

### Boot instead of logon

```powershell
# elevated PowerShell required
.\install-task.ps1 -AtStartup
```

`-AtStartup` registers the task as `SYSTEM` at boot. Use an install directory
whose data/config are writable by SYSTEM; a per-user `%LOCALAPPDATA%` install is
usually fine, but verify with a reboot. Prefer the default logon task for a
personal desktop.

## Option B — Startup folder shortcut

The installer can place a shortcut in the Startup folder:

```powershell
.\install.ps1 -StartOnLogin
```

This starts a console window at logon; closing the window stops the server.
The Task Scheduler option is quieter and supports restart-on-failure.

## Logs

There is no file logger in v1. The task runs without a console, so use
`RUST_LOG` and redirect if you need a log:

```powershell
# custom task action example
cmd /c ""D:\Apps\LocalReel\localreel.exe" serve 2>> "D:\Apps\LocalReel\localreel.log""
```

For health, use `GET /api/v1/health` and `GET /api/v1/admin/health`.

## Linux

Use the systemd **user** unit shown in [linux.md](linux.md). There is no
Task Scheduler equivalent.
