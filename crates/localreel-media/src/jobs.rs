use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::jobs::{JobContext, JobHandler, JobOutcome, JobRegistry};
use localreel_kernel::media_ops::{MediaOps, TranscodePreset};
use localreel_kernel::storage::MediaUri;
use serde::{Deserialize, Serialize};

use crate::backup::{read_backup_with_limit, Backup, MAX_BACKUP_RESTORE_BYTES};
use crate::playback::DEFAULT_TRANSCODE_PRESET;
use crate::restore::restore_backup;
use crate::scan::discover_source_with_cancel;
use crate::service::MediaService;

/// Media-domain job handlers registered with the kernel worker.
///
/// The scan handler discovers files and enqueues probe jobs; probe updates
/// metadata and enqueues a thumbnail job; thumbnail writes the artifact. None
/// of these run on the async executor directly — the worker uses
/// `spawn_blocking`.
#[derive(Clone)]
pub struct MediaJobs {
    shared: Arc<Shared>,
}

struct Shared {
    db: Database,
    media_ops: Arc<MediaOps>,
    thumbnail_dir: PathBuf,
    transcode_dir: PathBuf,
}

impl MediaJobs {
    pub fn new(
        db: Database,
        media_ops: Arc<MediaOps>,
        thumbnail_dir: PathBuf,
        transcode_dir: PathBuf,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                db,
                media_ops,
                thumbnail_dir,
                transcode_dir,
            }),
        }
    }

    pub fn register(&self, registry: &mut JobRegistry) {
        registry.register(ScanSourceHandler(self.shared.clone()));
        registry.register(ProbeMediaHandler(self.shared.clone()));
        registry.register(ThumbnailHandler(self.shared.clone()));
        registry.register(TranscodeHandler(self.shared.clone()));
        registry.register(CacheGcHandler(self.shared.clone()));
        registry.register(RestoreBackupHandler(self.shared.clone()));
        registry.register(RestoreBackupFileHandler(self.shared.clone()));
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ScanPayload {
    source_id: i64,
}

struct ScanSourceHandler(Arc<Shared>);

impl JobHandler for ScanSourceHandler {
    fn kind(&self) -> &'static str {
        "scan_source"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: ScanPayload = context.payload()?;
        let service = MediaService::new(self.0.db.clone());
        let source = service
            .get_source(payload.source_id)?
            .ok_or_else(|| KernelError::NotFound(format!("source {}", payload.source_id)))?;
        if source.archived_at.is_some() {
            return Err(KernelError::InvalidInput(format!(
                "source {} is archived",
                source.id
            )));
        }
        if !source.enabled {
            return Err(KernelError::InvalidInput(format!(
                "source {} is disabled",
                source.id
            )));
        }

        context.report_progress(0.05, Some("discovering"));
        let discovery = discover_source_with_cancel(&self.0.db, &source, &context.cancel_flag())?;
        if discovery.status == "canceled" {
            return JobOutcome::json(discovery);
        }
        context.report_progress(0.6, Some("enqueueing metadata jobs"));

        for media_id in service.probe_candidate_ids(source.id)? {
            let payload = serde_json::json!({ "media_id": media_id }).to_string();
            if let Err(error) = context.queue().enqueue(
                "probe_media",
                &payload,
                Some(&format!("probe:{media_id}")),
                3,
            ) {
                tracing::warn!(media_id, "failed to enqueue probe job: {error}");
            }
        }

        context.report_progress(1.0, Some("scan complete"));
        JobOutcome::json(discovery)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ProbePayload {
    media_id: i64,
}

struct ProbeMediaHandler(Arc<Shared>);

impl JobHandler for ProbeMediaHandler {
    fn kind(&self) -> &'static str {
        "probe_media"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: ProbePayload = context.payload()?;
        let service = MediaService::new(self.0.db.clone());
        let media = service
            .get_media(payload.media_id)?
            .ok_or_else(|| KernelError::NotFound(format!("media {}", payload.media_id)))?;

        let path = MediaUri::parse(&media.uri)?
            .local_path()
            .map(PathBuf::from)
            .ok_or_else(|| {
                KernelError::InvalidInput(format!(
                    "media {} is not local; remote probing is not supported",
                    media.id
                ))
            })?;
        if !path.is_file() {
            return Err(KernelError::NotFound(format!(
                "media file is missing: {}",
                path.display()
            )));
        }

        context.report_progress(0.1, Some("ffprobe"));
        let probe = self.0.media_ops.probe(&path, &context.cancel_flag())?;
        service.update_probe(media.id, &probe)?;
        context.report_progress(0.9, Some("metadata updated"));

        if media.thumbnail_path.is_none() {
            let payload = serde_json::json!({ "media_id": media.id }).to_string();
            if let Err(error) = context.queue().enqueue(
                "thumbnail",
                &payload,
                Some(&format!("thumbnail:{}", media.id)),
                3,
            ) {
                tracing::warn!(
                    media_id = media.id,
                    "failed to enqueue thumbnail job: {error}"
                );
            }
        }

        JobOutcome::json(probe)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ThumbnailPayload {
    media_id: i64,
}

struct ThumbnailHandler(Arc<Shared>);

impl JobHandler for ThumbnailHandler {
    fn kind(&self) -> &'static str {
        "thumbnail"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: ThumbnailPayload = context.payload()?;
        let service = MediaService::new(self.0.db.clone());
        let media = service
            .get_media(payload.media_id)?
            .ok_or_else(|| KernelError::NotFound(format!("media {}", payload.media_id)))?;

        let path = MediaUri::parse(&media.uri)?
            .local_path()
            .map(PathBuf::from)
            .ok_or_else(|| {
                KernelError::InvalidInput(format!(
                    "media {} is not local; remote thumbnails are not supported",
                    media.id
                ))
            })?;
        if !path.is_file() {
            return Err(KernelError::NotFound(format!(
                "media file is missing: {}",
                path.display()
            )));
        }

        let filename = format!("thumb_{}.jpg", media.uid);
        let output = self.0.thumbnail_dir.join(&filename);
        let seek = media
            .duration
            .map(|duration| (duration * 0.1).clamp(1.0, 10.0))
            .unwrap_or(3.0);

        context.report_progress(0.1, Some("ffmpeg thumbnail"));
        self.0
            .media_ops
            .thumbnail(&path, &output, seek, 480, &context.cancel_flag())?;

        let size = std::fs::metadata(&output)
            .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        service.set_thumbnail(media.id, &filename)?;
        service.record_variant(media.id, "thumbnail", "default", &filename, size)?;
        context.report_progress(1.0, Some("thumbnail ready"));
        Ok(JobOutcome::empty())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TranscodePayload {
    media_id: i64,
    #[serde(default)]
    preset: Option<String>,
}

struct TranscodeHandler(Arc<Shared>);

impl JobHandler for TranscodeHandler {
    fn kind(&self) -> &'static str {
        "transcode"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: TranscodePayload = context.payload()?;
        let preset_name = payload
            .preset
            .unwrap_or_else(|| DEFAULT_TRANSCODE_PRESET.to_string());
        if preset_name != DEFAULT_TRANSCODE_PRESET {
            return Err(KernelError::InvalidInput(format!(
                "unsupported transcode preset '{preset_name}'"
            )));
        }

        let service = MediaService::new(self.0.db.clone());
        let media = service
            .get_media(payload.media_id)?
            .ok_or_else(|| KernelError::NotFound(format!("media {}", payload.media_id)))?;
        let input = MediaUri::parse(&media.uri)?
            .local_path()
            .map(PathBuf::from)
            .ok_or_else(|| {
                KernelError::InvalidInput(format!(
                    "media {} is not local; remote transcoding is not supported",
                    media.id
                ))
            })?;
        if !input.is_file() {
            return Err(KernelError::NotFound(format!(
                "media file is missing: {}",
                input.display()
            )));
        }

        let filename = format!(
            "transcode_{}_{}_{}_{}.mp4",
            sanitize_component(&media.uid, 64),
            sanitize_component(&preset_name, 32),
            media.file_size,
            media.modified_at
        );
        let output = self.0.transcode_dir.join(&filename);
        let preset = TranscodePreset::default();
        let mut last_progress = 0.0f64;
        let mut last_report = Instant::now() - Duration::from_secs(2);

        context.report_progress(0.0, Some("transcoding"));
        self.0.media_ops.transcode(
            &input,
            &output,
            &preset,
            media.duration,
            &context.cancel_flag(),
            |progress| {
                if progress - last_progress >= 0.02
                    || last_report.elapsed() >= Duration::from_secs(1)
                {
                    context.report_progress(progress, Some("transcoding"));
                    last_progress = progress;
                    last_report = Instant::now();
                }
            },
        )?;

        let size = std::fs::metadata(&output)
            .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        service.record_variant(media.id, "transcode", &preset_name, &filename, size)?;
        context.report_progress(1.0, Some("transcode ready"));
        JobOutcome::json(serde_json::json!({
            "path": filename,
            "preset": preset_name,
            "size": size,
        }))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheGcPayload {
    #[serde(default)]
    max_bytes: Option<u64>,
    #[serde(default)]
    max_age_secs: Option<i64>,
    #[serde(default)]
    protect_recent_secs: Option<i64>,
}

struct CacheGcHandler(Arc<Shared>);

impl JobHandler for CacheGcHandler {
    fn kind(&self) -> &'static str {
        "cache_gc"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: CacheGcPayload = context.payload()?;
        let max_bytes = payload.max_bytes.unwrap_or(20 * 1024 * 1024 * 1024);
        let max_age_secs = payload.max_age_secs.unwrap_or(30 * 24 * 60 * 60);
        let protect_recent_secs = payload.protect_recent_secs.unwrap_or(3600);

        context.report_progress(0.1, Some("scanning transcode cache"));
        let report = MediaService::new(self.0.db.clone()).gc_transcode_cache(
            &self.0.transcode_dir,
            max_bytes,
            max_age_secs,
            protect_recent_secs,
        )?;
        context.report_progress(1.0, Some("cache GC complete"));
        JobOutcome::json(report)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RestorePayload {
    backup: Backup,
}

struct RestoreBackupHandler(Arc<Shared>);

impl JobHandler for RestoreBackupHandler {
    fn kind(&self) -> &'static str {
        "restore_backup"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: RestorePayload = context.payload()?;
        let cancel = context.cancel_flag();
        let mut progress = |value: f32, message: &str| {
            context.report_progress(f64::from(value), Some(message));
        };
        let report = restore_backup(
            &self.0.db,
            &payload.backup,
            &self.0.media_ops,
            &self.0.thumbnail_dir,
            false,
            &cancel,
            &mut progress,
        )?;
        JobOutcome::json(report)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct RestoreFilePayload {
    path: String,
}

struct RestoreBackupFileHandler(Arc<Shared>);

impl JobHandler for RestoreBackupFileHandler {
    fn kind(&self) -> &'static str {
        "restore_backup_file"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: RestoreFilePayload = context.payload()?;
        let path = PathBuf::from(&payload.path);
        let result = (|| {
            let backup = read_backup_with_limit(&path, MAX_BACKUP_RESTORE_BYTES)?;
            let cancel = context.cancel_flag();
            let mut progress = |value: f32, message: &str| {
                context.report_progress(f64::from(value), Some(message));
            };
            let report = restore_backup(
                &self.0.db,
                &backup,
                &self.0.media_ops,
                &self.0.thumbnail_dir,
                false,
                &cancel,
                &mut progress,
            )?;
            JobOutcome::json(report)
        })();
        // The uploaded JSON is a one-shot job input; always clean it up.
        let _ = std::fs::remove_file(&path);
        result
    }
}

fn sanitize_component(value: &str, max_len: usize) -> String {
    let mut output = String::with_capacity(value.len().min(max_len));
    for ch in value.chars() {
        if output.len() >= max_len {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    if output.is_empty() {
        output.push('_');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use localreel_kernel::events::EventBus;
    use localreel_kernel::jobs::{JobQueue, JobWorker};
    use localreel_kernel::migrations as kernel_migrations;

    use crate::migrations;

    #[cfg(windows)]
    const FAKE_FFPROBE: &str = "@echo off\r\nif \"%~1\"==\"-version\" exit /b 0\r\necho {\"format\":{\"duration\":\"2.0\",\"bit_rate\":\"123\"},\"streams\":[{\"codec_type\":\"video\",\"codec_name\":\"h264\",\"width\":320,\"height\":240},{\"codec_type\":\"audio\",\"codec_name\":\"aac\"}]}\r\nexit /b 0\r\n";

    #[cfg(windows)]
    const FAKE_FFMPEG: &str = "@echo off\r\nif \"%~1\"==\"-version\" exit /b 0\r\nsetlocal enabledelayedexpansion\r\nset \"out=\"\r\n:loop\r\nif \"%~1\"==\"\" goto done\r\nset \"out=%~1\"\r\nshift\r\ngoto loop\r\n:done\r\nif defined out echo fake>\"%out%\"\r\nexit /b 0\r\n";

    #[cfg(windows)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_probe_thumbnail_chain_with_fake_binaries() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("jobs.db").to_str().unwrap(), 1).unwrap();
        kernel_migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        kernel_migrations::apply(&db, "media", migrations::list()).unwrap();

        let media_dir = dir.path().join("videos");
        std::fs::create_dir_all(&media_dir).unwrap();
        std::fs::write(media_dir.join("a.mp4"), b"not a real video").unwrap();
        let service = MediaService::new(db.clone());
        let source = service.add_source(media_dir.to_str().unwrap()).unwrap();

        let ffprobe = dir.path().join("ffprobe.cmd");
        let ffmpeg = dir.path().join("ffmpeg.cmd");
        std::fs::write(&ffprobe, FAKE_FFPROBE).unwrap();
        std::fs::write(&ffmpeg, FAKE_FFMPEG).unwrap();
        let ops = Arc::new(MediaOps::new(
            ffmpeg,
            ffprobe,
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(30),
        ));
        let thumbnails = dir.path().join("thumbnails");
        let transcodes = dir.path().join("transcodes");

        let events = EventBus::new(64);
        let queue = JobQueue::new(db.clone(), events);
        let mut registry = JobRegistry::new();
        MediaJobs::new(db.clone(), ops, thumbnails.clone(), transcodes).register(&mut registry);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = JobWorker::new(
            queue.clone(),
            Arc::new(registry),
            "fake-worker",
            Duration::from_secs(10),
            Duration::from_millis(20),
        )
        .spawn_with_shutdown(shutdown.clone());

        queue
            .enqueue(
                "scan_source",
                &serde_json::json!({ "source_id": source.id }).to_string(),
                Some(&format!("scan:{}", source.id)),
                1,
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let jobs = queue.list_recent(50).unwrap();
            let active = jobs
                .iter()
                .any(|job| job.status == "queued" || job.status == "running");
            let kinds = |kind: &str| {
                jobs.iter()
                    .any(|job| job.kind == kind && job.status == "succeeded")
            };
            if !active && kinds("scan_source") && kinds("probe_media") && kinds("thumbnail") {
                break;
            }
            if Instant::now() > deadline {
                panic!("job chain did not finish: {jobs:#?}");
            }
        }

        shutdown.store(true, Ordering::SeqCst);
        let _ = worker.await;

        let (duration, width, height, video, audio, thumbnail): (
            f64,
            i64,
            i64,
            String,
            String,
            String,
        ) = db
            .with_read(|connection| {
                Ok(connection.query_row(
                    "SELECT duration, width, height, video_codec, audio_codec, thumbnail_path
                     FROM media LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )?)
            })
            .unwrap();
        assert_eq!(duration, 2.0);
        assert_eq!(width, 320);
        assert_eq!(height, 240);
        assert_eq!(video, "h264");
        assert_eq!(audio, "aac");
        assert!(thumbnails.join(&thumbnail).is_file());
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn scan_probe_thumbnail_chain_with_fake_shell_binaries() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("jobs.db").to_str().unwrap(), 1).unwrap();
        kernel_migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        kernel_migrations::apply(&db, "media", migrations::list()).unwrap();

        let media_dir = dir.path().join("videos");
        std::fs::create_dir_all(&media_dir).unwrap();
        std::fs::write(media_dir.join("a.mp4"), b"not a real video").unwrap();
        let service = MediaService::new(db.clone());
        let source = service.add_source(media_dir.to_str().unwrap()).unwrap();

        let ffprobe = dir.path().join("ffprobe.sh");
        let ffmpeg = dir.path().join("ffmpeg.sh");
        std::fs::write(
            &ffprobe,
            r#"#!/bin/sh
if [ "$1" = "-version" ]; then exit 0; fi
cat <<'JSON'
{"format":{"duration":"2.0","bit_rate":"123"},"streams":[{"codec_type":"video","codec_name":"h264","width":320,"height":240},{"codec_type":"audio","codec_name":"aac"}]}
JSON
"#,
        )
        .unwrap();
        std::fs::write(
            &ffmpeg,
            r#"#!/bin/sh
if [ "$1" = "-version" ]; then exit 0; fi
last=""
for arg in "$@"; do last="$arg"; done
if [ -n "$last" ]; then printf fake > "$last"; fi
exit 0
"#,
        )
        .unwrap();
        for script in [&ffprobe, &ffmpeg] {
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let ops = Arc::new(MediaOps::new(
            ffmpeg,
            ffprobe,
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(30),
        ));
        let thumbnails = dir.path().join("thumbnails");
        let transcodes = dir.path().join("transcodes");

        let events = EventBus::new(64);
        let queue = JobQueue::new(db.clone(), events);
        let mut registry = JobRegistry::new();
        MediaJobs::new(db.clone(), ops, thumbnails.clone(), transcodes).register(&mut registry);
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker = JobWorker::new(
            queue.clone(),
            Arc::new(registry),
            "fake-shell-worker",
            Duration::from_secs(10),
            Duration::from_millis(20),
        )
        .spawn_with_shutdown(shutdown.clone());

        queue
            .enqueue(
                "scan_source",
                &serde_json::json!({ "source_id": source.id }).to_string(),
                Some(&format!("scan:{}", source.id)),
                1,
            )
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            tokio::time::sleep(Duration::from_millis(100)).await;
            let jobs = queue.list_recent(50).unwrap();
            let active = jobs
                .iter()
                .any(|job| job.status == "queued" || job.status == "running");
            let kinds = |kind: &str| {
                jobs.iter()
                    .any(|job| job.kind == kind && job.status == "succeeded")
            };
            if !active && kinds("scan_source") && kinds("probe_media") && kinds("thumbnail") {
                break;
            }
            if Instant::now() > deadline {
                panic!("job chain did not finish: {jobs:#?}");
            }
        }

        shutdown.store(true, Ordering::SeqCst);
        let _ = worker.await;

        let (duration, width, height, video, audio, thumbnail): (
            f64,
            i64,
            i64,
            String,
            String,
            String,
        ) = db
            .with_read(|connection| {
                Ok(connection.query_row(
                    "SELECT duration, width, height, video_codec, audio_codec, thumbnail_path
                     FROM media LIMIT 1",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )?)
            })
            .unwrap();
        assert_eq!(duration, 2.0);
        assert_eq!(width, 320);
        assert_eq!(height, 240);
        assert_eq!(video, "h264");
        assert_eq!(audio, "aac");
        assert!(thumbnails.join(&thumbnail).is_file());
    }
}
