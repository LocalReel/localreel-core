# ADR-010: HLS output deferred to v1.1

- Status: Deferred
- Date: 2026-11-01

## Context

The v1 playback path already covers the actual library:

- Direct play for H.264/AAC MP4 (the real 12-file library) with byte ranges.
- A persistent `transcode` job produces an MP4 variant for incompatible inputs.
- The UI plays those through a plain `<video>` element.

HLS output was listed as a later option, not a v1 requirement. HLS adds:

- a segmentation pipeline (`ffmpeg -f hls`), playlist/segment files and
  variant bookkeeping;
- playlist endpoints, path traversal rules and cache/GC integration;
- browser packaging (hls.js is required in Chrome/Firefox for HLS);
- bitrate/rendition decisions and a much larger test matrix (Safari, mobile,
  network stalls, segment cleanup).

The product is local, single-user and Windows-first. No current user need
requires adaptive bitrate or a segmented protocol; direct play + MP4 transcode is
simpler and already verified on Windows, WSL2 Linux and Docker.

## Decision

Defer HLS output to v1.1. v1 continues to serve:

- direct stream (`/media/{id}/stream`) for browser-compatible files;
- progressive MP4 transcode variants for incompatible files;
- HLS **input** sources remain deferred (not silently scanned).

Do not add hls.js, segment endpoints, or an HLS job to v1.

## Consequences

- No adaptive bitrate / no native Safari HLS in v1.
- Playback compatibility is limited to what the browser can play directly or
  after the MP4 transcode preset.
- The v1 cache GC only manages MP4 transcodes and thumbnails; no HLS segments.
- If remote/NAS/mobile becomes a real requirement, revisit this ADR.

## Revisit triggers and sketch

Revisit when any of these becomes a real requirement:

- remote access/multi-device streaming over variable bandwidth;
- a meaningful share of files that cannot direct-play even after MP4 transcode;
- Safari/iOS as a first-class target;
- adaptive bitrate (360p/720p) requests.

Sketch if accepted:

1. Job kind `hls_package` with `media_id` + preset; output
   `<data>/hls/<uid>/<preset>/index.m3u8` + segments.
2. ffmpeg: `-f hls -hls_time 4 -hls_playlist_type vod
   -hls_segment_filename ...`; atomic temp dir then rename.
3. Variant record kind `hls`, playlist path stored; cache GC owns segment
   directories and removes stale ones.
4. API: `GET /media/{id}/hls/index.m3u8` and segment route with the same
   canonical-path/traversal protections as the stream route.
5. UI: try direct first; only load hls.js when direct fails or when the API
   advertises an HLS variant. Keep the dependency out of the base bundle if
   possible.
6. Tests: playlist/segment path traversal, cancel/cleanup, one real 12-file
   playback, Chrome + Safari smoke.
