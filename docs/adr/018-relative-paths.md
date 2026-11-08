# ADR-018: Relative paths belong to the install directory

- Status: Accepted
- Date: 2026-11-08

## Context

A `[data] dir = "data"` in `%APPDATA%\LocalReel\localreel.toml` was joined onto
whatever working directory the server happened to have. The same installation
therefore used a different library depending on how it was started: the report
that exposed this had the install on `D:\AppData\LocalReel` and the store in
`G:\Downloads\data`, because the server was launched from that folder.

The failure is quiet. The database, thumbnails, HLS segments and transcodes all
derive from `data_dir`, so a wrong base creates a second, empty library and the
old one looks like it vanished. Launch methods differ by user: a shortcut with
"Start in", a double-click (the executable's folder), a terminal (its current
directory), a service manager, a scheduled task.

A household server is also expected to behave like a folder of software: copy
the installed folder to another drive and it keeps working, library included.
That is what the portable layout already did, and what the per-user install
should do too.

## Decision

Every configured path is made absolute once, at load time, against a stable
base.

- A relative path from the config file is resolved against **the program's
  directory**, the folder that contains `localreel.exe`.
- A value from `LOCALREEL_*` is resolved against the working directory once, as
  a command-line argument would be, and is then absolute.
- Absolute values pass through untouched.
- The resolved `data_dir` is logged at startup and shown in the banner, so the
  effective location is never inferred from the configured spelling.
- The program directory is the reference point even when the config lives in
  `%APPDATA%`: an installed copy keeps its library next to itself.

## Consequences

- Starting the server from any folder yields the same library.
- Moving or copying the installed folder moves the library with it.
- The installer must not treat the install folder as purely its own: uninstall
  removes only the files recorded in `install.json` and leaves everything else,
  which is how a library under `<install>\data` survives. The zip uninstaller
  moves that folder aside and back.
- An all-users install under `C:\Program Files` cannot write next to itself; a
  relative `[data] dir` there fails to create the library, so the documentation
  recommends an absolute path for machine-wide installs.
- The remaining working-directory dependency is deliberate: legacy adoption
  (`<exe dir>/data`, `<cwd>/data`) only runs when no data directory was
  configured and the platform location is empty, and ffmpeg discovery also
  checks the working directory for a portable sidecar.
