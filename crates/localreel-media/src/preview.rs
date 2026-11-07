//! Progress-bar previews: sprite geometry and the WebVTT that indexes it.
//!
//! The player needs one image plus a cue list that maps a time range to a region
//! of that image. Both are derived here, away from ffmpeg, so the maths is
//! unit-tested rather than eyeballed.

use localreel_kernel::media_ops::{PREVIEW_COLUMNS, PREVIEW_ROWS, PREVIEW_TILE_WIDTH};

/// One cue of the preview track.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewCue {
    /// Start of the range, in seconds.
    pub start: f64,
    /// End of the range, in seconds.
    pub end: f64,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Tile size for a source of `width`x`height`.
///
/// The sheet keeps the aspect ratio (`scale=160:-1`), and ffmpeg's `-1` rounds to
/// an even number; reproducing that here keeps the `xywh` rectangles aligned
/// with the pixels.
pub fn tile_size(width: Option<i32>, height: Option<i32>) -> (u32, u32) {
    let width = width
        .filter(|value| *value > 0)
        .unwrap_or(PREVIEW_TILE_WIDTH as i32);
    let height = height.filter(|value| *value > 0).unwrap_or(90);
    let scaled = (f64::from(PREVIEW_TILE_WIDTH) * f64::from(height) / f64::from(width)).round();
    let mut scaled = scaled.max(1.0) as u32;
    if scaled % 2 == 1 {
        scaled += 1;
    }
    (PREVIEW_TILE_WIDTH, scaled)
}

/// Cues covering the whole file, one per sprite tile.
///
/// A file shorter than one interval gets a single cue for its whole length.
pub fn preview_cues(duration: f64, interval: f64, tile: (u32, u32)) -> Vec<PreviewCue> {
    let (width, height) = tile;
    let slots = PREVIEW_COLUMNS * PREVIEW_ROWS;
    let mut cues = Vec::with_capacity(slots as usize);
    if duration <= 0.0 {
        return cues;
    }
    let interval = interval.max(0.001);
    for index in 0..slots {
        let start = f64::from(index) * interval;
        if start >= duration {
            break;
        }
        let end = ((f64::from(index) + 1.0) * interval).min(duration);
        let column = index % PREVIEW_COLUMNS;
        let row = index / PREVIEW_COLUMNS;
        cues.push(PreviewCue {
            start,
            end,
            x: column * width,
            y: row * height,
            width,
            height,
        });
    }
    cues
}

/// Render the WebVTT document the player fetches.
///
/// `sprite_url` is a root-absolute URL, so the document works from any endpoint.
pub fn preview_vtt(sprite_url: &str, cues: &[PreviewCue]) -> String {
    let mut output = String::from("WEBVTT\n\n");
    for cue in cues {
        output.push_str(&format!(
            "{} --> {}\n{}#xywh={},{},{},{}\n\n",
            timestamp(cue.start),
            timestamp(cue.end),
            sprite_url,
            cue.x,
            cue.y,
            cue.width,
            cue.height
        ));
    }
    output
}

/// `HH:MM:SS.mmm`, the only timestamp form WebVTT accepts.
fn timestamp(seconds: f64) -> String {
    let total_ms = (seconds.max(0.0) * 1000.0).round() as u64;
    let millis = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let secs = total_secs % 60;
    let minutes = (total_secs / 60) % 60;
    let hours = total_secs / 3600;
    format!("{hours:02}:{minutes:02}:{secs:02}.{millis:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_use_the_webvtt_shape() {
        assert_eq!(timestamp(0.0), "00:00:00.000");
        assert_eq!(timestamp(1.5), "00:00:01.500");
        assert_eq!(timestamp(61.25), "00:01:01.250");
        assert_eq!(timestamp(3725.4), "01:02:05.400");
        // Rounding must not produce `:60`.
        assert_eq!(timestamp(59.9996), "00:01:00.000");
    }

    #[test]
    fn tiles_keep_the_source_aspect_ratio() {
        assert_eq!(tile_size(Some(1920), Some(1080)), (160, 90));
        assert_eq!(tile_size(Some(640), Some(360)), (160, 90));
        // 4:3 stays 4:3, rounded to an even height.
        assert_eq!(tile_size(Some(640), Some(480)), (160, 120));
        // Unknown dimensions fall back to 16:9.
        assert_eq!(tile_size(None, None), (160, 90));
        // Odd results are rounded up to an even number, like ffmpeg's `-1`.
        let (_, height) = tile_size(Some(1080), Some(607));
        assert_eq!(height % 2, 0);
    }

    #[test]
    fn cues_cover_the_file_without_overrunning_it() {
        // Ten minutes with a six-second interval: 100 slots, last one clipped.
        let cues = preview_cues(600.0, 6.0, (160, 90));
        assert_eq!(cues.len(), 100);
        assert_eq!(cues[0].start, 0.0);
        assert_eq!(cues[0].end, 6.0);
        assert_eq!(cues[0].x, 0);
        assert_eq!(cues[0].y, 0);
        assert_eq!(cues.last().unwrap().end, 600.0);

        // Shorter than the interval: one cue for the whole file.
        let short = preview_cues(4.0, 10.0, (160, 90));
        assert_eq!(short.len(), 1);
        assert_eq!(short[0].end, 4.0);

        // Zero duration produces nothing rather than an empty cue.
        assert!(preview_cues(0.0, 10.0, (160, 90)).is_empty());
    }

    #[test]
    fn cues_walk_the_grid_row_by_row() {
        let cues = preview_cues(1000.0, 10.0, (160, 90));
        assert_eq!(cues[9].x, 9 * 160);
        assert_eq!(cues[9].y, 0);
        assert_eq!(cues[10].x, 0);
        assert_eq!(cues[10].y, 90, "the eleventh frame starts a new row");
        assert_eq!(cues[99].x, 9 * 160);
        assert_eq!(cues[99].y, 9 * 90);
    }

    #[test]
    fn the_document_is_valid_webvtt() {
        let cues = preview_cues(20.0, 10.0, (160, 90));
        let text = preview_vtt("/previews/abc.jpg", &cues);
        assert!(text.starts_with("WEBVTT\n\n"));
        assert!(text.contains("00:00:00.000 --> 00:00:10.000"));
        assert!(text.contains("/previews/abc.jpg#xywh=0,0,160,90"));
        assert!(text.contains("/previews/abc.jpg#xywh=160,0,160,90"));
        assert!(text.ends_with("\n\n"));
    }
}
