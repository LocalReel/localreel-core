use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::jobs::{JobContext, JobHandler, JobOutcome, JobRegistry};
use localreel_kernel::media_ops::{
    is_hardware_failure, HlsRequest, HwAccel, MediaInput, MediaOps, TranscodeOptions,
    TranscodeRequest,
};
use localreel_kernel::storage::MediaUri;
use serde::{Deserialize, Serialize};

use crate::backup::{read_backup_with_limit, Backup, MAX_BACKUP_RESTORE_BYTES};
use crate::playback::{preset_by_name, transcode_variant_key, DEFAULT_TRANSCODE_PRESET};
use crate::restore::restore_backup;
use crate::scan::discover_source_with_storage;
use crate::service::MediaService;
use crate::storage::StorageRegistry;

/// Job priority for thumbnails. Higher than the queue default (0) so short,
/// user-visible thumbnail work overtakes a bulk probe backlog instead of
/// starving behind it.
const THUMBNAIL_PRIORITY: i64 = 10;

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

#[derive(Clone)]
struct Shared {
    db: Database,
    media_ops: Arc<MediaOps>,
    thumbnail_dir: PathBuf,
    transcode_dir: PathBuf,
    hls_dir: PathBuf,
    preview_dir: PathBuf,
    storage: Arc<StorageRegistry>,
    /// Base URL of the running server, used to read remote media over HTTP.
    stream_base: Option<String>,
    /// Render progress-bar sprite sheets for long videos.
    preview_sprites: bool,
    /// Anything shorter than this gets no sprite.
    preview_min_secs: f64,
    /// External metadata provider, when one is configured.
    metadata: Option<Arc<dyn crate::metadata::MetadataProvider>>,
    /// Where downloaded posters live.
    poster_dir: PathBuf,
    /// A media item is re-matched at most once per this window.
    metadata_max_age_secs: i64,
}

impl MediaJobs {
    pub fn new(
        db: Database,
        media_ops: Arc<MediaOps>,
        thumbnail_dir: PathBuf,
        transcode_dir: PathBuf,
    ) -> Self {
        Self::with_storage(
            db,
            media_ops,
            thumbnail_dir,
            transcode_dir,
            Arc::new(StorageRegistry::new()),
        )
    }

    /// Build handlers whose sources can resolve plugin-provided storage.
    pub fn with_storage(
        db: Database,
        media_ops: Arc<MediaOps>,
        thumbnail_dir: PathBuf,
        transcode_dir: PathBuf,
        storage: Arc<StorageRegistry>,
    ) -> Self {
        let data_dir = thumbnail_dir.parent().map(|parent| parent.to_path_buf());
        Self {
            shared: Arc::new(Shared {
                db,
                media_ops,
                hls_dir: data_dir
                    .as_ref()
                    .map(|parent| parent.join("hls"))
                    .unwrap_or_else(|| PathBuf::from("hls")),
                preview_dir: data_dir
                    .as_ref()
                    .map(|parent| parent.join("previews"))
                    .unwrap_or_else(|| PathBuf::from("previews")),
                thumbnail_dir,
                transcode_dir,
                storage,
                stream_base: None,
                preview_sprites: true,
                preview_min_secs: 600.0,
                metadata: None,
                poster_dir: data_dir
                    .as_ref()
                    .map(|parent| parent.join("cache").join("posters"))
                    .unwrap_or_else(|| PathBuf::from("posters")),
                metadata_max_age_secs: 7 * 24 * 60 * 60,
            }),
        }
    }

    /// Apply one change to the shared configuration without repeating every
    /// field for each builder.
    fn reshape(self, update: impl FnOnce(&mut Shared)) -> Self {
        let mut shared = (*self.shared).clone();
        update(&mut shared);
        Self {
            shared: Arc::new(shared),
        }
    }

    /// Teach the handlers where the server listens, so remote media can be read
    /// through `/api/v1/media/<id>/stream` instead of a local path.
    pub fn with_stream_base(self, base: impl Into<String>) -> Self {
        let base = base.into();
        self.reshape(|shared| shared.stream_base = Some(base))
    }

    /// Point HLS packaging at the configured directory.
    pub fn with_hls_dir(self, dir: impl Into<PathBuf>) -> Self {
        let dir = dir.into();
        self.reshape(|shared| shared.hls_dir = dir)
    }

    /// Configure progress-bar previews.
    pub fn with_previews(self, dir: impl Into<PathBuf>, enabled: bool, min_secs: f64) -> Self {
        let dir = dir.into();
        self.reshape(|shared| {
            shared.preview_dir = dir;
            shared.preview_sprites = enabled;
            shared.preview_min_secs = min_secs;
        })
    }

    /// Configure the optional external metadata provider.
    pub fn with_metadata(
        self,
        provider: Option<Arc<dyn crate::metadata::MetadataProvider>>,
        poster_dir: impl Into<PathBuf>,
    ) -> Self {
        let poster_dir = poster_dir.into();
        self.reshape(|shared| {
            shared.metadata = provider;
            shared.poster_dir = poster_dir;
        })
    }

    pub fn register(&self, registry: &mut JobRegistry) {
        registry.register(ScanSourceHandler(self.shared.clone()));
        registry.register(ProbeMediaHandler(self.shared.clone()));
        registry.register(ThumbnailHandler(self.shared.clone()));
        registry.register(TranscodeHandler(self.shared.clone()));
        registry.register(HlsPackageHandler(self.shared.clone()));
        registry.register(PreviewSpriteHandler(self.shared.clone()));
        registry.register(MetadataMatchHandler(self.shared.clone()));
        registry.register(CacheGcHandler(self.shared.clone()));
        registry.register(RestoreBackupHandler(self.shared.clone()));
        registry.register(RestoreBackupFileHandler(self.shared.clone()));
    }
}

/// Where ffprobe/ffmpeg should read this media from.
fn media_input(media: &crate::model::Media, stream_base: Option<&str>) -> Result<MediaInput> {
    match MediaUri::parse(&media.uri)? {
        MediaUri::Local(path) => Ok(MediaInput::path(path)),
        _ => {
            let base = stream_base.ok_or_else(|| {
                KernelError::InvalidInput(format!(
                    "media {} is remote; probing it requires the running server (start `serve`)",
                    media.id
                ))
            })?;
            Ok(MediaInput::url(format!(
                "{}/api/v1/media/{}/stream",
                base.trim_end_matches('/'),
                media.id
            )))
        }
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
        let service = MediaService::with_storage(self.0.db.clone(), self.0.storage.clone());
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
        let provider = service.storage_for(&source)?;
        let discovery = discover_source_with_storage(
            &self.0.db,
            &source,
            provider.as_ref(),
            &context.cancel_flag(),
        )?;
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

fn enqueue_probe_version(
    queue: &localreel_kernel::jobs::JobQueue,
    media: &crate::model::Media,
) -> Result<String> {
    queue.enqueue(
        "probe_media",
        &serde_json::json!({ "media_id": media.id }).to_string(),
        Some(&format!(
            "probe:{}:{}:{}",
            media.id, media.file_size, media.modified_at
        )),
        3,
    )
}

fn derived_job_key(kind: &str, media: &crate::model::Media) -> String {
    format!(
        "{kind}:{}:{}:{}",
        media.id, media.file_size, media.modified_at
    )
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

        let input = media_input(&media, self.0.stream_base.as_deref())?;
        if let MediaInput::Path(path) = &input {
            if !path.is_file() {
                return Err(KernelError::NotFound(format!(
                    "media file is missing: {}",
                    path.display()
                )));
            }
        }

        context.report_progress(0.1, Some("ffprobe"));
        let probe = self.0.media_ops.probe(&input, &context.cancel_flag())?;
        if !service.apply_probe_for_media(&media, &probe)? {
            if let Some(current) = service
                .get_media(media.id)?
                .filter(|item| item.missing_at.is_none())
            {
                // Versioned key never deduplicates to this superseded execution.
                enqueue_probe_version(context.queue(), &current)?;
            }
            return JobOutcome::json(serde_json::json!({ "superseded": true }));
        }
        context.report_progress(0.9, Some("metadata updated"));

        if media.thumbnail_path.is_none() {
            let payload = serde_json::json!({ "media_id": media.id }).to_string();
            // Thumbnails are short and user-visible, while a fresh scan can
            // leave tens of thousands of probes queued; a higher priority keeps
            // them from starving behind the probe backlog.
            if let Err(error) = context.queue().enqueue_with_priority(
                "thumbnail",
                &payload,
                Some(&derived_job_key("thumbnail", &media)),
                3,
                THUMBNAIL_PRIORITY,
            ) {
                tracing::warn!(
                    media_id = media.id,
                    "failed to enqueue thumbnail job: {error}"
                );
            }
        }

        // Long videos get a progress-bar preview; short ones gain nothing from
        // one and the render still costs a process launch.
        if self.0.preview_sprites
            && probe
                .duration
                .is_some_and(|duration| duration >= self.0.preview_min_secs)
        {
            let payload = serde_json::json!({ "media_id": media.id }).to_string();
            if let Err(error) = context.queue().enqueue_with_priority(
                "preview_sprite",
                &payload,
                Some(&derived_job_key("sprite", &media)),
                2,
                THUMBNAIL_PRIORITY - 5,
            ) {
                tracing::warn!(
                    media_id = media.id,
                    "failed to enqueue preview sprite job: {error}"
                );
            }
        }

        // External metadata is opt-in and refreshed at most once a week, so a
        // rescan does not hammer the provider.
        if self.0.metadata.is_some()
            && service.metadata_is_stale(media.id, self.0.metadata_max_age_secs)?
        {
            let payload = serde_json::json!({ "media_id": media.id }).to_string();
            if let Err(error) = context.queue().enqueue_with_priority(
                "metadata_match",
                &payload,
                Some(&format!("metadata:{}", media.id)),
                1,
                THUMBNAIL_PRIORITY - 6,
            ) {
                tracing::warn!(
                    media_id = media.id,
                    "failed to enqueue metadata job: {error}"
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

        let input = media_input(&media, self.0.stream_base.as_deref())?;
        if let MediaInput::Path(path) = &input {
            if !path.is_file() {
                return Err(KernelError::NotFound(format!(
                    "media file is missing: {}",
                    path.display()
                )));
            }
        }

        let filename = format!(
            "thumb_{}_{}_{}.jpg",
            media.uid, media.file_size, media.modified_at
        );
        let output = self.0.thumbnail_dir.join(&filename);
        let seek = media
            .duration
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .map(|duration| (duration * 0.1).min(10.0))
            .unwrap_or(0.0);

        context.report_progress(0.1, Some("ffmpeg thumbnail"));
        self.0
            .media_ops
            .thumbnail(&input, &output, seek, 480, &context.cancel_flag())?;

        let size = std::fs::metadata(&output)
            .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        service.set_thumbnail_for_media(&media, &filename)?;
        service.record_variant_for_media(&media, "thumbnail", "default", &filename, size)?;
        context.report_progress(1.0, Some("thumbnail ready"));
        Ok(JobOutcome::empty())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct PreviewPayload {
    media_id: i64,
}

/// Render the sprite sheet the progress bar shows on hover.
///
/// Only long videos ask for one: a ten-second clip gains nothing from a preview
/// and the encode still costs a process launch.
struct PreviewSpriteHandler(Arc<Shared>);

impl JobHandler for PreviewSpriteHandler {
    fn kind(&self) -> &'static str {
        "preview_sprite"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: PreviewPayload = context.payload()?;
        let service = MediaService::new(self.0.db.clone());
        let media = service
            .get_media(payload.media_id)?
            .ok_or_else(|| KernelError::NotFound(format!("media {}", payload.media_id)))?;

        let Some(duration) = media.duration.filter(|value| *value > 0.0) else {
            return JobOutcome::json(serde_json::json!({ "skipped": "unknown duration" }));
        };
        if duration < self.0.preview_min_secs {
            return JobOutcome::json(serde_json::json!({ "skipped": "too short" }));
        }

        let filename = format!(
            "{}_{}_{}.jpg",
            sanitize_component(&media.uid, 64),
            media.file_size,
            media.modified_at
        );
        let output = self.0.preview_dir.join(&filename);
        if output.is_file() {
            let size = std::fs::metadata(&output)
                .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            service.record_variant_for_media(&media, "sprite", "default", &filename, size)?;
            return JobOutcome::json(serde_json::json!({
                "file": filename,
                "exists": true,
            }));
        }

        let input = media_input(&media, self.0.stream_base.as_deref())?;
        if let MediaInput::Path(path) = &input {
            if !path.is_file() {
                return Err(KernelError::NotFound(format!(
                    "media file is missing: {}",
                    path.display()
                )));
            }
        }

        let interval = localreel_kernel::media_ops::preview_interval(Some(duration));
        context.report_progress(0.1, Some("rendering preview sprites"));
        self.0
            .media_ops
            .preview_sprite(&input, &output, interval, &context.cancel_flag())?;

        let size = std::fs::metadata(&output)
            .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        service.record_variant_for_media(&media, "sprite", "default", &filename, size)?;
        context.report_progress(1.0, Some("preview sprites ready"));
        JobOutcome::json(serde_json::json!({
            "file": filename,
            "interval": interval,
            "size": size,
        }))
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct TranscodePayload {
    media_id: i64,
    #[serde(default)]
    preset: Option<String>,
    /// ffmpeg stream index of the audio track to keep; omitted = first track.
    #[serde(default)]
    audio_stream: Option<usize>,
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
        let preset = preset_by_name(&preset_name).ok_or_else(|| {
            KernelError::InvalidInput(format!("unsupported transcode preset '{preset_name}'"))
        })?;
        // The audio choice is part of the variant key, so two languages never
        // overwrite each other's cache entry.
        let variant_key = transcode_variant_key(&preset_name, payload.audio_stream);

        let service = MediaService::new(self.0.db.clone());
        let media = service
            .get_media(payload.media_id)?
            .ok_or_else(|| KernelError::NotFound(format!("media {}", payload.media_id)))?;
        let input = media_input(&media, self.0.stream_base.as_deref())?;
        if let MediaInput::Path(path) = &input {
            if !path.is_file() {
                return Err(KernelError::NotFound(format!(
                    "media file is missing: {}",
                    path.display()
                )));
            }
        }

        let filename = format!(
            "transcode_{}_{}_{}_{}_{}.mp4",
            sanitize_component(&media.uid, 64),
            sanitize_component(&preset_name, 32),
            payload
                .audio_stream
                .map(|index| format!("a{index}"))
                .unwrap_or_else(|| "aauto".to_string()),
            media.file_size,
            media.modified_at
        );
        let output = self.0.transcode_dir.join(&filename);
        let hwaccel = self.0.media_ops.hwaccel();
        let options = TranscodeOptions {
            hwaccel,
            audio_stream: payload.audio_stream,
        };
        let cancel = context.cancel_flag();

        context.report_progress(0.0, Some("transcoding"));
        let mut fallback = false;
        match self.0.media_ops.transcode(
            &input,
            &output,
            &preset,
            &TranscodeRequest {
                options,
                duration_secs: media.duration,
                cancel: &cancel,
            },
            progress_reporter(context),
        ) {
            Ok(()) => {}
            Err(error) if hwaccel != HwAccel::None && is_hardware_failure(&error.to_string()) => {
                // The encoder exists but this machine cannot run it (no device,
                // no driver, a container without /dev/dri). One CPU retry turns
                // a hard failure into a slower success.
                tracing::warn!(
                    media_id = media.id,
                    hwaccel = hwaccel.as_str(),
                    "hardware encoder failed, retrying on the CPU: {error}"
                );
                context.report_progress(0.0, Some("hardware encoder failed, retrying on CPU"));
                self.0.media_ops.transcode(
                    &input,
                    &output,
                    &preset,
                    &TranscodeRequest {
                        options: TranscodeOptions {
                            hwaccel: HwAccel::None,
                            ..options
                        },
                        duration_secs: media.duration,
                        cancel: &cancel,
                    },
                    progress_reporter(context),
                )?;
                fallback = true;
            }
            Err(error) => return Err(error),
        }

        let size = std::fs::metadata(&output)
            .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        service.record_variant_for_media(&media, "transcode", &variant_key, &filename, size)?;
        context.report_progress(
            1.0,
            Some(if fallback {
                "transcode ready (hardware encoder failed, retried on CPU)"
            } else {
                "transcode ready"
            }),
        );
        JobOutcome::json(serde_json::json!({
            "path": filename,
            "preset": preset_name,
            "size": size,
            "hwaccel": if fallback { "cpu" } else { hwaccel.as_str() },
        }))
    }
}

/// Throttled progress reporting for one transcode attempt.
///
/// A closure cannot be reused after it is moved into `transcode`, so each
/// attempt builds its own.
fn progress_reporter(context: &JobContext) -> impl FnMut(f64) + '_ {
    let mut last_progress = 0.0f64;
    let mut last_report = Instant::now() - Duration::from_secs(2);
    move |progress: f64| {
        if progress - last_progress >= 0.02 || last_report.elapsed() >= Duration::from_secs(1) {
            context.report_progress(progress, Some("transcoding"));
            last_progress = progress;
            last_report = Instant::now();
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct HlsPayload {
    media_id: i64,
    #[serde(default)]
    segment_secs: Option<u32>,
    /// ffmpeg stream index of the audio track to keep; omitted = first track.
    #[serde(default)]
    audio_stream: Option<usize>,
}

struct HlsPackageHandler(Arc<Shared>);

impl JobHandler for HlsPackageHandler {
    fn kind(&self) -> &'static str {
        "hls_package"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: HlsPayload = context.payload()?;
        let service = MediaService::with_storage(self.0.db.clone(), self.0.storage.clone());
        let media = service
            .get_media(payload.media_id)?
            .ok_or_else(|| KernelError::NotFound(format!("media {}", payload.media_id)))?;
        let input = media_input(&media, self.0.stream_base.as_deref())?;
        if let MediaInput::Path(path) = &input {
            if !path.is_file() {
                return Err(KernelError::NotFound(format!(
                    "media file is missing: {}",
                    path.display()
                )));
            }
        }

        let version = format!("{}_{}", media.file_size, media.modified_at);
        let variant_path = format!("{}/{}/index.m3u8", media.uid, version);
        let final_dir = self.0.hls_dir.join(&media.uid).join(&version);
        let playlist = final_dir.join("index.m3u8");
        if playlist.is_file() {
            let size = std::fs::metadata(&playlist)
                .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
                .unwrap_or(0);
            service.record_variant_for_media(&media, "hls", "default", &variant_path, size)?;
            return JobOutcome::json(serde_json::json!({
                "playlist": variant_path,
                "exists": true,
            }));
        }

        let segment_secs = payload.segment_secs.unwrap_or(4).clamp(1, 30);
        std::fs::create_dir_all(self.0.hls_dir.join(&media.uid))?;
        let temp_dir = self
            .0
            .hls_dir
            .join(&media.uid)
            .join(format!("{version}.tmp"));
        if temp_dir.exists() {
            std::fs::remove_dir_all(&temp_dir).ok();
        }
        context.report_progress(0.05, Some("packaging hls"));
        self.0.media_ops.package_hls(
            &input,
            &temp_dir,
            &HlsRequest {
                segment_secs,
                duration_secs: media.duration,
                audio_stream: payload.audio_stream,
                cancel: &context.cancel_flag(),
            },
            |progress| context.report_progress(0.05 + progress * 0.9, Some("packaging hls")),
        )?;

        if final_dir.exists() {
            std::fs::remove_dir_all(&final_dir).map_err(|error| {
                KernelError::Media(format!("cannot replace {}: {error}", final_dir.display()))
            })?;
        }
        std::fs::rename(&temp_dir, &final_dir).map_err(|error| {
            KernelError::Media(format!(
                "cannot move {} to {}: {error}",
                temp_dir.display(),
                final_dir.display()
            ))
        })?;

        let size = i64::try_from(directory_size(&final_dir)).unwrap_or(i64::MAX);
        service.record_variant_for_media(&media, "hls", "default", &variant_path, size)?;
        context.report_progress(1.0, Some("hls ready"));
        JobOutcome::json(serde_json::json!({
            "playlist": variant_path,
            "segment_secs": segment_secs,
            "bytes": size,
        }))
    }
}

fn directory_size(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let Ok(entries) = std::fs::read_dir(path) else {
        return 0;
    };
    for entry in entries.flatten() {
        if let Ok(metadata) = entry.metadata() {
            if metadata.is_dir() {
                total = total.saturating_add(directory_size(&entry.path()));
            } else {
                total = total.saturating_add(metadata.len());
            }
        }
    }
    total
}

#[derive(Debug, Serialize, Deserialize)]
struct MetadataPayload {
    media_id: i64,
}

/// Match one media item against the configured provider.
///
/// Everything here is best effort: a provider that is down, a title that does
/// not match and a poster that fails to download are all warnings, never a job
/// failure. A wrong poster is worse than no poster, so the confidence threshold
/// does the rejecting.
struct MetadataMatchHandler(Arc<Shared>);

impl JobHandler for MetadataMatchHandler {
    fn kind(&self) -> &'static str {
        "metadata_match"
    }

    fn run(&self, context: &JobContext) -> Result<JobOutcome> {
        let payload: MetadataPayload = context.payload()?;
        let Some(provider) = &self.0.metadata else {
            return JobOutcome::json(serde_json::json!({ "skipped": "no provider configured" }));
        };
        let service = MediaService::new(self.0.db.clone());
        let media = service
            .get_media(payload.media_id)?
            .ok_or_else(|| KernelError::NotFound(format!("media {}", payload.media_id)))?;

        // Episodes are matched by their show, not by "S01E02"; films by the
        // cleaned filename.
        let (series_key, series_title) = service.series_info(media.id)?;
        let series = series_key.is_some();
        let (title, year) = match series_title {
            Some(title) if !title.trim().is_empty() => (title, None),
            _ => crate::metadata::clean_title(&media.filename),
        };
        if title.trim().is_empty() {
            return JobOutcome::json(serde_json::json!({ "skipped": "no usable title" }));
        }
        let query = crate::metadata::MetadataQuery {
            title,
            year,
            series,
        };

        context.report_progress(0.1, Some("searching metadata"));
        let candidate = match provider.search(&query) {
            Ok(candidate) => candidate,
            Err(error) => {
                tracing::warn!(media_id = media.id, "metadata search failed: {error}");
                return JobOutcome::json(serde_json::json!({ "error": error }));
            }
        };
        let Some(candidate) = candidate else {
            return JobOutcome::json(serde_json::json!({
                "matched": false,
                "title": query.title,
            }));
        };

        context.report_progress(0.5, Some("downloading poster"));
        let mut poster_path = None;
        if let Some(url) = &candidate.poster_url {
            match provider.download(url) {
                Ok(bytes) if bytes.len() > 1024 => {
                    std::fs::create_dir_all(&self.0.poster_dir)?;
                    let name = format!("{}.jpg", media.id);
                    std::fs::write(self.0.poster_dir.join(&name), &bytes)?;
                    poster_path = Some(name);
                }
                Ok(_) => tracing::warn!(media_id = media.id, "poster was too small to be an image"),
                Err(error) => {
                    tracing::warn!(media_id = media.id, "poster download failed: {error}")
                }
            }
        }

        service.upsert_metadata(
            media.id,
            provider.name(),
            &candidate,
            poster_path.as_deref(),
        )?;
        context.report_progress(1.0, Some("metadata matched"));
        JobOutcome::json(serde_json::json!({
            "matched": true,
            "provider_id": candidate.provider_id,
            "title": candidate.title,
            "confidence": candidate.confidence,
            "poster": poster_path.is_some(),
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
        let service = MediaService::new(self.0.db.clone());
        let mut report = service.gc_transcode_cache(
            &self.0.transcode_dir,
            max_bytes,
            max_age_secs,
            protect_recent_secs,
        )?;
        context.report_progress(0.8, Some("scanning hls packages"));
        if let Ok((hls_removed, hls_freed)) = service.gc_hls_cache(&self.0.hls_dir, max_age_secs) {
            report.removed += hls_removed;
            report.freed_bytes = report.freed_bytes.saturating_add(hls_freed);
        }
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
            crate::restore::RestoreInput {
                backup: &payload.backup,
                media_ops: &self.0.media_ops,
                thumbnail_dir: &self.0.thumbnail_dir,
                dry_run: false,
                cancel: &cancel,
            },
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
                crate::restore::RestoreInput {
                    backup: &backup,
                    media_ops: &self.0.media_ops,
                    thumbnail_dir: &self.0.thumbnail_dir,
                    dry_run: false,
                    cancel: &cancel,
                },
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
    #[test]
    fn derived_jobs_do_not_dedupe_to_a_running_old_version() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("derived.db").to_str().unwrap(), 1).unwrap();
        kernel_migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        let queue = JobQueue::new(db, EventBus::new(16));
        let old_media = test_media("http://example.test/movie.mp4");
        let mut current = old_media.clone();
        current.modified_at += 1;
        for (kind, prefix) in [("thumbnail", "thumbnail"), ("preview_sprite", "sprite")] {
            let old = queue
                .enqueue(kind, "{}", Some(&derived_job_key(prefix, &old_media)), 3)
                .unwrap();
            let claimed = queue.claim("worker", 30).unwrap().unwrap();
            assert_eq!(claimed.id, old);
            let new = queue
                .enqueue(kind, "{}", Some(&derived_job_key(prefix, &current)), 3)
                .unwrap();
            assert_ne!(old, new);
            assert_eq!(queue.get(&new).unwrap().unwrap().status, "queued");
            queue.cancel(&new).unwrap();
            queue.complete(&old, "{}").unwrap();
        }
    }

    #[test]
    fn superseded_probe_gets_a_fresh_retry_budget_and_key() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("probe.db").to_str().unwrap(), 1).unwrap();
        kernel_migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        let queue = JobQueue::new(db, EventBus::new(16));
        let old = queue
            .enqueue("probe_media", "{\"media_id\":7}", Some("probe:7"), 1)
            .unwrap();
        queue.claim("old", 30).unwrap().unwrap();
        let current = test_media("http://example.test/movie.mp4");
        let fresh = enqueue_probe_version(&queue, &current).unwrap();
        assert_ne!(old, fresh);
        let job = queue.get(&fresh).unwrap().unwrap();
        assert_eq!(job.max_attempts, 3);
        assert_eq!(job.attempts, 0);
        assert_eq!(job.status, "queued");
        queue.complete(&old, "{\"superseded\":true}").unwrap();
        assert_eq!(queue.get(&fresh).unwrap().unwrap().status, "queued");
    }

    #[test]
    fn local_media_reads_from_its_path() {
        let media = test_media("file:///D:/Videos/x.mp4");
        let input = media_input(&media, None).unwrap();
        assert!(!input.is_url());
    }

    #[test]
    fn remote_media_reads_through_the_stream_endpoint() {
        let media = test_media("http://plugin.local/Show/ep01.mp4");
        assert_eq!(
            media_input(&media, Some("http://127.0.0.1:3001/")).unwrap(),
            MediaInput::url("http://127.0.0.1:3001/api/v1/media/7/stream")
        );
        assert!(media_input(&media, None).is_err());
    }

    fn test_media(uri: &str) -> crate::model::Media {
        crate::model::Media {
            id: 7,
            uid: "uid-7".into(),
            source_id: 1,
            uri: uri.into(),
            canonical_path: uri.into(),
            filename: "x.mp4".into(),
            title: None,
            duration: None,
            width: None,
            height: None,
            file_size: 1,
            modified_at: 1,
            video_codec: None,
            audio_codec: None,
            bitrate: None,
            thumbnail_path: None,
            missing_at: None,
            created_at: 0,
            updated_at: 0,
            series_key: None,
            series_title: None,
            episode: None,
            series_id: None,
        }
    }
}
