use std::path::PathBuf;
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
    /// The configuration file that was loaded, when there was one. `disable`
    /// and `enable` can write the switch back here instead of leaving a
    /// runtime-only state behind.
    pub config_source: Option<PathBuf>,
    /// The metadata provider actually active, or `None` when disabled or
    /// configured without an API key.
    pub metadata_provider: Option<String>,
    /// What the installed ffmpeg/ffprobe pair can do (`full`, `probe-only`,
    /// `missing`), so the UI can explain what is degraded instead of guessing.
    pub ffmpeg_capability: localreel_kernel::media_ops::FfmpegCapability,
    /// Resolved hardware acceleration: `none` when no encoder is usable.
    pub hwaccel: localreel_kernel::media_ops::HwAccel,
    pub started_at: i64,
    /// Signalled by the admin shutdown endpoint and the tray icon; the server
    /// selects on it together with Ctrl+C.
    pub shutdown: Arc<tokio::sync::Notify>,
    /// Persistent shutdown state for response bodies, including late subscribers.
    pub draining: tokio::sync::watch::Sender<bool>,
    pub stream_slots: Arc<tokio::sync::Semaphore>,
    pub auth_slots: Arc<tokio::sync::Semaphore>,
    /// Last successful remote release check, and when it ran.
    pub update_cache: std::sync::Mutex<Option<crate::update::CachedCheck>>,
    /// The one-at-a-time self-update: install kind, download progress and the
    /// verified installer the console can hand to the setup.
    pub update: crate::update_runtime::Runtime,
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
    pub config_source: Option<PathBuf>,
    pub metadata_provider: Option<String>,
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
            config_source,
            metadata_provider,
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
            config_source,
            metadata_provider,
            ffmpeg_capability,
            hwaccel,
            started_at: now_secs(),
            shutdown: Arc::new(tokio::sync::Notify::new()),
            draining: tokio::sync::watch::channel(false).0,
            stream_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            auth_slots: Arc::new(tokio::sync::Semaphore::new(4)),
            update_cache: std::sync::Mutex::new(None),
            update: crate::update_runtime::Runtime::new(),
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
