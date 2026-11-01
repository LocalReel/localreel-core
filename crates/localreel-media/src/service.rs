use std::collections::HashSet;
use std::path::Path;

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::media_ops::ProbeResult;
use localreel_kernel::storage::{normalize_local_path, MediaUri};
use localreel_kernel::time::now_secs;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension, Row};

use crate::model::{
    CacheGcReport, CacheStats, Media, MediaFilter, MediaState, ScanIssue, ScanRun, Source,
};

/// First-party media service.
///
/// Phase 0 only covers sources and read/state access. Scan/probe/thumbnail/
/// transcode handlers are added in Phase 1/2 and registered with the kernel
/// [`localreel_kernel::jobs::JobRegistry`].
#[derive(Clone)]
pub struct MediaService {
    db: Database,
}

impl MediaService {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn get_source(&self, id: i64) -> Result<Option<Source>> {
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT id, uri, kind, enabled, created_at, updated_at, archived_at
                     FROM sources WHERE id = ?1",
                    [id],
                    row_to_source,
                )
                .optional()?)
        })
    }

    pub fn list_sources(&self) -> Result<Vec<Source>> {
        self.list_sources_with_archived(false)
    }

    pub fn list_sources_with_archived(&self, include_archived: bool) -> Result<Vec<Source>> {
        self.db.with_read(|connection| {
            let sql = if include_archived {
                "SELECT id, uri, kind, enabled, created_at, updated_at, archived_at
                 FROM sources ORDER BY id ASC"
            } else {
                "SELECT id, uri, kind, enabled, created_at, updated_at, archived_at
                 FROM sources WHERE archived_at IS NULL ORDER BY id ASC"
            };
            let mut statement = connection.prepare(sql)?;
            let rows = statement.query_map([], row_to_source)?;
            let mut sources = Vec::new();
            for row in rows {
                sources.push(row?);
            }
            Ok(sources)
        })
    }

    /// Add a local directory or HLS endpoint. Local paths are canonicalized and
    /// must exist; duplicates are returned as-is.
    pub fn add_source(&self, raw_uri: &str) -> Result<Source> {
        let parsed = MediaUri::parse(raw_uri)?;
        let normalized = match parsed {
            MediaUri::Local(path) => {
                let canonical = path.canonicalize().map_err(|error| {
                    KernelError::InvalidInput(format!(
                        "source does not exist: {} ({error})",
                        path.display()
                    ))
                })?;
                if !canonical.is_dir() {
                    return Err(KernelError::InvalidInput(format!(
                        "source is not a directory: {}",
                        canonical.display()
                    )));
                }
                MediaUri::Local(canonical)
            }
            other => other,
        };
        let uri = normalized.to_uri();
        let kind = match normalized {
            MediaUri::Local(_) => "local",
            MediaUri::Http(_) => "hls",
            MediaUri::S3 { .. } => "s3",
        };
        let now = now_secs();

        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO sources (uri, kind, enabled, created_at, updated_at, archived_at)
                 VALUES (?1, ?2, 1, ?3, ?3, NULL)
                 ON CONFLICT(uri) DO UPDATE SET
                     kind = excluded.kind,
                     enabled = 1,
                     archived_at = NULL,
                     updated_at = excluded.updated_at",
                params![uri, kind, now],
            )?;
            let source = connection
                .query_row(
                    "SELECT id, uri, kind, enabled, created_at, updated_at, archived_at
                     FROM sources WHERE uri = ?1",
                    [&uri],
                    row_to_source,
                )
                .optional()?
                .ok_or_else(|| KernelError::NotFound("source after insert".into()))?;
            Ok(source)
        })
    }

    pub fn set_source_enabled(&self, id: i64, enabled: bool) -> Result<Source> {
        let now = now_secs();
        self.db.with_write(|connection| {
            let changed = connection.execute(
                "UPDATE sources SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
                params![if enabled { 1 } else { 0 }, now, id],
            )?;
            if changed == 0 {
                return Err(KernelError::NotFound(format!("source {id}")));
            }
            Ok(connection.query_row(
                "SELECT id, uri, kind, enabled, created_at, updated_at, archived_at
                 FROM sources WHERE id = ?1",
                [id],
                row_to_source,
            )?)
        })
    }

    pub fn set_source_archived(&self, id: i64, archived: bool) -> Result<Source> {
        let now = now_secs();
        self.db.with_write(|connection| {
            let changed = connection.execute(
                "UPDATE sources
                 SET archived_at = ?1, enabled = ?2, updated_at = ?3
                 WHERE id = ?4",
                params![
                    if archived { Some(now) } else { None },
                    if archived { 0 } else { 1 },
                    now,
                    id
                ],
            )?;
            if changed == 0 {
                return Err(KernelError::NotFound(format!("source {id}")));
            }
            Ok(connection.query_row(
                "SELECT id, uri, kind, enabled, created_at, updated_at, archived_at
                 FROM sources WHERE id = ?1",
                [id],
                row_to_source,
            )?)
        })
    }

    /// Move a local source to a new directory without losing media ids/state.
    ///
    /// Only media whose old canonical path is under the old root and whose new
    /// file exists are rewritten. Missing/out-of-root rows are reported and
    /// left untouched. `dry_run` computes the same report without writing.
    pub fn remap_source(
        &self,
        id: i64,
        new_uri_raw: &str,
        dry_run: bool,
    ) -> Result<crate::model::RemapReport> {
        let source = self
            .get_source(id)?
            .ok_or_else(|| KernelError::NotFound(format!("source {id}")))?;
        let old_root = match MediaUri::parse(&source.uri)? {
            MediaUri::Local(path) => normalize_local_path(&path),
            _ => {
                return Err(KernelError::InvalidInput(
                    "only local sources can be remapped".into(),
                ))
            }
        };
        let new_root = match MediaUri::parse(new_uri_raw)? {
            MediaUri::Local(path) => path,
            _ => {
                return Err(KernelError::InvalidInput(
                    "new remap target must be a local directory".into(),
                ))
            }
        };
        let new_root = normalize_local_path(&new_root.canonicalize().map_err(|error| {
            KernelError::InvalidInput(format!(
                "new source does not exist: {} ({error})",
                new_root.display()
            ))
        })?);
        if !new_root.is_dir() {
            return Err(KernelError::InvalidInput(format!(
                "new source is not a directory: {}",
                new_root.display()
            )));
        }
        let new_uri = MediaUri::Local(new_root.clone()).to_uri();

        let conflict: Option<i64> = self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT id FROM sources WHERE uri = ?1 AND id <> ?2",
                    params![new_uri, id],
                    |row| row.get(0),
                )
                .optional()?)
        })?;
        if conflict.is_some() {
            return Err(KernelError::Conflict(format!(
                "another source already uses {new_uri}"
            )));
        }

        let media_rows: Vec<(i64, String)> = self.db.with_read(|connection| {
            let mut statement =
                connection.prepare("SELECT id, canonical_path FROM media WHERE source_id = ?1")?;
            let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            let mut media = Vec::new();
            for row in rows {
                media.push(row?);
            }
            Ok(media)
        })?;

        let mut updates = Vec::new();
        let mut report = crate::model::RemapReport {
            source_id: id,
            old_uri: source.uri.clone(),
            new_uri: new_uri.clone(),
            dry_run,
            media_total: media_rows.len() as i64,
            ..crate::model::RemapReport::default()
        };
        for (media_id, canonical_path) in &media_rows {
            let old_path = normalize_local_path(Path::new(canonical_path));
            let Ok(relative) = old_path.strip_prefix(&old_root) else {
                report.skipped += 1;
                continue;
            };
            let new_path = new_root.join(relative);
            if !new_path.is_file() {
                report.missing += 1;
                continue;
            }
            report.remapped += 1;
            updates.push((
                *media_id,
                MediaUri::Local(new_path.clone()).to_uri(),
                new_path.to_string_lossy().into_owned(),
            ));
        }

        if report.missing > 0 {
            report.warnings.push(format!(
                "{} media files were not found under the new root; their URIs were left unchanged",
                report.missing
            ));
        }

        if !dry_run {
            let now = now_secs();
            self.db.with_write(|connection| {
                let transaction = connection.transaction()?;
                for (media_id, uri, canonical_path) in &updates {
                    transaction.execute(
                        "UPDATE media SET uri = ?1, canonical_path = ?2, updated_at = ?3
                         WHERE id = ?4",
                        params![uri, canonical_path, now, media_id],
                    )?;
                }
                transaction.execute(
                    "UPDATE sources SET uri = ?1, updated_at = ?2 WHERE id = ?3",
                    params![new_uri, now, id],
                )?;
                transaction.commit()?;
                Ok(())
            })?;
        }

        Ok(report)
    }

    /// Delete a source after an explicit confirmation.
    ///
    /// `delete_media=false` is the safe default: if the source still has media,
    /// the call is refused with a conflict so the caller can show counts and
    /// ask again. `dry_run=true` returns the same counts without changing
    /// anything. When confirmed, media/state/variants cascade via the schema;
    /// known thumbnail/transcode files are removed first, best-effort.
    pub fn delete_source(
        &self,
        id: i64,
        delete_media: bool,
        dry_run: bool,
        thumbnail_dir: &Path,
        transcode_dir: &Path,
    ) -> Result<crate::model::DeleteSourceReport> {
        let source = self
            .get_source(id)?
            .ok_or_else(|| KernelError::NotFound(format!("source {id}")))?;
        let (media_count, state_count, variants): (i64, i64, Vec<(String, String)>) =
            self.db.with_read(|connection| {
                let media_count: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM media WHERE source_id = ?1",
                    [id],
                    |row| row.get(0),
                )?;
                let state_count: i64 = connection.query_row(
                    "SELECT COUNT(*) FROM media_state s
                     JOIN media m ON m.id = s.media_id
                     WHERE m.source_id = ?1",
                    [id],
                    |row| row.get(0),
                )?;
                let mut statement = connection.prepare(
                    "SELECT v.kind, v.path FROM media_variants v
                     JOIN media m ON m.id = v.media_id
                     WHERE m.source_id = ?1",
                )?;
                let rows = statement.query_map([id], |row| Ok((row.get(0)?, row.get(1)?)))?;
                let mut variants = Vec::new();
                for row in rows {
                    variants.push(row?);
                }
                Ok((media_count, state_count, variants))
            })?;

        let mut report = crate::model::DeleteSourceReport {
            source_id: source.id,
            source_uri: source.uri.clone(),
            dry_run,
            archived: media_count > 0 && !delete_media,
            media_count,
            state_count,
            variant_count: variants.len() as i64,
            deleted_files: 0,
        };
        if dry_run {
            return Ok(report);
        }
        if media_count > 0 && !delete_media {
            self.set_source_archived(id, true)?;
            report.archived = true;
            return Ok(report);
        }

        for (kind, path) in &variants {
            if !is_safe_variant_name(path) {
                continue;
            }
            let directory = match kind.as_str() {
                "thumbnail" => thumbnail_dir,
                "transcode" => transcode_dir,
                _ => continue,
            };
            match std::fs::remove_file(directory.join(path)) {
                Ok(()) => report.deleted_files += 1,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(path = %path, "failed to delete source artifact: {error}");
                }
            }
        }

        self.db.with_write(|connection| {
            connection.execute("DELETE FROM sources WHERE id = ?1", [id])?;
            Ok(())
        })?;
        Ok(report)
    }

    pub fn list_media(&self, limit: i64, offset: i64) -> Result<Vec<Media>> {
        let limit = limit.clamp(1, 500);
        let offset = offset.max(0);
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, uid, source_id, uri, canonical_path, filename, title,
                        duration, width, height, file_size, modified_at, video_codec,
                        audio_codec, bitrate, thumbnail_path, missing_at, created_at,
                        updated_at
                 FROM media
                 ORDER BY created_at DESC, id DESC
                 LIMIT ?1 OFFSET ?2",
            )?;
            let rows = statement.query_map(params![limit, offset], row_to_media)?;
            let mut media = Vec::new();
            for row in rows {
                media.push(row?);
            }
            Ok(media)
        })
    }

    /// Paginated media list plus the total count, optionally filtered by source.
    pub fn list_media_page(
        &self,
        limit: i64,
        offset: i64,
        source_id: Option<i64>,
        query: Option<&str>,
        filter: MediaFilter,
        include_archived_sources: bool,
    ) -> Result<(Vec<Media>, i64)> {
        let limit = limit.clamp(1, 200);
        let offset = offset.max(0);
        self.db.with_read(|connection| {
            let mut conditions = Vec::new();
            let mut values: Vec<Value> = Vec::new();

            if let Some(source_id) = source_id {
                conditions.push("m.source_id = ?");
                values.push(source_id.into());
            }
            if !include_archived_sources {
                conditions
                    .push("m.source_id IN (SELECT id FROM sources WHERE archived_at IS NULL)");
            }
            if let Some(query) = query.map(str::trim).filter(|query| !query.is_empty()) {
                let pattern = format!("%{query}%");
                conditions.push("(m.title LIKE ? OR m.filename LIKE ? OR m.uri LIKE ?)");
                values.push(pattern.clone().into());
                values.push(pattern.clone().into());
                values.push(pattern.into());
            }
            match filter {
                MediaFilter::All => {}
                MediaFilter::Favorites => conditions.push("s.favorite = 1"),
                MediaFilter::Continue => conditions.push(
                    "s.position > 0.0 AND s.duration > 0.0 AND s.position < s.duration - 5.0",
                ),
            }

            let where_clause = if conditions.is_empty() {
                String::new()
            } else {
                format!("WHERE {}", conditions.join(" AND "))
            };
            let from = "FROM media m LEFT JOIN media_state s ON s.media_id = m.id";
            let count_sql = format!("SELECT COUNT(*) {from} {where_clause}");
            let total: i64 =
                connection.query_row(&count_sql, params_from_iter(values.iter()), |row| {
                    row.get(0)
                })?;

            let order = match filter {
                MediaFilter::Continue | MediaFilter::Favorites => "s.updated_at DESC, m.id DESC",
                MediaFilter::All => "m.created_at DESC, m.id DESC",
            };
            let list_sql = format!(
                "SELECT m.id, m.uid, m.source_id, m.uri, m.canonical_path, m.filename,
                        m.title, m.duration, m.width, m.height, m.file_size, m.modified_at,
                        m.video_codec, m.audio_codec, m.bitrate, m.thumbnail_path,
                        m.missing_at, m.created_at, m.updated_at
                 {from} {where_clause}
                 ORDER BY {order}
                 LIMIT ? OFFSET ?"
            );
            values.push(limit.into());
            values.push(offset.into());
            let mut statement = connection.prepare(&list_sql)?;
            let rows = statement.query_map(params_from_iter(values.iter()), row_to_media)?;
            let mut media = Vec::new();
            for row in rows {
                media.push(row?);
            }
            Ok((media, total))
        })
    }

    /// Update the metadata fields produced by `ffprobe`.
    pub fn update_probe(&self, media_id: i64, probe: &ProbeResult) -> Result<()> {
        self.db.with_write(|connection| {
            connection.execute(
                "UPDATE media SET
                     duration = ?1, width = ?2, height = ?3, video_codec = ?4,
                     audio_codec = ?5, bitrate = ?6, updated_at = ?7
                 WHERE id = ?8",
                params![
                    probe.duration,
                    probe.width,
                    probe.height,
                    probe.video_codec,
                    probe.audio_codec,
                    probe.bitrate,
                    now_secs(),
                    media_id
                ],
            )?;
            Ok(())
        })
    }

    pub fn set_thumbnail(&self, media_id: i64, thumbnail_path: &str) -> Result<()> {
        self.db.with_write(|connection| {
            connection.execute(
                "UPDATE media SET thumbnail_path = ?1, updated_at = ?2 WHERE id = ?3",
                params![thumbnail_path, now_secs(), media_id],
            )?;
            Ok(())
        })
    }

    pub fn find_variant(
        &self,
        media_id: i64,
        kind: &str,
        preset: &str,
    ) -> Result<Option<crate::model::MediaVariant>> {
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT id, media_id, kind, preset, path, size, created_at, last_used_at
                     FROM media_variants
                     WHERE media_id = ?1 AND kind = ?2 AND preset = ?3
                     ORDER BY created_at DESC LIMIT 1",
                    params![media_id, kind, preset],
                    |row| {
                        Ok(crate::model::MediaVariant {
                            id: row.get(0)?,
                            media_id: row.get(1)?,
                            kind: row.get(2)?,
                            preset: row.get(3)?,
                            path: row.get(4)?,
                            size: row.get(5)?,
                            created_at: row.get(6)?,
                            last_used_at: row.get(7)?,
                        })
                    },
                )
                .optional()?)
        })
    }

    pub fn record_variant(
        &self,
        media_id: i64,
        kind: &str,
        preset: &str,
        path: &str,
        size: i64,
    ) -> Result<()> {
        let now = now_secs();
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO media_variants (media_id, kind, preset, path, size, created_at, last_used_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(media_id, kind, preset, path) DO UPDATE SET
                     size = excluded.size, last_used_at = excluded.last_used_at",
                params![media_id, kind, preset, path, size, now],
            )?;
            Ok(())
        })
    }

    pub fn touch_variant(&self, media_id: i64, preset: &str) -> Result<()> {
        self.db.with_write(|connection| {
            connection.execute(
                "UPDATE media_variants SET last_used_at = ?1
                 WHERE media_id = ?2 AND kind = 'transcode' AND preset = ?3",
                params![now_secs(), media_id, preset],
            )?;
            Ok(())
        })
    }

    pub fn cache_stats(&self) -> Result<CacheStats> {
        self.db.with_read(|connection| {
            let mut stats = CacheStats::default();
            let total: i64 = connection.query_row(
                "SELECT COALESCE(SUM(size), 0) FROM media_variants",
                [],
                |row| row.get(0),
            )?;
            let mut statement = connection.prepare(
                "SELECT kind, COUNT(*), COALESCE(SUM(size), 0)
                 FROM media_variants GROUP BY kind",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            for row in rows {
                let (kind, count, bytes) = row?;
                match kind.as_str() {
                    "transcode" => {
                        stats.transcode_count = count;
                        stats.transcode_bytes = bytes;
                    }
                    "thumbnail" => {
                        stats.thumbnail_count = count;
                        stats.thumbnail_bytes = bytes;
                    }
                    _ => {}
                }
            }
            stats.total_bytes = total;
            Ok(stats)
        })
    }

    /// Reclaim transcode variants by age and then by total size.
    ///
    /// Thumbnails are intentionally excluded. Files touched within
    /// `protect_recent_secs` (active/recent playback) are never candidates.
    /// Paths containing directories are skipped so a poisoned DB row cannot
    /// delete outside the transcode directory.
    pub fn gc_transcode_cache(
        &self,
        transcode_dir: &Path,
        max_bytes: u64,
        max_age_secs: i64,
        protect_recent_secs: i64,
    ) -> Result<CacheGcReport> {
        let now = now_secs();
        let variants = self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, media_id, kind, preset, path, size, created_at, last_used_at
                 FROM media_variants
                 WHERE kind = 'transcode'
                 ORDER BY COALESCE(last_used_at, created_at) ASC, id ASC",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(crate::model::MediaVariant {
                    id: row.get(0)?,
                    media_id: row.get(1)?,
                    kind: row.get(2)?,
                    preset: row.get(3)?,
                    path: row.get(4)?,
                    size: row.get(5)?,
                    created_at: row.get(6)?,
                    last_used_at: row.get(7)?,
                })
            })?;
            let mut variants = Vec::new();
            for row in rows {
                variants.push(row?);
            }
            Ok(variants)
        })?;

        let total: i64 = variants.iter().map(|variant| variant.size).sum();
        let is_protected = |variant: &crate::model::MediaVariant| {
            protect_recent_secs > 0
                && variant.last_used_at.unwrap_or(variant.created_at) >= now - protect_recent_secs
        };
        let is_expired = |variant: &crate::model::MediaVariant| {
            max_age_secs > 0
                && variant.last_used_at.unwrap_or(variant.created_at) < now - max_age_secs
        };

        let mut remove_ids = HashSet::new();
        let mut remaining = total;
        for variant in &variants {
            if !is_protected(variant) && is_expired(variant) {
                remove_ids.insert(variant.id);
                remaining -= variant.size;
            }
        }
        if remaining.max(0) as u64 > max_bytes {
            for variant in &variants {
                if remaining.max(0) as u64 <= max_bytes {
                    break;
                }
                if is_protected(variant) || remove_ids.contains(&variant.id) {
                    continue;
                }
                remove_ids.insert(variant.id);
                remaining -= variant.size;
            }
        }

        let mut removed = 0i64;
        let mut freed_bytes = 0i64;
        for variant in &variants {
            if !remove_ids.contains(&variant.id) {
                continue;
            }
            if !is_safe_variant_name(&variant.path) {
                continue;
            }
            let file = transcode_dir.join(&variant.path);
            match std::fs::remove_file(&file) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(path = %file.display(), "cache GC could not remove file: {error}");
                    continue;
                }
            }
            self.db.with_write(|connection| {
                connection.execute("DELETE FROM media_variants WHERE id = ?1", [variant.id])?;
                Ok(())
            })?;
            removed += 1;
            freed_bytes += variant.size;
        }

        Ok(CacheGcReport {
            removed,
            freed_bytes,
            kept: variants.len() as i64 - removed,
            remaining_bytes: total - freed_bytes,
        })
    }

    /// Media rows that still need metadata or a thumbnail for a source.
    pub fn probe_candidate_ids(&self, source_id: i64) -> Result<Vec<i64>> {
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id FROM media
                 WHERE source_id = ?1
                   AND (video_codec IS NULL OR width IS NULL OR thumbnail_path IS NULL)
                 ORDER BY id ASC",
            )?;
            let rows = statement.query_map([source_id], |row| row.get(0))?;
            let mut ids = Vec::new();
            for row in rows {
                ids.push(row?);
            }
            Ok(ids)
        })
    }

    pub fn list_scan_runs(&self, limit: i64) -> Result<Vec<ScanRun>> {
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, source_id, source_uri, started_at, finished_at, status,
                        discovered, added, updated, unchanged, removed, failed
                 FROM scan_runs
                 ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map([limit.clamp(1, 200)], |row| {
                Ok(ScanRun {
                    id: row.get(0)?,
                    source_id: row.get(1)?,
                    source_uri: row.get(2)?,
                    started_at: row.get(3)?,
                    finished_at: row.get(4)?,
                    status: row.get(5)?,
                    discovered: row.get(6)?,
                    added: row.get(7)?,
                    updated: row.get(8)?,
                    unchanged: row.get(9)?,
                    removed: row.get(10)?,
                    failed: row.get(11)?,
                })
            })?;
            let mut runs = Vec::new();
            for row in rows {
                runs.push(row?);
            }
            Ok(runs)
        })
    }

    pub fn list_scan_issues(&self, limit: i64) -> Result<Vec<ScanIssue>> {
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, run_id, source_id, path, message, created_at
                 FROM scan_issues ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map([limit.clamp(1, 200)], |row| {
                Ok(ScanIssue {
                    id: row.get(0)?,
                    run_id: row.get(1)?,
                    source_id: row.get(2)?,
                    path: row.get(3)?,
                    message: row.get(4)?,
                    created_at: row.get(5)?,
                })
            })?;
            let mut issues = Vec::new();
            for row in rows {
                issues.push(row?);
            }
            Ok(issues)
        })
    }

    pub fn get_media(&self, id: i64) -> Result<Option<Media>> {
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT id, uid, source_id, uri, canonical_path, filename, title,
                            duration, width, height, file_size, modified_at, video_codec,
                            audio_codec, bitrate, thumbnail_path, missing_at, created_at,
                            updated_at
                     FROM media WHERE id = ?1",
                    [id],
                    row_to_media,
                )
                .optional()?)
        })
    }

    pub fn get_state(&self, media_id: i64) -> Result<MediaState> {
        self.db.with_read(|connection| {
            let state = connection
                .query_row(
                    "SELECT media_id, liked, favorite, position, duration, updated_at
                     FROM media_state WHERE media_id = ?1",
                    [media_id],
                    |row| {
                        Ok(MediaState {
                            media_id: row.get(0)?,
                            liked: row.get::<_, i64>(1)? != 0,
                            favorite: row.get::<_, i64>(2)? != 0,
                            position: row.get(3)?,
                            duration: row.get(4)?,
                            updated_at: row.get(5)?,
                        })
                    },
                )
                .optional()?;
            Ok(state.unwrap_or(MediaState {
                media_id,
                liked: false,
                favorite: false,
                position: 0.0,
                duration: 0.0,
                updated_at: 0,
            }))
        })
    }

    /// Upsert the full state object. Callers merge partial patches before
    /// calling this; doing the merge in one writer transaction avoids the
    /// lost-update race in the previous implementation.
    pub fn save_state(&self, state: &MediaState) -> Result<MediaState> {
        let now = now_secs();
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO media_state (media_id, liked, favorite, position, duration, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(media_id) DO UPDATE SET
                     liked = excluded.liked,
                     favorite = excluded.favorite,
                     position = excluded.position,
                     duration = excluded.duration,
                     updated_at = excluded.updated_at",
                params![
                    state.media_id,
                    state.liked as i64,
                    state.favorite as i64,
                    state.position,
                    state.duration,
                    now
                ],
            )?;
            let mut saved = state.clone();
            saved.updated_at = now;
            Ok(saved)
        })
    }

    pub fn media_count(&self) -> Result<i64> {
        self.db.with_read(|connection| {
            Ok(connection.query_row("SELECT COUNT(*) FROM media", [], |row| row.get(0))?)
        })
    }
}

fn row_to_source(row: &Row<'_>) -> rusqlite::Result<Source> {
    Ok(Source {
        id: row.get(0)?,
        uri: row.get(1)?,
        kind: row.get(2)?,
        enabled: row.get::<_, i64>(3)? != 0,
        created_at: row.get(4)?,
        updated_at: row.get(5)?,
        archived_at: row.get(6)?,
    })
}

fn row_to_media(row: &Row<'_>) -> rusqlite::Result<Media> {
    Ok(Media {
        id: row.get(0)?,
        uid: row.get(1)?,
        source_id: row.get(2)?,
        uri: row.get(3)?,
        canonical_path: row.get(4)?,
        filename: row.get(5)?,
        title: row.get(6)?,
        duration: row.get(7)?,
        width: row.get(8)?,
        height: row.get(9)?,
        file_size: row.get(10)?,
        modified_at: row.get(11)?,
        video_codec: row.get(12)?,
        audio_codec: row.get(13)?,
        bitrate: row.get(14)?,
        thumbnail_path: row.get(15)?,
        missing_at: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
    })
}

fn is_safe_variant_name(name: &str) -> bool {
    let path = Path::new(name);
    path.components().count() == 1 && path.file_name().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::migrations;

    use crate::scan::discover_source;

    fn setup() -> (tempfile::TempDir, MediaService) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("media.db");
        let db = Database::open(db_path.to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        migrations::apply(&db, "media", crate::migrations::list()).unwrap();
        (dir, MediaService::new(db))
    }

    #[test]
    fn adds_a_local_source_idempotently() {
        let (dir, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let first = service.add_source(source_dir.to_str().unwrap()).unwrap();
        let second = service.add_source(source_dir.to_str().unwrap()).unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(service.list_sources().unwrap().len(), 1);
        assert_eq!(first.kind, "local");
    }

    #[test]
    fn toggles_source_enabled_immediately() {
        let (dir, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();
        assert!(source.enabled);

        let disabled = service.set_source_enabled(source.id, false).unwrap();
        assert!(!disabled.enabled);
        assert!(!service.get_source(source.id).unwrap().unwrap().enabled);

        let enabled = service.set_source_enabled(source.id, true).unwrap();
        assert!(enabled.enabled);
    }

    fn insert_media(service: &MediaService, source_id: i64, uid: &str) -> i64 {
        let now = now_secs();
        service
            .db
            .with_write(|connection| {
                connection.execute(
                    "INSERT INTO media (
                         uid, source_id, uri, canonical_path, filename, file_size,
                         modified_at, created_at, updated_at
                     ) VALUES (?1, ?2, ?3, ?3, ?4, 0, 0, ?5, ?5)",
                    params![
                        uid,
                        source_id,
                        format!("file:///cache/{uid}.mp4"),
                        format!("{uid}.mp4"),
                        now
                    ],
                )?;
                Ok(connection.last_insert_rowid())
            })
            .unwrap()
    }

    #[test]
    fn cache_stats_and_gc_respect_budget() {
        let (dir, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();
        let media_id = insert_media(&service, source.id, "cache-a");

        let transcode_dir = dir.path().join("transcodes");
        std::fs::create_dir_all(&transcode_dir).unwrap();
        for name in ["a.mp4", "b.mp4", "c.mp4"] {
            std::fs::write(transcode_dir.join(name), vec![0u8; 100]).unwrap();
            service
                .record_variant(media_id, "transcode", "h264_aac", name, 100)
                .unwrap();
        }

        let stats = service.cache_stats().unwrap();
        assert_eq!(stats.transcode_count, 3);
        assert_eq!(stats.transcode_bytes, 300);

        let report = service
            .gc_transcode_cache(&transcode_dir, 150, 86_400, 0)
            .unwrap();
        assert_eq!(report.removed, 2);
        assert_eq!(report.freed_bytes, 200);
        assert_eq!(report.kept, 1);
        assert_eq!(report.remaining_bytes, 100);
        assert_eq!(service.cache_stats().unwrap().transcode_count, 1);
        assert_eq!(std::fs::read_dir(&transcode_dir).unwrap().count(), 1);
    }

    #[test]
    fn cache_gc_skips_recently_used_variants() {
        let (dir, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();
        let media_id = insert_media(&service, source.id, "cache-b");
        let transcode_dir = dir.path().join("transcodes");
        std::fs::create_dir_all(&transcode_dir).unwrap();
        for name in ["recent-a.mp4", "recent-b.mp4"] {
            std::fs::write(transcode_dir.join(name), vec![0u8; 100]).unwrap();
            service
                .record_variant(media_id, "transcode", "h264_aac", name, 100)
                .unwrap();
        }

        let report = service
            .gc_transcode_cache(&transcode_dir, 100, 86_400, 3600)
            .unwrap();
        assert_eq!(report.removed, 0);
        assert_eq!(report.kept, 2);
        assert_eq!(service.cache_stats().unwrap().transcode_count, 2);
    }

    #[test]
    fn cache_gc_refuses_path_traversal_rows() {
        let (dir, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();
        let media_id = insert_media(&service, source.id, "cache-c");
        let transcode_dir = dir.path().join("transcodes");
        std::fs::create_dir_all(&transcode_dir).unwrap();
        let outside = dir.path().join("outside.mp4");
        std::fs::write(&outside, b"keep").unwrap();
        service
            .record_variant(media_id, "transcode", "h264_aac", "../outside.mp4", 4)
            .unwrap();

        let report = service.gc_transcode_cache(&transcode_dir, 0, 0, 0).unwrap();
        assert_eq!(report.removed, 0);
        assert!(outside.is_file());
    }

    #[test]
    fn delete_source_requires_confirmation_and_cleans_artifacts() {
        let (dir, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();
        let media_id = insert_media(&service, source.id, "delete-a");

        let thumbnails = dir.path().join("thumbnails");
        let transcodes = dir.path().join("transcodes");
        std::fs::create_dir_all(&thumbnails).unwrap();
        std::fs::create_dir_all(&transcodes).unwrap();
        std::fs::write(thumbnails.join("thumb.jpg"), b"thumb").unwrap();
        std::fs::write(transcodes.join("movie.mp4"), b"transcode").unwrap();
        service
            .record_variant(media_id, "thumbnail", "default", "thumb.jpg", 5)
            .unwrap();
        service
            .record_variant(media_id, "transcode", "h264_aac", "movie.mp4", 9)
            .unwrap();

        let preview = service
            .delete_source(source.id, false, true, &thumbnails, &transcodes)
            .unwrap();
        assert!(preview.dry_run);
        assert!(preview.archived);
        assert_eq!(preview.media_count, 1);
        assert_eq!(preview.variant_count, 2);
        assert!(service.get_source(source.id).unwrap().is_some());

        let archived = service
            .delete_source(source.id, false, false, &thumbnails, &transcodes)
            .unwrap();
        assert!(archived.archived);
        assert!(service
            .get_source(source.id)
            .unwrap()
            .unwrap()
            .archived_at
            .is_some());
        assert!(service.list_sources().unwrap().is_empty());
        assert_eq!(service.list_sources_with_archived(true).unwrap().len(), 1);
        let media_count: i64 = service
            .db
            .with_read(|connection| {
                Ok(connection.query_row("SELECT COUNT(*) FROM media", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(media_count, 1);

        let restored = service.set_source_archived(source.id, false).unwrap();
        assert!(restored.archived_at.is_none());
        assert!(restored.enabled);

        let report = service
            .delete_source(source.id, true, false, &thumbnails, &transcodes)
            .unwrap();
        assert_eq!(report.media_count, 1);
        assert_eq!(report.deleted_files, 2);
        assert!(service.get_source(source.id).unwrap().is_none());
        assert!(!thumbnails.join("thumb.jpg").is_file());
        assert!(!transcodes.join("movie.mp4").is_file());
        let media_count: i64 = service
            .db
            .with_read(|connection| {
                Ok(connection.query_row("SELECT COUNT(*) FROM media", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(media_count, 0);
    }

    #[test]
    fn remaps_source_root_preserving_media_and_state() {
        let (dir, service) = setup();
        let old_root = dir.path().join("old");
        let new_root = dir.path().join("new");
        std::fs::create_dir_all(&old_root).unwrap();
        std::fs::create_dir_all(&new_root).unwrap();
        std::fs::write(old_root.join("episode.mp4"), b"video").unwrap();
        std::fs::copy(old_root.join("episode.mp4"), new_root.join("episode.mp4")).unwrap();

        let source = service.add_source(old_root.to_str().unwrap()).unwrap();
        let discovery = discover_source(&service.db, &source).unwrap();
        assert_eq!(discovery.added, 1);
        let media = service.list_media(10, 0).unwrap().pop().unwrap();
        service
            .save_state(&MediaState {
                media_id: media.id,
                liked: true,
                favorite: true,
                position: 42.0,
                duration: 100.0,
                updated_at: 0,
            })
            .unwrap();

        let preview = service
            .remap_source(source.id, new_root.to_str().unwrap(), true)
            .unwrap();
        assert_eq!(preview.remapped, 1, "preview={preview:?}");
        assert_eq!(preview.missing, 0);
        assert!(service
            .get_media(media.id)
            .unwrap()
            .unwrap()
            .uri
            .contains("/old/"));

        let report = service
            .remap_source(source.id, new_root.to_str().unwrap(), false)
            .unwrap();
        assert_eq!(report.remapped, 1);
        let media = service.get_media(media.id).unwrap().unwrap();
        assert!(media.uri.contains("/new/"));
        assert!(media.canonical_path.contains("new"));
        let state = service.get_state(media.id).unwrap();
        assert!(state.liked);
        assert!(state.favorite);
        assert_eq!(state.position, 42.0);

        let empty_root = dir.path().join("empty");
        std::fs::create_dir_all(&empty_root).unwrap();
        let missing = service
            .remap_source(source.id, empty_root.to_str().unwrap(), true)
            .unwrap();
        assert_eq!(missing.missing, 1);
        assert_eq!(missing.remapped, 0);
    }

    #[test]
    fn remap_rejects_a_source_uri_owned_by_another_source() {
        let (dir, service) = setup();
        let first = dir.path().join("first");
        let second = dir.path().join("second");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let first = service.add_source(first.to_str().unwrap()).unwrap();
        service.add_source(second.to_str().unwrap()).unwrap();
        assert!(matches!(
            service.remap_source(first.id, second.to_str().unwrap(), true),
            Err(KernelError::Conflict(_))
        ));
    }

    #[test]
    fn archived_sources_are_hidden_from_the_media_page() {
        let (dir, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("a.mp4"), b"x").unwrap();
        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();
        discover_source(&service.db, &source).unwrap();
        assert_eq!(
            service
                .list_media_page(10, 0, None, None, MediaFilter::All, false)
                .unwrap()
                .1,
            1
        );

        service.set_source_archived(source.id, true).unwrap();
        let (items, total) = service
            .list_media_page(10, 0, None, None, MediaFilter::All, false)
            .unwrap();
        assert!(items.is_empty());
        assert_eq!(total, 0);
        let (items, total) = service
            .list_media_page(10, 0, None, None, MediaFilter::All, true)
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(total, 1);
    }

    #[test]
    fn state_defaults_to_zero() {
        let (_dir, service) = setup();
        let state = service.get_state(42).unwrap();
        assert_eq!(state.media_id, 42);
        assert!(!state.liked);
        assert_eq!(state.position, 0.0);
    }
}
