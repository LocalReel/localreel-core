use std::path::Path;

use localreel_kernel::media_ops::TranscodePreset;
use localreel_kernel::storage::MediaUri;

use crate::model::Media;

/// Default preset name used by the playback endpoint and transcode job.
pub const DEFAULT_TRANSCODE_PRESET: &str = "h264_aac";

/// Remux preset: copy the H.264 video stream and the audio stream into MP4.
pub const REMUX_PRESET: &str = "remux_h264";

/// Remux preset with an AAC audio track, for containers whose audio (AC-3,
/// DTS, FLAC, ...) the browser cannot play straight from MP4.
pub const REMUX_PRESET_AAC: &str = "remux_h264_aac";

/// True when the media URI is an HTTP(S) `.m3u8` playlist.
pub fn is_hls(media: &Media) -> bool {
    if !media.uri.to_ascii_lowercase().starts_with("http") {
        return false;
    }
    let without_query = media.uri.split('?').next().unwrap_or(&media.uri);
    without_query.to_ascii_lowercase().ends_with(".m3u8")
}

fn extension(media: &Media) -> Option<String> {
    match MediaUri::parse(&media.uri) {
        Ok(MediaUri::Local(path)) => extension_of(&path),
        Ok(MediaUri::Http(url)) => extension_of(Path::new(url.split('?').next().unwrap_or(&url))),
        _ => None,
    }
}

fn video_is_h264(media: &Media) -> bool {
    matches!(
        media
            .video_codec
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "h264" | "avc" | "avc1"
    )
}

fn audio_is_browser_safe(media: &Media) -> bool {
    matches!(
        media
            .audio_codec
            .as_deref()
            .unwrap_or("none")
            .to_ascii_lowercase()
            .as_str(),
        "aac" | "mp3" | "none"
    )
}

/// Containers that are not playable as-is but whose H.264 stream can be
/// rewritten into MP4 without re-encoding.
fn remux_container(extension: Option<&str>) -> bool {
    matches!(
        extension,
        Some("mkv") | Some("mov") | Some("ts") | Some("m2ts") | Some("flv") | Some("avi")
    )
}

/// Preset to use when direct play is not possible.
///
/// * H.264 in a non-MP4 container → copy the video stream (and the audio when
///   the browser can take it), which takes seconds instead of minutes.
/// * anything else → a full H.264/AAC re-encode.
pub fn preferred_preset(media: &Media) -> &'static str {
    let extension = extension(media);
    if remux_container(extension.as_deref()) && video_is_h264(media) {
        return if audio_is_browser_safe(media) {
            REMUX_PRESET
        } else {
            REMUX_PRESET_AAC
        };
    }
    DEFAULT_TRANSCODE_PRESET
}

/// Resolve a preset name to the typed preset the transcode job executes.
pub fn preset_by_name(name: &str) -> Option<TranscodePreset> {
    let preset = match name {
        DEFAULT_TRANSCODE_PRESET => TranscodePreset::default(),
        REMUX_PRESET => TranscodePreset {
            name: REMUX_PRESET.to_string(),
            video_codec: "copy".to_string(),
            audio_codec: "copy".to_string(),
            crf: 0,
            speed: "ultrafast".to_string(),
            audio_bitrate: "160k".to_string(),
        },
        REMUX_PRESET_AAC => TranscodePreset {
            name: REMUX_PRESET_AAC.to_string(),
            video_codec: "copy".to_string(),
            audio_codec: "aac".to_string(),
            crf: 0,
            speed: "ultrafast".to_string(),
            audio_bitrate: "160k".to_string(),
        },
        _ => return None,
    };
    Some(preset)
}

/// How a file will be played. `Remux` is cheap (seconds, stream copy);
/// `Transcode` re-encodes and can take minutes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackMode {
    Direct,
    Remux,
    Transcode,
}

impl PlaybackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PlaybackMode::Direct => "direct",
            PlaybackMode::Remux => "remux",
            PlaybackMode::Transcode => "transcode",
        }
    }
}

/// Decide the playback mode from probe metadata, without a full [`Media`] row.
///
/// The feed uses this to tell the UI which neighbours are cheap to prepare, and
/// the API uses it for the same decision, so both stay in sync.
pub fn playback_mode(
    video_codec: Option<&str>,
    audio_codec: Option<&str>,
    filename: &str,
) -> PlaybackMode {
    let extension = Path::new(filename)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    let video = video_codec.unwrap_or("").to_ascii_lowercase();
    let audio = audio_codec.unwrap_or("none").to_ascii_lowercase();
    let video_is_h264 = matches!(video.as_str(), "h264" | "avc" | "avc1");
    let audio_is_safe = matches!(audio.as_str(), "aac" | "mp3" | "none");

    let direct = match extension.as_deref() {
        Some("mp4") | Some("m4v") => {
            video_is_h264 && matches!(audio.as_str(), "aac" | "mp3" | "none")
        }
        Some("webm") => {
            matches!(video.as_str(), "vp8" | "vp9")
                && matches!(audio.as_str(), "opus" | "vorbis" | "none")
        }
        _ => false,
    };
    if direct {
        return PlaybackMode::Direct;
    }
    if remux_container(extension.as_deref()) && video_is_h264 && audio_is_safe {
        return PlaybackMode::Remux;
    }
    PlaybackMode::Transcode
}

/// Whether the browser can play the file directly without a transcode.
///
/// This is deliberately conservative: unknown or missing codec metadata means
/// "transcode", not "hope the browser handles it".
pub fn direct_play_supported(media: &Media) -> bool {
    let extension = extension(media);
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

    #[test]
    fn mkv_h264_aac_is_remuxed() {
        assert_eq!(
            preferred_preset(&media("file:///D:/a.mkv", Some("h264"), Some("aac"))),
            REMUX_PRESET
        );
        assert_eq!(
            preferred_preset(&media("file:///D:/a.flv", Some("avc1"), Some("mp3"))),
            REMUX_PRESET
        );
    }

    #[test]
    fn remux_transcodes_only_the_audio_when_needed() {
        assert_eq!(
            preferred_preset(&media("file:///D:/a.mkv", Some("h264"), Some("ac3"))),
            REMUX_PRESET_AAC
        );
        assert_eq!(
            preferred_preset(&media("file:///D:/a.mkv", Some("h264"), Some("dts"))),
            REMUX_PRESET_AAC
        );
    }

    #[test]
    fn non_h264_or_unknown_needs_a_full_reencode() {
        assert_eq!(
            preferred_preset(&media("file:///D:/a.mkv", Some("hevc"), Some("aac"))),
            DEFAULT_TRANSCODE_PRESET
        );
        assert_eq!(
            preferred_preset(&media("file:///D:/a.flv", Some("rv40"), Some("aac"))),
            DEFAULT_TRANSCODE_PRESET
        );
        assert_eq!(
            preferred_preset(&media("file:///D:/a.mkv", None, None)),
            DEFAULT_TRANSCODE_PRESET
        );
    }

    #[test]
    fn presets_are_typed_and_reject_unknown_names() {
        let copy = preset_by_name(REMUX_PRESET).unwrap();
        assert_eq!(copy.video_codec, "copy");
        assert_eq!(copy.audio_codec, "copy");

        let copy_audio = preset_by_name(REMUX_PRESET_AAC).unwrap();
        assert_eq!(copy_audio.video_codec, "copy");
        assert_eq!(copy_audio.audio_codec, "aac");

        assert!(preset_by_name(DEFAULT_TRANSCODE_PRESET).is_some());
        assert!(preset_by_name("nope").is_none());
    }
    #[test]
    fn playback_mode_matches_the_api_policy() {
        assert_eq!(
            playback_mode(Some("h264"), Some("aac"), "a.mp4"),
            PlaybackMode::Direct
        );
        assert_eq!(
            playback_mode(Some("h264"), Some("mp3"), "a.mp4"),
            PlaybackMode::Direct
        );
        assert_eq!(
            playback_mode(Some("h264"), Some("aac"), "a.mkv"),
            PlaybackMode::Remux
        );
        assert_eq!(
            playback_mode(Some("h264"), Some("flac"), "a.mkv"),
            PlaybackMode::Transcode
        );
        assert_eq!(
            playback_mode(Some("hevc"), Some("aac"), "a.mkv"),
            PlaybackMode::Transcode
        );
        assert_eq!(
            playback_mode(None, None, "a.mkv"),
            PlaybackMode::Transcode
        );
    }

}
