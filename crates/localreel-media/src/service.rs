use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::media_ops::ProbeResult;
use localreel_kernel::storage::MediaUri;
use localreel_kernel::time::now_secs;
use rusqlite::types::Value;
use rusqlite::{params, params_from_iter, OptionalExtension, Row};

use crate::model::{Media, MediaFilter, MediaState, ScanIssue, ScanRun, Source};

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
                    "SELECT id, uri, kind, enabled, created_at, updated_at
                     FROM sources WHERE id = ?1",
                    [id],
                    row_to_source,
                )
                .optional()?)
        })
    }

    pub fn list_sources(&self) -> Result<Vec<Source>> {
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, uri, kind, enabled, created_at, updated_at
                 FROM sources ORDER BY id ASC",
            )?;
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
                "INSERT INTO sources (uri, kind, enabled, created_at, updated_at)
                 VALUES (?1, ?2, 1, ?3, ?3)
                 ON CONFLICT(uri) DO UPDATE SET
                     kind = excluded.kind,
                     updated_at = excluded.updated_at",
                params![uri, kind, now],
            )?;
            let source = connection
                .query_row(
                    "SELECT id, uri, kind, enabled, created_at, updated_at
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
                "SELECT id, uri, kind, enabled, created_at, updated_at
                 FROM sources WHERE id = ?1",
                [id],
                row_to_source,
            )?)
        })
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

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::migrations;

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

    #[test]
    fn state_defaults_to_zero() {
        let (_dir, service) = setup();
        let state = service.get_state(42).unwrap();
        assert_eq!(state.media_id, 42);
        assert!(!state.liked);
        assert_eq!(state.position, 0.0);
    }
}
