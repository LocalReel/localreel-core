use std::path::Path;

use localreel_kernel::storage::MediaUri;

use crate::model::Media;

/// Default preset name used by the playback endpoint and transcode job.
pub const DEFAULT_TRANSCODE_PRESET: &str = "h264_aac";

/// True when the media URI is an HTTP(S) `.m3u8` playlist.
pub fn is_hls(media: &Media) -> bool {
    if !media.uri.to_ascii_lowercase().starts_with("http") {
        return false;
    }
    let without_query = media.uri.split('?').next().unwrap_or(&media.uri);
    without_query.to_ascii_lowercase().ends_with(".m3u8")
}

/// Whether the browser can play the file directly without a transcode.
///
/// This is deliberately conservative: unknown or missing codec metadata means
/// "transcode", not "hope the browser handles it".
pub fn direct_play_supported(media: &Media) -> bool {
    let extension = match MediaUri::parse(&media.uri) {
        Ok(MediaUri::Local(path)) => extension_of(&path),
        Ok(MediaUri::Http(url)) => extension_of(Path::new(url.split('?').next().unwrap_or(&url))),
        _ => None,
    };
    let video = media
        .video_codec
        .as_deref()
        .unwrap_or("")
        .to_ascii_lowercase();
    let audio = media
        .audio_codec
        .as_deref()
        .unwrap_or("none")
        .to_ascii_lowercase();

    match extension.as_deref() {
        Some("mp4") | Some("m4v") => {
            matches!(video.as_str(), "h264" | "avc" | "avc1")
                && matches!(audio.as_str(), "aac" | "mp3" | "none")
        }
        Some("webm") => {
            matches!(video.as_str(), "vp8" | "vp9")
                && matches!(audio.as_str(), "opus" | "vorbis" | "none")
        }
        _ => false,
    }
}

fn extension_of(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn media(uri: &str, video: Option<&str>, audio: Option<&str>) -> Media {
        Media {
            id: 1,
            uid: "u".into(),
            source_id: 1,
            uri: uri.into(),
            canonical_path: uri.into(),
            filename: "f".into(),
            title: None,
            duration: Some(10.0),
            width: Some(1280),
            height: Some(720),
            file_size: 1,
            modified_at: 1,
            video_codec: video.map(str::to_string),
            audio_codec: audio.map(str::to_string),
            bitrate: None,
            thumbnail_path: None,
            missing_at: None,
            created_at: 1,
            updated_at: 1,
        }
    }

    #[test]
    fn mp4_h264_aac_is_direct() {
        assert!(direct_play_supported(&media(
            "file:///D:/a.mp4",
            Some("h264"),
            Some("aac")
        )));
    }

    #[test]
    fn mkv_and_unknown_codecs_are_not_direct() {
        assert!(!direct_play_supported(&media(
            "file:///D:/a.mkv",
            Some("h264"),
            Some("aac")
        )));
        assert!(!direct_play_supported(&media(
            "file:///D:/a.mp4",
            Some("hevc"),
            Some("aac")
        )));
        assert!(!direct_play_supported(&media(
            "file:///D:/a.mp4",
            None,
            None
        )));
    }

    #[test]
    fn hls_is_passthrough() {
        assert!(is_hls(&media(
            "https://example.com/live/index.m3u8?token=x",
            None,
            None
        )));
        assert!(!is_hls(&media("file:///D:/a.m3u8", None, None)));
    }
}
