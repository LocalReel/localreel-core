# ADR-018: Relative paths belong to the config file

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

## Decision

Every configured path is made absolute once, at load time.

- A value from the config file is resolved against **the config file's
  directory**. `dir = "data"` next to a portable `localreel.toml` stays next to
  that package; a config in `%APPDATA%` keeps its data beside itself.
- A value from `LOCALREEL_*` is resolved against the working directory once, as
  a command-line argument would be, and is then absolute.
- Absolute values pass through untouched.
- The resolved `data_dir` is logged at startup and shown in the banner, so the
  effective location is never inferred from the configured spelling.

## Consequences

- Starting the server from any folder now yields the same library.
- A portable package keeps working: its config sits next to the executable, so
  file-relative values point where they always did.
- Moving the config file moves the relative paths with it. An operator who wants
  a location pinned to a drive writes an absolute path; the installer and the
  documentation recommend that when the library is on another volume.
- The remaining working-directory dependency is deliberate: legacy adoption
  (`<exe dir>/data`, `<cwd>/data`) only runs when no data directory was
  configured and the platform location is empty, and ffmpeg discovery also
  checks the working directory for a portable sidecar.
