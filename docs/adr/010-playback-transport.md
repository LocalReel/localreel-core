# ADR-010: Playback transport

- Status: Accepted
- Date: 2026-11-01

## Context

The library contains user files in many containers and codecs. The browser can
play some of them directly; others need the server to change the media before it
is sent. The server has three ways to satisfy a play request:

1. **Direct** — the file is already browser-compatible (MP4/M4V with H.264 and
   AAC/MP3, or WebM with VP8/VP9 and Opus/Vorbis). It is streamed with byte
   ranges.
2. **Remux** — the video stream is H.264 but the container is not MP4 (MKV, MOV,
   TS, M2TS, FLV, AVI). The streams are copied into MP4, which takes seconds.
3. **Transcode** — anything else is re-encoded to H.264 + AAC into a cached MP4
   variant. This takes minutes for long files and is cancellable.

A segmented protocol (HLS) would add a packaging job, playlist and segment
endpoints, path-traversal rules for segments, cache/GC integration and a client
library for browsers without native support. None of that changes what the
current server can play; it changes delivery.

## Decision

Playback uses the three transports above. The server does not produce HLS or any
other segmented output, and the UI does not load a client-side streaming library.

HLS **input** sources are not scanned as media sources.

## Consequences

- Playback compatibility is exactly what a browser can play directly or after
  the MP4 variant.
- No adaptive bitrate, no segment cleanup, no playlist endpoints.
- The cache GC manages thumbnails and MP4 transcodes only.
- A future transport (segmented or otherwise) is added as a new playback mode,
  not by changing the existing three.
