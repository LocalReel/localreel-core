use serde::{Deserialize, Serialize};

/// Library view filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaFilter {
    All,
    Favorites,
    Continue,
}

impl MediaFilter {
    pub fn parse(value: &str) -> Option<Self> {
        match value.to_ascii_lowercase().as_str() {
            "" | "all" => Some(Self::All),
            "favorites" | "favorite" => Some(Self::Favorites),
            "continue" | "continue_watching" => Some(Self::Continue),
            _ => None,
        }
    }
}

/// A configured media source (a local directory or an HLS endpoint).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Source {
    pub id: i64,
    pub uri: String,
    pub kind: String,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
    pub archived_at: Option<i64>,
}

/// One indexed media item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Media {
    pub id: i64,
    pub uid: String,
    pub source_id: i64,
    pub uri: String,
    pub canonical_path: String,
    pub filename: String,
    pub title: Option<String>,
    pub duration: Option<f64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub file_size: i64,
    pub modified_at: i64,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub bitrate: Option<i64>,
    pub thumbnail_path: Option<String>,
    pub missing_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Per-media user state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaState {
    pub media_id: i64,
    pub liked: bool,
    pub favorite: bool,
    pub position: f64,
    pub duration: f64,
    pub updated_at: i64,
}

/// Scan history for one source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanRun {
    pub id: i64,
    pub source_id: i64,
    pub source_uri: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub status: String,
    pub discovered: i64,
    pub added: i64,
    pub updated: i64,
    pub unchanged: i64,
    pub removed: i64,
    pub failed: i64,
}

/// A per-run issue (unreadable file, failed probe, ...).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScanIssue {
    pub id: i64,
    pub run_id: i64,
    pub source_id: i64,
    pub path: String,
    pub message: String,
    pub created_at: i64,
}

/// A generated artifact registered in the database.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaVariant {
    pub id: i64,
    pub media_id: i64,
    pub kind: String,
    pub preset: String,
    pub path: String,
    pub size: i64,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheStats {
    pub transcode_count: i64,
    pub transcode_bytes: i64,
    pub thumbnail_count: i64,
    pub thumbnail_bytes: i64,
    pub total_bytes: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheGcReport {
    pub removed: i64,
    pub freed_bytes: i64,
    pub kept: i64,
    pub remaining_bytes: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RemapReport {
    pub source_id: i64,
    pub old_uri: String,
    pub new_uri: String,
    pub dry_run: bool,
    pub media_total: i64,
    pub remapped: i64,
    pub missing: i64,
    pub skipped: i64,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeleteSourceReport {
    pub source_id: i64,
    pub source_uri: String,
    pub dry_run: bool,
    pub archived: bool,
    pub media_count: i64,
    pub state_count: i64,
    pub variant_count: i64,
    pub deleted_files: i64,
}
