//! Generic capabilities used by LocalReel.
//!
//! The kernel deliberately does not know what a video, source or thumbnail is.
//! It provides configuration, a SQLite database, a persistent job queue, an
//! in-process event bus, a local-first storage abstraction and typed media
//! operations (ffmpeg/ffprobe). The `localreel-media` crate owns the domain.

pub mod config;
pub mod db;
pub mod error;
pub mod events;
pub mod jobs;
pub mod media_ops;
pub mod migrations;
pub mod paths;
pub mod storage;
pub mod time;

/// Kernel crate version, exposed for `/api/v1/health`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
