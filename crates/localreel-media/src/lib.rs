//! First-party LocalReel media domain.
//!
//! This crate owns the media schema and the scan/probe/thumbnail/transcode
//! handlers that the kernel job worker executes. It must not know about HTTP,
//! static file serving or the web UI.

pub mod backup;
pub mod jobs;
pub mod legacy;
pub mod migrations;
pub mod model;
pub mod playback;
pub mod scan;
pub mod service;
pub mod state_migration;

pub use backup::{
    export_backup, import_backup, parse_backup, read_backup, Backup, BackupImportReport,
    BackupState, BACKUP_VERSION, MAX_BACKUP_BYTES,
};
pub use jobs::MediaJobs;
pub use legacy::{import_legacy, ImportReport};
pub use model::{
    CacheGcReport, CacheStats, DeleteSourceReport, Media, MediaFilter, MediaState, MediaVariant,
    RemapReport, ScanIssue, ScanRun, Source,
};
pub use scan::{discover_source, discover_source_with_cancel, ScanDiscovery};
pub use service::MediaService;
pub use state_migration::{
    episode_key, migrate_legacy_state, StateMigrationDetail, StateMigrationReport,
};
