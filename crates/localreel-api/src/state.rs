use std::sync::Arc;

use localreel_kernel::config::Config;
use localreel_kernel::db::Database;
use localreel_kernel::events::EventBus;
use localreel_kernel::jobs::JobQueue;
use localreel_kernel::time::now_secs;
use localreel_media::{AccountService, MediaService};

use crate::auth::LoginLimiter;

/// Shared state for the Axum router.
pub struct AppState {
    pub config: Config,
    pub db: Database,
    pub events: EventBus,
    pub jobs: JobQueue,
    pub media: MediaService,
    pub accounts: AccountService,
    pub login_limiter: LoginLimiter,
    /// Filesystem subscriptions for the enabled local sources.
    pub watchers: Arc<localreel_media::WatcherManager>,
    /// The single ffmpeg budget (transcode / HLS packaging).
    pub transcodes: Arc<localreel_media::TranscodeGate>,
    /// The sidecar plugins this process started.
    pub plugins: Arc<localreel_plugin::PluginHost>,
    /// What the installed ffmpeg/ffprobe pair can do (`full`, `probe-only`,
    /// `missing`), so the UI can explain what is degraded instead of guessing.
    pub ffmpeg_capability: localreel_kernel::media_ops::FfmpegCapability,
    /// Resolved hardware acceleration: `none` when no encoder is usable.
    pub hwaccel: localreel_kernel::media_ops::HwAccel,
    pub started_at: i64,
    /// Signalled by the admin shutdown endpoint and the tray icon; the server
    /// selects on it together with Ctrl+C.
    pub shutdown: Arc<tokio::sync::Notify>,
}

/// The pieces the server assembles before it starts serving.
///
/// A parts struct instead of a long positional constructor: the list grows with
/// every subsystem, and a wrong order would compile.
pub struct AppStateParts {
    pub config: Config,
    pub db: Database,
    pub events: EventBus,
    pub jobs: JobQueue,
    pub media: MediaService,
    pub accounts: AccountService,
    pub transcodes: Arc<localreel_media::TranscodeGate>,
    pub plugins: Arc<localreel_plugin::PluginHost>,
    pub ffmpeg_capability: localreel_kernel::media_ops::FfmpegCapability,
    pub hwaccel: localreel_kernel::media_ops::HwAccel,
}

impl AppState {
    pub fn new(parts: AppStateParts) -> Arc<Self> {
        let AppStateParts {
            config,
            db,
            events,
            jobs,
            media,
            accounts,
            transcodes,
            plugins,
            ffmpeg_capability,
            hwaccel,
        } = parts;
        let watchers = Arc::new(localreel_media::WatcherManager::new(
            media.clone(),
            jobs.clone(),
            config.scan.watch,
        ));
        Arc::new(Self {
            config,
            db,
            events,
            jobs,
            media,
            accounts,
            login_limiter: LoginLimiter::new(),
            watchers,
            transcodes,
            plugins,
            ffmpeg_capability,
            hwaccel,
            started_at: now_secs(),
            shutdown: Arc::new(tokio::sync::Notify::new()),
        })
    }

    /// Reconcile the filesystem watchers with the `sources` table.
    ///
    /// Every mutation that can change the set of watchable directories calls
    /// this. Failures are logged, never propagated: a library that cannot be
    /// watched still scans on demand.
    pub fn refresh_watchers(&self) {
        if let Err(error) = self.watchers.refresh() {
            tracing::warn!("failed to refresh source watchers: {error}");
        }
    }
}
