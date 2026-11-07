# ADR-012: Persistent state locations

- Status: Accepted
- Date: 2026-11-07

## Context

State used to resolve against the process working directory: `./data` for the
database, thumbnails and transcodes, `./localreel.toml` for configuration.

That layout is wrong for a packaged application. A shortcut starts the binary
with one working directory, a shell with another, and a service manager with a
third, so the same install could silently use different libraries. It also ties
the library to the install directory: removing or moving the program removed the
user's watch history, and a dead shortcut left nothing to diagnose.

## Decision

State lives in the location each platform defines for application data, never in
the working directory and never inside the install directory:

| Platform | Data | Config | Logs |
|---|---|---|---|
| Windows | `%LOCALAPPDATA%\LocalReel` | `%APPDATA%\LocalReel` | `<data>\logs` |
| macOS | `~/Library/Application Support/LocalReel` | same | `~/Library/Logs/LocalReel` |
| Linux/BSD | `$XDG_DATA_HOME/localreel` | `$XDG_CONFIG_HOME/localreel` | `$XDG_STATE_HOME/localreel/log` |

Rules:

- Explicit configuration wins, in order: `--mode`-independent overrides
  (`LOCALREEL_DATA_DIR`, `LOCALREEL_LOG_DIR`), the config file, then the platform
  defaults.
- The config file is looked up in the platform config directory, then the data
  directory (containers keep everything under `/data`), then the working
  directory.
- Upgrading, uninstalling or moving the program never touches the library.
- A `data/` directory left by an earlier version is adopted, with a warning,
  only when the platform location has no database and no explicit data directory
  was configured. Nothing is moved or copied automatically.
- The server writes `<log dir>/localreel.log`, rotating to `localreel.log.1` at
  8 MiB, because a shortcut window disappears and a service has no console.

## Consequences

- The install directory becomes replaceable: the MSI can upgrade, the portable
  zip can be extracted anywhere, and both address the same library.
- Backup, restore and support instructions have one stable path to point at.
- Container images keep `/data` as the single volume; the XDG rules are not
  involved because the image sets `LOCALREEL_DATA_DIR` explicitly.
