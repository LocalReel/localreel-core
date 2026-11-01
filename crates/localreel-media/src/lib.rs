//! First-party LocalReel media domain.
//!
//! This crate owns the media schema and the scan/probe/thumbnail/transcode
//! handlers that the kernel job worker executes. It must not know about HTTP,
//! static file serving or the web UI.

pub mod migrations;
pub mod model;
pub mod service;

pub use model::{Media, MediaState, MediaVariant, ScanIssue, ScanRun, Source};
pub use service::MediaService;
