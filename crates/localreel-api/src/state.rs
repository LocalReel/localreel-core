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
    pub watchers: std::sync::Arc<localreel_media::WatcherManager>,
    pub ffmpeg_available: bool,
    pub started_at: i64,
    /// Signalled by the admin shutdown endpoint and the tray icon; the server
    /// selects on it together with Ctrl+C.
    pub shutdown: std::sync::Arc<tokio::sync::Notify>,
}

impl AppState {
    pub fn new(
        config: Config,
        db: Database,
        events: EventBus,
        jobs: JobQueue,
        media: MediaService,
        accounts: AccountService,
        ffmpeg_available: bool,
    ) -> Arc<Self> {
        let watchers = std::sync::Arc::new(localreel_media::WatcherManager::new(
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
            ffmpeg_available,
            started_at: now_secs(),
            shutdown: std::sync::Arc::new(tokio::sync::Notify::new()),
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
