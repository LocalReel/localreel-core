use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::storage::{LocalStorage, MediaUri, Storage, StorageRouter};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use uuid::Uuid;

use crate::model::Source;

/// Bump when the series parser's output changes so existing rows are recomputed
/// on the next scan (see `media.series_rev`).
const SERIES_REV: i64 = 1;

/// Result of one discovery pass (no probe/thumbnail yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScanDiscovery {
    pub run_id: i64,
    pub source_id: i64,
    pub status: String,
    pub discovered: usize,
    pub added: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub missing: usize,
    pub failed: usize,
}

const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "3gp", "ts", "mts", "m2ts",
];

/// Walk one local source and update `media` rows.
///
/// This is the "discovery" half of a scan: it fingerprints files and upserts
/// rows, but it does not run ffprobe or generate thumbnails. Those are separate
/// jobs (`probe_media`, `thumbnail`) in the next phase.
///
/// Missing files are marked with `missing_at`; rows and user state are never
/// deleted by a scan.
pub fn discover_source(db: &Database, source: &Source) -> Result<ScanDiscovery> {
    let storage = storage_for_source(source)?;
    discover_source_with_storage(db, source, &storage, &AtomicBool::new(false))
}

/// Same as [`discover_source`], but stops walking/upserting when `cancel`
/// becomes true. A canceled run is recorded as such and does not mark missing
/// rows (a partial walk would otherwise hide media).
pub fn discover_source_with_cancel(
    db: &Database,
    source: &Source,
    cancel: &AtomicBool,
) -> Result<ScanDiscovery> {
    let storage = storage_for_source(source)?;
    discover_source_with_storage(db, source, &storage, cancel)
}

/// The provider-based entry point. The media layer only sees [`Storage`], so a
/// future WebDAV/SMB source can be scanned by passing its provider here.
pub fn discover_source_with_storage(
    db: &Database,
    source: &Source,
    storage: &dyn Storage,
    cancel: &AtomicBool,
) -> Result<ScanDiscovery> {
    let root = MediaUri::parse(&source.uri)?;
    let root_stat = match storage.stat(&root) {
        Ok(stat) => stat,
        Err(error) => {
            let message = format!("source root is unavailable: {error}");
            let run_id = start_run(db, source)?;
            finish_run(
                db,
                run_id,
                "failed",
                ScanCounts {
                    failed: 1,
                    ..ScanCounts::default()
                },
                &[(source.uri.clone(), message)],
            )?;
            return Ok(ScanDiscovery {
                run_id,
                source_id: source.id,
                status: "failed".to_string(),
                ..ScanDiscovery::empty(source.id, run_id)
            });
        }
    };
    if !root_stat.is_dir {
        let message = "source root is not a directory".to_string();
        let run_id = start_run(db, source)?;
        finish_run(
            db,
            run_id,
            "failed",
            ScanCounts {
                failed: 1,
                ..ScanCounts::default()
            },
            &[(source.uri.clone(), message)],
        )?;
        return Ok(ScanDiscovery {
            run_id,
            source_id: source.id,
            status: "failed".to_string(),
            ..ScanDiscovery::empty(source.id, run_id)
        });
    }

    let run_id = start_run(db, source)?;

    let existing = load_existing(db, source.id)?;
    let mut seen: HashSet<String> = HashSet::new();
    let mut candidates = Vec::new();
    let mut issues: Vec<(String, String)> = Vec::new();
    let mut traversal_incomplete = false;

    let walker = match storage.walk(&root) {
        Ok(walker) => walker,
        Err(error) => {
            finish_run(
                db,
                run_id,
                "failed",
                ScanCounts {
                    failed: 1,
                    ..ScanCounts::default()
                },
                &[(
                    source.uri.clone(),
                    format!("cannot walk source root: {error}"),
                )],
            )?;
            return Ok(ScanDiscovery {
                run_id,
                source_id: source.id,
                status: "failed".to_string(),
                ..ScanDiscovery::empty(source.id, run_id)
            });
        }
    };

    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            let counts = ScanCounts {
                discovered: candidates.len(),
                failed: issues.len(),
                ..ScanCounts::default()
            };
            finish_run(db, run_id, "canceled", counts, &issues)?;
            return Ok(ScanDiscovery {
                run_id,
                source_id: source.id,
                status: "canceled".to_string(),
                discovered: counts.discovered,
                added: 0,
                updated: 0,
                unchanged: 0,
                missing: 0,
                failed: counts.failed,
            });
        }

        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                traversal_incomplete = true;
                issues.push((
                    source.uri.clone(),
                    format!("failed to read directory entry: {error}"),
                ));
                continue;
            }
        };
        if entry.is_dir {
            continue;
        }
        let path = match MediaUri::parse(&entry.uri) {
            Ok(MediaUri::Local(path)) => path,
            _ => {
                issues.push((
                    entry.uri.clone(),
                    "storage returned an entry without a local path".to_string(),
                ));
                continue;
            }
        };
        if !is_video_path(&path) {
            continue;
        }

        let filename = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.name.clone());
        seen.insert(entry.uri.clone());
        candidates.push(Candidate {
            uri: entry.uri,
            canonical_path: path.to_string_lossy().into_owned(),
            filename,
            file_size: i64::try_from(entry.size).unwrap_or(i64::MAX),
            modified_at: entry.modified.unwrap_or(0),
        });
    }

    if cancel.load(Ordering::Relaxed) {
        let counts = ScanCounts {
            discovered: candidates.len(),
            failed: issues.len(),
            ..ScanCounts::default()
        };
        finish_run(db, run_id, "canceled", counts, &issues)?;
        return Ok(ScanDiscovery {
            run_id,
            source_id: source.id,
            status: "canceled".to_string(),
            discovered: counts.discovered,
            added: 0,
            updated: 0,
            unchanged: 0,
            missing: 0,
            failed: counts.failed,
        });
    }

    let mut counts = ScanCounts {
        discovered: candidates.len(),
        ..ScanCounts::default()
    };

    let mut added = 0usize;
    let mut updated = 0usize;
    let mut unchanged = 0usize;

    db.with_write(|connection| {
        let transaction = connection.transaction()?;
        let now = now_secs();

        for candidate in &candidates {
            let owner: Option<i64> = transaction
                .query_row(
                    "SELECT source_id FROM media WHERE uri = ?1",
                    [&candidate.uri],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(owner_id) = owner {
                if owner_id != source.id {
                    issues.push((
                        candidate.uri.clone(),
                        format!("already indexed by source {owner_id}"),
                    ));
                    continue;
                }
            }

            let series = crate::series::derive(&candidate.canonical_path, &candidate.filename);
            let series_rev = SERIES_REV;
            match existing.get(&candidate.uri) {
                Some(existing) => {
                    let changed = existing.file_size != candidate.file_size
                        || existing.modified_at != candidate.modified_at;
                    if changed {
                        transaction.execute(
                            "UPDATE media SET
                                 canonical_path = ?1, filename = ?2,
                                 file_size = ?3, modified_at = ?4,
                                 series_key = ?5, series_title = ?6,
                                 episode = ?7, is_extra = ?8, series_rev = ?9,
                                 thumbnail_path = NULL, missing_at = NULL,
                                 updated_at = ?10
                             WHERE id = ?11",
                            params![
                                candidate.canonical_path,
                                candidate.filename,
                                candidate.file_size,
                                candidate.modified_at,
                                series.key,
                                series.title,
                                series.episode,
                                series.is_extra as i64,
                                series_rev,
                                now,
                                existing.id
                            ],
                        )?;
                        updated += 1;
                    } else {
                        transaction.execute(
                            "UPDATE media SET missing_at = NULL WHERE id = ?1 AND missing_at IS NOT NULL",
                            [existing.id],
                        )?;
                        // Backfill rows created before the series migration.
                        transaction.execute(
                            "UPDATE media SET series_key = ?2, series_title = ?3,
                                 episode = ?4, is_extra = ?5, series_rev = ?6
                             WHERE id = ?1 AND series_rev < ?6",
                            params![
                                existing.id,
                                series.key,
                                series.title,
                                series.episode,
                                series.is_extra as i64,
                                series_rev
                            ],
                        )?;
                        unchanged += 1;
                    }
                }
                None => {
                    let uid = Uuid::new_v4().to_string();
                    transaction.execute(
                        "INSERT INTO media (
                             uid, source_id, uri, canonical_path, filename, title,
                             series_key, series_title, episode, is_extra, series_rev,
                             file_size, modified_at, created_at, updated_at
                         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?14)",
                        params![
                            uid,
                            source.id,
                            candidate.uri,
                            candidate.canonical_path,
                            candidate.filename,
                            derive_title(&candidate.filename),
                            series.key,
                            series.title,
                            series.episode,
                            series.is_extra as i64,
                            series_rev,
                            candidate.file_size,
                            candidate.modified_at,
                            now
                        ],
                    )?;
                    added += 1;
                }
            }
        }

        // Missing rows are marked, never deleted. Skip when the walk was
        // incomplete so a permission error cannot hide a whole subtree.
        let mut missing = 0usize;
        if !traversal_incomplete {
            for (uri, existing) in &existing {
                if seen.contains(uri) {
                    continue;
                }
                let changed = transaction.execute(
                    "UPDATE media SET missing_at = ?1, updated_at = ?1
                     WHERE id = ?2 AND missing_at IS NULL",
                    params![now, existing.id],
                )?;
                missing += changed;
            }
        }

        counts.added = added;
        counts.updated = updated;
        counts.unchanged = unchanged;
        counts.removed = missing;
        counts.failed = issues.len();

        transaction.commit()?;
        Ok(())
    })?;

    let status = if !issues.is_empty() {
        "completed_with_errors"
    } else {
        "completed"
    };
    finish_run(db, run_id, status, counts, &issues)?;

    Ok(ScanDiscovery {
        run_id,
        source_id: source.id,
        status: status.to_string(),
        discovered: counts.discovered,
        added,
        updated,
        unchanged,
        missing: counts.removed,
        failed: counts.failed,
    })
}

impl ScanDiscovery {
    fn empty(source_id: i64, run_id: i64) -> Self {
        Self {
            run_id,
            source_id,
            status: "failed".to_string(),
            discovered: 0,
            added: 0,
            updated: 0,
            unchanged: 0,
            missing: 0,
            failed: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ScanCounts {
    discovered: usize,
    added: usize,
    updated: usize,
    unchanged: usize,
    removed: usize,
    failed: usize,
}

struct ExistingMedia {
    id: i64,
    file_size: i64,
    modified_at: i64,
}

struct Candidate {
    uri: String,
    canonical_path: String,
    filename: String,
    file_size: i64,
    modified_at: i64,
}

fn start_run(db: &Database, source: &Source) -> Result<i64> {
    db.with_write(|connection| {
        connection.execute(
            "INSERT INTO scan_runs (source_id, source_uri, started_at, status)
             VALUES (?1, ?2, ?3, 'running')",
            params![source.id, source.uri, now_secs()],
        )?;
        Ok(connection.last_insert_rowid())
    })
}

/// Build the storage provider for a source.
///
/// Local paths are the only registered provider today; a remote source is
/// rejected here with a clear message until its provider is added to
/// [`StorageRouter`].
pub fn storage_for_source(source: &Source) -> Result<StorageRouter> {
    match MediaUri::parse(&source.uri)? {
        MediaUri::Local(path) => Ok(StorageRouter::new(LocalStorage::new([path])?)),
        other => Err(KernelError::Storage(format!(
            "no storage provider registered for {} sources",
            other.kind()
        ))),
    }
}

fn load_existing(db: &Database, source_id: i64) -> Result<HashMap<String, ExistingMedia>> {
    db.with_read(|connection| {
        let mut statement = connection
            .prepare("SELECT uri, id, file_size, modified_at FROM media WHERE source_id = ?1")?;
        let rows = statement.query_map([source_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                ExistingMedia {
                    id: row.get(1)?,
                    file_size: row.get(2)?,
                    modified_at: row.get(3)?,
                },
            ))
        })?;
        let mut existing = HashMap::new();
        for row in rows {
            let (uri, media) = row?;
            existing.insert(uri, media);
        }
        Ok(existing)
    })
}

fn finish_run(
    db: &Database,
    run_id: i64,
    status: &str,
    counts: ScanCounts,
    issues: &[(String, String)],
) -> Result<()> {
    db.with_write(|connection| {
        let transaction = connection.transaction()?;
        for (path, message) in issues {
            transaction.execute(
                "INSERT INTO scan_issues (run_id, source_id, path, message, created_at)
                 VALUES (?1, COALESCE((SELECT source_id FROM scan_runs WHERE id = ?1), 0), ?2, ?3, ?4)",
                params![run_id, path, message, now_secs()],
            )?;
        }
        transaction.execute(
            "UPDATE scan_runs SET
                 finished_at = ?1, status = ?2, discovered = ?3, added = ?4,
                 updated = ?5, unchanged = ?6, removed = ?7, failed = ?8
             WHERE id = ?9",
            params![
                now_secs(),
                status,
                counts.discovered as i64,
                counts.added as i64,
                counts.updated as i64,
                counts.unchanged as i64,
                counts.removed as i64,
                counts.failed as i64,
                run_id
            ],
        )?;
        transaction.commit()?;
        Ok(())
    })
}

fn is_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| VIDEO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn derive_title(filename: &str) -> Option<String> {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|value| value.to_str())?
        .replace(['_', '-'], " ");
    let trimmed = stem.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations;
    use crate::service::MediaService;
    use localreel_kernel::migrations as kernel_migrations;

    fn setup() -> (tempfile::TempDir, Database, MediaService) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("scan.db");
        let db = Database::open(db_path.to_str().unwrap(), 1).unwrap();
        kernel_migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        kernel_migrations::apply(&db, "media", migrations::list()).unwrap();
        (dir, db.clone(), MediaService::new(db))
    }

    /// A provider that serves a fixed list of entries. The files never exist on
    /// disk, which proves discovery goes through `Storage` only.
    struct MockStorage {
        entries: Vec<localreel_kernel::storage::StorageEntry>,
    }

    impl Storage for MockStorage {
        fn stat(&self, _uri: &MediaUri) -> Result<localreel_kernel::storage::FileStat> {
            Ok(localreel_kernel::storage::FileStat {
                size: 0,
                modified: Some(0),
                is_dir: true,
            })
        }

        fn list(&self, _uri: &MediaUri) -> Result<Vec<localreel_kernel::storage::StorageEntry>> {
            Ok(self.entries.clone())
        }

        fn open_range(
            &self,
            _uri: &MediaUri,
            _start: u64,
            _len: u64,
        ) -> Result<Box<dyn std::io::Read + Send>> {
            Ok(Box::new(std::io::empty()))
        }

        fn walk(
            &self,
            _root: &MediaUri,
        ) -> Result<Box<dyn Iterator<Item = Result<localreel_kernel::storage::StorageEntry>> + Send>>
        {
            let entries = self.entries.clone();
            Ok(Box::new(entries.into_iter().map(Ok)))
        }
    }

    #[test]
    fn scans_through_the_storage_provider_without_files_on_disk() {
        let (dir, db, service) = setup();
        let root = dir.path().join("library");
        std::fs::create_dir_all(&root).unwrap();
        let source = service.add_source(root.to_str().unwrap()).unwrap();
        let entry =
            |name: &str, size: u64, modified: i64| localreel_kernel::storage::StorageEntry {
                uri: MediaUri::Local(root.join(name)).to_uri(),
                name: name.to_string(),
                is_dir: false,
                size,
                modified: Some(modified),
            };
        let mock = MockStorage {
            entries: vec![
                entry("episode-01.mp4", 11, 100),
                entry("episode-02.mkv", 22, 200),
                entry("notes.txt", 3, 300),
            ],
        };

        let discovery =
            discover_source_with_storage(&db, &source, &mock, &AtomicBool::new(false)).unwrap();
        assert_eq!(discovery.discovered, 2);
        assert_eq!(discovery.added, 2);
        let names: Vec<String> = db
            .with_read(|conn| {
                let mut statement = conn.prepare("SELECT filename FROM media ORDER BY filename")?;
                let rows = statement.query_map([], |row| row.get(0))?;
                Ok(rows.collect::<rusqlite::Result<Vec<String>>>()?)
            })
            .unwrap();
        assert_eq!(names, vec!["episode-01.mp4", "episode-02.mkv"]);
    }

    #[test]
    fn discovers_files_and_marks_missing_without_deleting_state() {
        let (dir, db, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("a.mp4"), b"a").unwrap();
        std::fs::write(source_dir.join("b.mkv"), b"b").unwrap();
        std::fs::write(source_dir.join("notes.txt"), b"not media").unwrap();

        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();
        let first = discover_source(&db, &source).unwrap();
        assert_eq!(first.discovered, 2);
        assert_eq!(first.added, 2);
        assert_eq!(first.unchanged, 0);
        assert_eq!(first.missing, 0);
        assert_eq!(first.status, "completed");

        let second = discover_source(&db, &source).unwrap();
        assert_eq!(second.unchanged, 2);
        assert_eq!(second.added, 0);

        // Add state, remove the file, rescan: row and state must survive.
        let media_id: i64 = db
            .with_read(|conn| {
                Ok(
                    conn.query_row("SELECT id FROM media WHERE filename = 'a.mp4'", [], |row| {
                        row.get(0)
                    })?,
                )
            })
            .unwrap();
        let mut state = service.get_state(media_id).unwrap();
        state.favorite = true;
        state.position = 12.0;
        service.save_state(&state).unwrap();

        std::fs::remove_file(source_dir.join("a.mp4")).unwrap();
        let third = discover_source(&db, &source).unwrap();
        assert_eq!(third.missing, 1);

        let (missing_at, favorite): (Option<i64>, i64) = db
            .with_read(|conn| {
                Ok(conn.query_row(
                    "SELECT m.missing_at, s.favorite
                     FROM media m JOIN media_state s ON s.media_id = m.id
                     WHERE m.filename = 'a.mp4'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?)
            })
            .unwrap();
        assert!(missing_at.is_some());
        assert_eq!(favorite, 1);
    }

    #[test]
    fn canceled_scan_is_recorded_without_upserting() {
        let (dir, db, service) = setup();
        let source_dir = dir.path().join("videos");
        std::fs::create_dir_all(&source_dir).unwrap();
        std::fs::write(source_dir.join("a.mp4"), b"a").unwrap();
        let source = service.add_source(source_dir.to_str().unwrap()).unwrap();

        let cancel = AtomicBool::new(true);
        let result = discover_source_with_cancel(&db, &source, &cancel).unwrap();
        assert_eq!(result.status, "canceled");
        assert_eq!(result.discovered, 0);
        let media_count: i64 = db
            .with_read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM media", [], |r| r.get(0))?))
            .unwrap();
        assert_eq!(media_count, 0);
        let run_status: String = db
            .with_read(|conn| {
                Ok(conn.query_row(
                    "SELECT status FROM scan_runs ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(run_status, "canceled");
    }

    #[test]
    fn hls_sources_are_deferred_not_mis_scanned() {
        let (dir, db, _service) = setup();
        let source = Source {
            id: 1,
            uri: "https://example.com/live/index.m3u8".into(),
            kind: "hls".into(),
            enabled: true,
            created_at: 0,
            updated_at: 0,
            archived_at: None,
        };
        assert!(discover_source(&db, &source).is_err());
        let _ = dir;
    }
}
