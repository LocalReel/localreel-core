//! Versioned data formats shared by the LocalReel crates.
//!
//! This crate intentionally has no I/O. It defines the on-disk/JSON shapes that
//! `localreel-core` reads and writes, so migrations and the operator CLI can
//! share one definition:
//!
//! * [`Backup`] — the portable user-state export (`version = 2`).
//! * [`ArtifactKind`] / [`artifact_file_name`] — deterministic names for
//!   generated thumbnails, transcodes and HLS packages.
//!
//! The format is versioned from day one. Unknown/newer versions must be
//! rejected by callers instead of being silently interpreted.

use serde::{Deserialize, Serialize};

/// Current backup format version.
pub const BACKUP_FORMAT_VERSION: u32 = 2;

/// Maximum number of sources accepted in one backup.
pub const MAX_BACKUP_SOURCES: usize = 256;

/// Maximum number of playback states accepted in one backup.
pub const MAX_BACKUP_STATES: usize = 100_000;

/// Portable backup of sources and per-media user state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Backup {
    pub version: u32,
    #[serde(default)]
    pub created_at: Option<i64>,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub states: Vec<BackupState>,
}

impl Backup {
    pub fn new() -> Self {
        Self {
            version: BACKUP_FORMAT_VERSION,
            created_at: Some(now_secs()),
            sources: Vec::new(),
            states: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), FormatError> {
        if self.version != BACKUP_FORMAT_VERSION {
            return Err(FormatError::UnsupportedBackupVersion(self.version));
        }
        if self.sources.len() > MAX_BACKUP_SOURCES {
            return Err(FormatError::TooManySources(self.sources.len()));
        }
        if self.states.len() > MAX_BACKUP_STATES {
            return Err(FormatError::TooManyStates(self.states.len()));
        }
        for source in &self.sources {
            if source.trim().is_empty() {
                return Err(FormatError::EmptySource);
            }
        }
        for state in &self.states {
            if state.path.trim().is_empty() {
                return Err(FormatError::EmptyStatePath);
            }
            if !state.position.is_finite() || state.position < 0.0 {
                return Err(FormatError::InvalidPosition);
            }
            if !state.duration.is_finite() || state.duration < 0.0 {
                return Err(FormatError::InvalidDuration);
            }
        }
        Ok(())
    }
}

impl Default for Backup {
    fn default() -> Self {
        Self::new()
    }
}

/// User state for one media item, keyed by `uid` when available and by the
/// canonical locator otherwise.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackupState {
    #[serde(default)]
    pub uid: Option<String>,
    pub path: String,
    #[serde(default)]
    pub liked: bool,
    #[serde(default)]
    pub favorite: bool,
    #[serde(default)]
    pub position: f64,
    #[serde(default)]
    pub duration: f64,
}

/// Kinds of generated media artifacts that live under the data directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Thumbnail,
    Transcode,
    Hls,
}

impl ArtifactKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Thumbnail => "thumb",
            ArtifactKind::Transcode => "transcode",
            ArtifactKind::Hls => "hls",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            ArtifactKind::Thumbnail => "jpg",
            ArtifactKind::Transcode => "mp4",
            ArtifactKind::Hls => "m3u8",
        }
    }
}

/// Deterministic, filesystem-safe artifact file name.
///
/// The source fingerprint is part of the name so a changed source never reuses
/// a stale artifact. All variable components are sanitized; the result contains
/// only ASCII letters, digits, `-`, `_` and `.`.
pub fn artifact_file_name(
    media_uid: &str,
    kind: ArtifactKind,
    preset: &str,
    source_size: u64,
    source_mtime: i64,
) -> String {
    format!(
        "{}_{}_{}_{}_{}.{}",
        kind.as_str(),
        sanitize_component(media_uid, 64),
        sanitize_component(preset, 32),
        source_size,
        source_mtime,
        kind.extension()
    )
}

/// Deterministic directory name for an HLS package (one directory per media and
/// preset).
pub fn artifact_dir_name(media_uid: &str, preset: &str) -> String {
    format!(
        "hls_{}_{}",
        sanitize_component(media_uid, 64),
        sanitize_component(preset, 32)
    )
}

fn sanitize_component(value: &str, max_len: usize) -> String {
    let mut out = String::with_capacity(value.len().min(max_len));
    for ch in value.chars() {
        if out.len() >= max_len {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Format-level errors. Callers decide how to surface them (HTTP 400, CLI
/// message, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    UnsupportedBackupVersion(u32),
    TooManySources(usize),
    TooManyStates(usize),
    EmptySource,
    EmptyStatePath,
    InvalidPosition,
    InvalidDuration,
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::UnsupportedBackupVersion(v) => {
                write!(
                    f,
                    "unsupported backup version {v} (expected {BACKUP_FORMAT_VERSION})"
                )
            }
            FormatError::TooManySources(n) => {
                write!(f, "too many sources: {n} (max {MAX_BACKUP_SOURCES})")
            }
            FormatError::TooManyStates(n) => {
                write!(f, "too many states: {n} (max {MAX_BACKUP_STATES})")
            }
            FormatError::EmptySource => write!(f, "source must not be empty"),
            FormatError::EmptyStatePath => write!(f, "state path must not be empty"),
            FormatError::InvalidPosition => write!(f, "position must be finite and non-negative"),
            FormatError::InvalidDuration => write!(f, "duration must be finite and non-negative"),
        }
    }
}

impl std::error::Error for FormatError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_roundtrip() {
        let mut backup = Backup::new();
        backup.sources.push("file:///D:/Videos".into());
        backup.states.push(BackupState {
            uid: Some("a1".into()),
            path: "file:///D:/Videos/ep01.mp4".into(),
            liked: true,
            favorite: false,
            position: 12.5,
            duration: 1400.0,
        });
        let json = serde_json::to_string(&backup).unwrap();
        let decoded: Backup = serde_json::from_str(&json).unwrap();
        assert_eq!(backup, decoded);
        assert!(decoded.validate().is_ok());
    }

    #[test]
    fn rejects_newer_backup_version() {
        let mut backup = Backup::new();
        backup.version = BACKUP_FORMAT_VERSION + 1;
        assert_eq!(
            backup.validate(),
            Err(FormatError::UnsupportedBackupVersion(backup.version))
        );
    }

    #[test]
    fn artifact_names_are_stable_and_safe() {
        let name = artifact_file_name("uid/../../x", ArtifactKind::Transcode, "h264 720p", 10, 20);
        assert_eq!(name, "transcode_uid_______x_h264_720p_10_20.mp4");
        assert!(!name.contains('/'));
        assert!(!name.contains('\\'));
        assert!(!name.contains(".."));
        assert_eq!(artifact_dir_name("uid", "h264"), "hls_uid_h264".to_string());
    }

    #[test]
    fn rejects_invalid_state_values() {
        let mut backup = Backup::new();
        backup.states.push(BackupState {
            uid: None,
            path: "x".into(),
            liked: false,
            favorite: false,
            position: f64::NAN,
            duration: 1.0,
        });
        assert!(backup.validate().is_err());
    }
}
