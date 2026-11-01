# ADR-005: Media operations

- Status: Accepted
- Date: 2026-11-01

## Context

The vtx prototype exposed raw ffmpeg argv to plugins. Option keys were
unrestricted, timeouts were configured but ignored, stdin could never close in
pipe mode, and progress was invisible.

## Decision

- The kernel exposes **typed operations**: `probe`, `thumbnail`, `transcode`
  (and later `remux`/`package_hls`). No caller passes arbitrary argv.
- Operations have explicit timeouts, a cancellation flag, kill-on-drop behavior
  and bounded stderr capture.
- Transcode uses named presets (`TranscodePreset`), not free-form flags.
- Progress is parsed from ffmpeg's `-progress pipe:1` and reported through the
  job context.
- The binary is resolved from config (`LOCALREEL_FFMPEG`/`LOCALREEL_FFPROBE`)
  with a PATH fallback; availability is checked once at startup and surfaced in
  `/api/v1/health`.

## Consequences

- No command-injection surface for extensions or API callers.
- Hardware acceleration, multi-rendition HLS, subtitle/audio track selection are
  explicit future presets, not ad-hoc flags.
- A missing ffmpeg is a degraded mode, not a boot failure.
