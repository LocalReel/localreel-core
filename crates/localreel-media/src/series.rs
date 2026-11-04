//! Series and episode derivation for the feed.
//!
//! Real libraries are not tidy: an episode can be `[Group] Title [12][1080p]`,
//! `Title - 01v2`, `Title 02话`, `Title S02E05`, `Title EP05` or even
//! `01 更多资源关注公众号.mp4`. The only signal that survives all of them is
//! the **parent directory** — `SPs` folders and season folders included. The
//! parser is therefore directory-first:
//!
//! * `series_key`  — normalized parent directory (lowercase, forward slashes);
//! * `series_title` — the directory name cleaned of tags; a season folder
//!   (`第二季`, `S2`, `Season 2`) is prefixed with its parent, so the feed can
//!   label it "Series · 第二季" instead of a bare "第二季";
//! * `episode`     — parsed from the filename when a pattern matches;
//! * `is_extra`    — SP/OVA/NCOP/NCED/Menu/CM/MV/PV/sample clips, and anything
//!   inside an `SPs`/`Extras`/`OVA` directory.
//!
//! It is deliberately conservative: no `episode` is better than a wrong one,
//! because the feed uses it to decide "next episode". Release CRC tags like
//! `[E4437ED5]` must never become episode 443.

use regex::Regex;
use std::sync::LazyLock;

/// Episode numbers above this are almost always resolution or bit-depth noise.
const MAX_EPISODE: i64 = 999;

/// Derived series metadata for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesInfo {
    pub key: String,
    pub title: String,
    pub episode: Option<i64>,
    pub is_extra: bool,
}

/// Derive series metadata from a canonical file path and its file name.
pub fn derive(canonical_path: &str, filename: &str) -> SeriesInfo {
    let normalized = canonical_path.replace('\\', "/");
    let normalized = normalized.strip_prefix("//?/").unwrap_or(&normalized);
    let parent = normalized
        .rsplit_once('/')
        .map(|(dir, _)| dir)
        .unwrap_or_default();
    let key = parent.to_ascii_lowercase();
    let dir_name = parent.rsplit('/').next().unwrap_or_default();
    let grandparent = parent
        .rsplit_once('/')
        .map(|(dir, _)| dir.rsplit('/').next().unwrap_or_default())
        .unwrap_or_default();

    let mut title = clean_title(dir_name);
    if is_season_dir(dir_name) {
        let parent_title = clean_title(grandparent);
        if !parent_title.is_empty() {
            title = format!("{parent_title} · {title}");
        }
    }
    if title.is_empty() {
        title = clean_title(
            filename
                .rsplit_once('.')
                .map(|(stem, _)| stem)
                .unwrap_or(filename),
        );
    }

    SeriesInfo {
        key,
        title,
        episode: parse_episode(filename),
        is_extra: is_extra(filename, dir_name),
    }
}

/// Parse an episode number from a release-style file name.
pub fn parse_episode(filename: &str) -> Option<i64> {
    let stem = filename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(filename);
    // Technical tokens would otherwise look like episode numbers (`1080p`).
    let stem = RESOLUTION.replace_all(stem, " ");
    let stem = stem.as_ref();
    if let Some(episode) = number_from(stem, &SXXEXX, 1) {
        return Some(episode);
    }
    if let Some(episode) = number_from(stem, &EPISODE_TAG, 1) {
        return Some(episode);
    }
    if let Some(episode) = number_from(stem, &CJK_EPISODE, 1) {
        return Some(episode);
    }
    if let Some(episode) = number_from(stem, &DASH_EPISODE, 1) {
        return Some(episode);
    }
    if let Some(episode) = number_from(stem, &LEADING_EPISODE, 1) {
        return Some(episode);
    }
    if let Some(episode) = number_from(stem, &BRACKET_EPISODE, 1) {
        return Some(episode);
    }
    number_from(stem, &TRAILING_EPISODE, 1)
}

fn number_from(text: &str, regex: &Regex, group: usize) -> Option<i64> {
    regex
        .captures(text)
        .and_then(|captures| captures.get(group))
        .and_then(|value| value.as_str().parse::<i64>().ok())
        .filter(|value| (1..=MAX_EPISODE).contains(value))
}

fn is_extra(filename: &str, dir_name: &str) -> bool {
    let stem = filename
        .rsplit_once('.')
        .map(|(stem, _)| stem)
        .unwrap_or(filename);
    EXTRA_DIR.is_match(dir_name.trim()) || EXTRA_MARKER.is_match(stem)
}

fn is_season_dir(dir_name: &str) -> bool {
    SEASON_DIR.is_match(dir_name.trim())
}

/// Trim release tags and technical noise from a directory or file stem.
pub fn clean_title(raw: &str) -> String {
    let mut text = raw.replace(['_', '.'], " ");
    loop {
        let mut next = String::with_capacity(text.len());
        let mut depth = 0i32;
        for character in text.chars() {
            match character {
                '[' | '(' | '【' => depth += 1,
                ']' | ')' | '】' => depth = (depth - 1).max(0),
                _ if depth == 0 => next.push(character),
                _ => {}
            }
        }
        let stable = next.trim() == text.trim();
        text = next;
        if stable {
            break;
        }
    }
    for noise in [
        "1080p", "720p", "2160p", "4k", "8bit", "10bit", "x264", "x265", "h264", "h265", "hevc",
        "aac", "flac", "bluray", "bdrip", "web-dl", "webrip",
    ] {
        text = text
            .split_whitespace()
            .filter(|word| !word.eq_ignore_ascii_case(noise))
            .collect::<Vec<_>>()
            .join(" ");
    }
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(['-', ' ', '·'])
        .to_string()
}

static RESOLUTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b\d{3,4}[pi]\b|\b\d{3,4}x\d{3,4}\b").unwrap());
static SXXEXX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)s\d{1,2}\s*e(\d{1,3})").unwrap());
static EPISODE_TAG: LazyLock<Regex> = LazyLock::new(|| {
    // The trailing boundary keeps CRCs like `[E4437ED5]` out: `443` is followed
    // by a hex digit, not by a separator or the end of the name.
    Regex::new(r"(?i)(?:^|[\s._\-])e(?:p|pisode)?[ ._-]?(\d{1,3})(?:v\d)?(?:[^0-9a-z]|$)").unwrap()
});
static CJK_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:第\s*)?(\d{1,3})\s*[集话話]").unwrap());
static DASH_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[-–]\s*(\d{1,3})(?:v\d)?(?:[^0-9]|$)").unwrap());
static LEADING_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d{1,3})(?:v\d)?[\s._\-]").unwrap());
static BRACKET_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\[\(【](\d{1,3})(?:v\d)?[\]\)】]").unwrap());
static TRAILING_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|[\s_\-])(\d{1,3})(?:v\d)?$").unwrap());
static EXTRA_MARKER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b(sp|ova|ncop|nced|special|menu|cm|mv|pv|sample|trailer)[\s._-]?\d{0,3}\b")
        .unwrap()
});
static EXTRA_DIR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^(sps?|specials?|extras?|ova|bonus|menus?)$").unwrap());
static SEASON_DIR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^(第\s*[一二三四五六七八九十0-9]+\s*[季部篇]|s\d{1,2}|season\s*\d{1,2})$")
        .unwrap()
});

#[cfg(test)]
mod tests {
    use super::*;

    fn episode(name: &str) -> Option<i64> {
        parse_episode(name)
    }

    #[test]
    fn parses_release_style_names() {
        assert_eq!(
            episode("[VCB-Studio] Netoge no Yome [12][Ma10p_1080p][x265_flac_aac].mkv"),
            Some(12)
        );
        assert_eq!(
            episode("[FFF] Love Live! Sunshine!! - 01v2 [ED76D13C].mkv"),
            Some(1)
        );
        assert_eq!(
            episode("[Lilith-Raws] Title - 07 [Baha][WEB-DL][1080p][AVC AAC].mp4"),
            Some(7)
        );
        assert_eq!(episode("FLIP FLAPPERS 02话.mp4"), Some(2));
        assert_eq!(episode("Title 第 5 话.mp4"), Some(5));
        assert_eq!(episode("Title S02E05.mkv"), Some(5));
        assert_eq!(episode("Title EP05.mp4"), Some(5));
        assert_eq!(episode("Title - 12.mkv"), Some(12));
        assert_eq!(episode("Title 12v2.mkv"), Some(12));
        // Spam-suffixed downloads keep a leading episode number.
        assert_eq!(episode("01 更多资源关注公众号：娃娃电影.mp4"), Some(1));
    }

    #[test]
    fn rejects_technical_numbers() {
        assert_eq!(episode("[Group] Movie [1080p][x264].mp4"), None);
        assert_eq!(episode("Title 2160p HDR.mkv"), None);
        assert_eq!(episode("Title - 1080p.mkv"), None);
        assert_eq!(episode("Title [1920x1080].mkv"), None);
        assert_eq!(episode("plain.mp4"), None);
        assert_eq!(episode("中文 名字 带空格.mp4"), None);
    }

    #[test]
    fn crc_tags_are_not_episodes() {
        assert_eq!(
            episode("[FFF] Love Live! Sunshine!! - 06v2 [E4437ED5].mkv"),
            Some(6)
        );
        assert_eq!(episode("[Group] Title [12][A1B2C3D4].mkv"), Some(12));
        assert_eq!(episode("[Group] Title [A1B2C3D4].mkv"), None);
    }

    #[test]
    fn derives_series_from_the_directory() {
        let info = derive(
            r"\\?\D:\library\flip flapper\FLIP FLAPPERS 02话.mp4",
            "FLIP FLAPPERS 02话.mp4",
        );
        assert_eq!(info.key, "d:/library/flip flapper");
        assert_eq!(info.title, "flip flapper");
        assert_eq!(info.episode, Some(2));
        assert!(!info.is_extra);
    }

    #[test]
    fn season_folders_get_their_parent_in_the_title() {
        let info = derive(
            r"\\?\D:\library\某动画\第二季\01 更多资源关注公众号.mp4",
            "01 更多资源关注公众号.mp4",
        );
        assert_eq!(info.title, "某动画 · 第二季");
        assert_eq!(info.episode, Some(1));
    }

    #[test]
    fn flags_specials() {
        assert!(is_extra("[Group] Title NCOP.mkv", "Title"));
        assert!(is_extra("Title SP01.mkv", "Title"));
        assert!(is_extra("OVA - Title.mkv", "Title"));
        assert!(is_extra("CM.mkv", "SPs"));
        assert!(is_extra("Menu.mkv", "SPs"));
        assert!(!is_extra("[VCB-Studio] Title [12][1080p].mkv", "Title"));
    }

    #[test]
    fn cleans_release_titles() {
        assert_eq!(
            clean_title("[VCB-Studio] Netoge no Yome [Ma10p_1080p]"),
            "Netoge no Yome"
        );
        assert_eq!(clean_title("flip flapper"), "flip flapper");
        assert_eq!(clean_title("【风车动漫】Title"), "Title");
    }
}
