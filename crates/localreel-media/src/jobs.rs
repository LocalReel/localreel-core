use std::path::PathBuf;
use std::sync::Arc;

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::jobs::{JobContext, JobHandler, JobOutcome, JobRegistry};
use localreel_kernel::media_ops::MediaOps;
use localreel_kernel::storage::MediaUri;
use serde::{Deserialize, Serialize};

use crate::scan::discover_source;
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
    #[allow(dead_code)]
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

        context.report_progress(0.05, Some("discovering"));
        let discovery = discover_source(&self.0.db, &source)?;
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
