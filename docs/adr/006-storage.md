# ADR-006: Storage

- Status: Accepted
- Date: 2026-11-01

## Context

LocalReel is local-first. The vtx prototype introduced an `object_store`-based
VFS with lazy S3 stores and no root confinement: safety depended on call-site
API shapes. That is over-engineering for one directory tree and a security
hole if a future host function accepts a URI.

## Decision

- v1 ships a **local-first `Storage` trait** with a single `LocalStorage`
  backend.
- Root confinement is enforced in one place: every resolved path is
  canonicalized and must start with a configured source root. `..`, symlinks
  and verbatim Windows prefixes are normalized before comparison.
- `MediaUri` is the serialized identity (`file://`, `http(s)://` for HLS
  passthrough, `s3://` parsed but not served). Storage is not a URI string
  parser scattered through the app.
- HTTP/HLS is passthrough only in v1; S3 and remote scanning are deferred.

## Consequences

- Windows/CJK/UNC paths are handled in one module with tests.
- Adding S3 later means adding a backend, not rewriting every I/O site.
- Extensions cannot read arbitrary files by constructing a URI; host functions
  take typed ids/roots, and `LocalStorage` still checks roots.
