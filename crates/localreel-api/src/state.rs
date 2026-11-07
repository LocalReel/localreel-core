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
    pub ffmpeg_available: bool,
    pub started_at: i64,
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
        Arc::new(Self {
            config,
            db,
            events,
            jobs,
            media,
            accounts,
            login_limiter: LoginLimiter::new(),
            ffmpeg_available,
            started_at: now_secs(),
        })
    }
}
