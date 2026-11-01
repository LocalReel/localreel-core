//! First-party LocalReel media domain.
//!
//! This crate owns the media schema and the scan/probe/thumbnail/transcode
//! handlers that the kernel job worker executes. It must not know about HTTP,
//! static file serving or the web UI.

pub mod jobs;
pub mod legacy;
pub mod migrations;
pub mod model;
pub mod playback;
pub mod scan;
pub mod service;

pub use jobs::MediaJobs;
pub use legacy::{import_legacy, ImportReport};
pub use model::{Media, MediaFilter, MediaState, MediaVariant, ScanIssue, ScanRun, Source};
pub use scan::{discover_source, ScanDiscovery};
pub use service::MediaService;
