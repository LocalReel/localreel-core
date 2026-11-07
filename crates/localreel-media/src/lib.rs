//! First-party LocalReel media domain.
//!
//! This crate owns the media schema and the scan/probe/thumbnail/transcode
//! handlers that the kernel job worker executes. It must not know about HTTP,
//! static file serving or the web UI.

pub mod account;
pub mod backup;
pub mod jobs;
pub mod legacy;
pub mod migrations;
pub mod model;
pub mod playback;
pub mod restore;
pub mod scan;
pub mod series;
pub mod service;
pub mod state_migration;
pub mod storage;
pub mod watch;

pub use account::{AccountService, CurrentUser, Role, Session, User, UserPatch};
pub use backup::{
    export_backup, import_backup, parse_backup, read_backup, read_backup_with_limit, Backup,
    BackupImportReport, BackupState, BackupUser, BACKUP_VERSION, MAX_BACKUP_BYTES,
    MAX_BACKUP_RESTORE_BYTES, MAX_BACKUP_USERS, MIN_BACKUP_FORMAT_VERSION,
};
pub use jobs::MediaJobs;
pub use legacy::{import_legacy, ImportReport};
pub use model::{
    CacheGcReport, CacheStats, DeleteSourceReport, FeedItem, Media, MediaFilter, MediaState,
    MediaVariant, RemapReport, ScanIssue, ScanRun, Source,
};
pub use restore::{restore_backup, RestoreReport};
pub use scan::{discover_source, discover_source_with_cancel, ScanDiscovery};
pub use service::{MediaPageQuery, MediaService};
pub use state_migration::{
    episode_key, migrate_legacy_state, StateMigrationDetail, StateMigrationReport,
};
pub use storage::StorageRegistry;
pub use watch::{WatcherManager, WatcherStatus};
