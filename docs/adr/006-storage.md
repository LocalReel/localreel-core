# ADR-006: Storage

- Status: Accepted
- Date: 2026-11-01

## Context

LocalReel is local-first. The vtx prototype introduced an `object_store`-based
VFS with lazy S3 stores and no root confinement: safety depended on call-site
API shapes. That is over-engineering for one directory tree and a security hole
if a future host function accepts a URI.

## Decision

The storage layer is a narrow provider seam, not a filesystem scattered through
the app.

- **`Storage` trait** in the kernel with four operations:
  `stat`, `list`, `open_range`, and `walk` (recursive, per-entry errors so one
  unreadable directory does not abort a scan). `MediaUri` (`file://`,
  `http(s)://`, `s3://`) is the serialized identity; `MediaUri::kind` selects
  the provider.
- **`LocalStorage`** is the only registered provider. Root confinement is
  enforced in one place: every resolved path is canonicalized and must start
  with a configured source root; `..`, symlinks and verbatim Windows prefixes are
  normalized before comparison.
- **`StorageRouter`** dispatches a URI to its provider. Local files are served
  today; `http` and `s3` URIs fail with "no storage provider registered" instead
  of pretending to work. Adding a provider means implementing the trait and
  adding a router arm.
- **Callers never touch the filesystem directly.** Library discovery walks via
  `Storage::walk`; HTTP streaming stats via `Storage::stat` and reads ranges via
  `Storage::open_range`. The media layer receives `&dyn Storage`, so tests can
  serve entries that do not exist on disk.
- Range reads stay synchronous (`Read + Send`) in the trait; the HTTP layer
  moves the open to a blocking task and pumps bytes into the response body
  through a bounded channel, so a slow client cannot pin an unbounded buffer.

## Consequences

- Windows/CJK/UNC paths are handled in one module with tests.
- Adding WebDAV/SMB/S3 later means adding a provider, not rewriting the scanner
  or the streaming handlers.
- The scan pipeline has a provider-level test (`MockStorage`) that proves
  discovery works without a filesystem.
- Root confinement remains mandatory for every local read; a remote provider is
  responsible for its own authorization.
