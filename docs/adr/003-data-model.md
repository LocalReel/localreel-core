# ADR-003: Data model and identity

- Status: Accepted
- Date: 2026-11-01

## Context

The previous schema was UI-shaped and path-keyed: `directories`, `videos`,
`video_states`, ad-hoc `ALTER TABLE` migrations, integer ids, and a rescan that
deleted rows (and cascaded away favorites/progress) when a source was
temporarily unavailable.

## Decision

- Tables: `sources`, `media`, `media_state`, `scan_runs`, `scan_issues`,
  `media_variants`, `media_fts`.
- Integer primary keys remain (simple joins, URLs, UI), plus a `uid TEXT UNIQUE`
  for portable identity across backups/caches.
- Timestamps are Unix seconds, one unit, everywhere.
- `media.missing_at` marks "not seen in the last complete scan". **Missing is
  not deleted.** Only an explicit user action removes a source/media and its
  state.
- Generated artifacts are registered in `media_variants`; cache GC never parses
  file names.
- Migrations are per-scope (`kernel`, `media`) in `sys_migrations`, applied in
  order by the server. No ad-hoc `pragma_table_info` patches.

## Consequences

- Legacy LocalReel data is imported by mapping `directories -> sources`,
  `videos -> media`, `video_states -> media_state`, `scan_jobs -> sys_jobs`.
- A moved/renamed file can still lose its state unless a future content hash or
  inode-based identity is added; `uid` makes that migration possible later.
- Source roots are normalized; case/Unicode normalization is the storage
  layer's job, not the UI's.
