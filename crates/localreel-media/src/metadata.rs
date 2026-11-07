//! Optional external metadata.
//!
//! A usable library does not need this: filenames, thumbnails and the series
//! parser already give a complete experience. Adding posters and descriptions
//! is opt-in (`[metadata] provider = "tmdb"`), and a failed lookup must never
//! make a scan or a playback worse — every error here is a warning.
//!
//! The provider is behind a trait so the matching rules can be tested without a
//! network, and so a plugin-backed provider can be dropped in later (ADR-006's
//! extension seam).

use std::io::Read;
use std::time::Duration;

use localreel_kernel::config::{MetadataConfig, MetadataProvider as ConfiguredProvider};

/// Below this the match is discarded: a wrong poster is worse than no poster.
pub const CONFIDENCE_THRESHOLD: f64 = 0.8;

const TMDB_API: &str = "https://api.themoviedb.org/3";
const TMDB_IMAGE: &str = "https://image.tmdb.org/t/p/w500";
const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);

/// What to look up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataQuery {
    /// Cleaned title (no extension, no release tags).
    pub title: String,
    pub year: Option<i32>,
    /// True for an episode: look the show up, not the episode title.
    pub series: bool,
}

/// One candidate returned by a provider.
#[derive(Debug, Clone, PartialEq)]
pub struct MetadataCandidate {
    pub provider_id: String,
    pub title: String,
    pub overview: Option<String>,
    pub release_date: Option<String>,
    pub vote_average: Option<f64>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub confidence: f64,
}

/// Anything that can answer "what is this file".
pub trait MetadataProvider: Send + Sync {
    fn name(&self) -> &'static str;

    /// The best candidate for `query`, or `None` when nothing matched.
    fn search(&self, query: &MetadataQuery) -> Result<Option<MetadataCandidate>, String>;

    /// Download a poster or backdrop.
    fn download(&self, url: &str) -> Result<Vec<u8>, String>;
}

/// Build the provider the configuration asks for.
///
/// `tmdb` without an API key degrades to "no provider" instead of failing at
/// startup: a missing key is a configuration mistake, not a reason to stop.
pub fn provider_from_config(config: &MetadataConfig) -> Option<Box<dyn MetadataProvider>> {
    match config.provider {
        ConfiguredProvider::None => None,
        ConfiguredProvider::Tmdb => {
            let key = config.api_key.as_deref()?.trim();
            if key.is_empty() {
                return None;
            }
            Some(Box::new(TmdbProvider::new(
                key.to_string(),
                config.language.clone(),
            )))
        }
    }
}

/// The title a filename wants to be.
///
/// `The.Matrix.1999.1080p.BluRay.x264-GROUP.mkv` becomes
/// `("The Matrix", Some(1999))`. Release tags are stripped so the search is
/// about the film, not about the rip.
pub fn clean_title(filename: &str) -> (String, Option<i32>) {
    let without_extension = filename.rsplit_once('.').map_or(filename, |(stem, _)| stem);
    let mut text = without_extension.replace(['.', '_'], " ");

    // The *last* four-digit year is the release year: "Blade Runner 2049 2017"
    // is the 2017 film, and the 2049 belongs to the title.
    let mut year = None;
    if let Some(index) = find_last_year(&text) {
        year = text[index..index + 4].parse::<i32>().ok();
        text.truncate(index);
    }

    let mut words: Vec<&str> = text.split_whitespace().collect();
    // Release junk always trails the title, so the first strong marker ends it.
    // Only unambiguous markers qualify: a film called "Web of Lies" must keep
    // its title.
    if let Some(index) = words
        .iter()
        .position(|word| is_release_marker(&word.to_ascii_lowercase()))
    {
        words.truncate(index);
    }
    let cleaned: Vec<&str> = words
        .into_iter()
        .filter(|word| !is_release_tag(&word.to_ascii_lowercase()))
        .collect();
    let title = cleaned.join(" ").trim().to_string();
    (
        title
            .trim_end_matches(['(', '[', '{', '-', ' '])
            .trim()
            .to_string(),
        year,
    )
}

/// Tokens that only ever appear in a scene name's tail.
fn is_release_marker(word: &str) -> bool {
    const MARKERS: &[&str] = &[
        "2160p", "1080p", "720p", "480p", "4k", "uhd", "hdr", "hdr10", "remux", "bluray",
        "blu-ray", "brrip", "bdrip", "webrip", "web-dl", "webdl", "hdtv", "dvdrip", "x264", "x265",
        "h264", "h265", "hevc", "avc", "10bit", "8bit", "truehd", "atmos", "dtshd",
    ];
    MARKERS.contains(&word)
        || word.starts_with("x264-")
        || word.starts_with("x265-")
        || word.starts_with("h264-")
        || word.starts_with("h265-")
        || word.starts_with("hevc-")
}

fn find_last_year(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut found = None;
    let mut index = 0;
    while index + 4 <= bytes.len() {
        let candidate = &text[index..index + 4];
        let looks_like_a_year = candidate.starts_with("19") || candidate.starts_with("20");
        let is_word_start = index == 0 || !bytes[index - 1].is_ascii_alphanumeric();
        let is_word_end = index + 4 == bytes.len() || !bytes[index + 4].is_ascii_alphanumeric();
        if looks_like_a_year
            && is_word_start
            && is_word_end
            && candidate.chars().all(|ch| ch.is_ascii_digit())
        {
            found = Some(index);
            index += 4;
            continue;
        }
        index += 1;
    }
    found
}

fn is_release_tag(word: &str) -> bool {
    const TAGS: &[&str] = &[
        "2160p", "1080p", "720p", "480p", "4k", "uhd", "hdr", "hdr10", "dv", "remux", "bluray",
        "blu-ray", "brrip", "bdrip", "webrip", "web-dl", "webdl", "web", "hdtv", "dvdrip", "dvd",
        "x264", "x265", "h264", "h265", "hevc", "avc", "10bit", "8bit", "aac", "ac3", "dts",
        "dtshd", "truehd", "atmos", "flac", "mp3", "proper", "repack", "extended", "uncut",
        "internal", "limited", "multi", "dual", "subs", "sub", "eng", "chi", "chs", "cht",
    ];
    TAGS.contains(&word)
        // Scene groups are glued to the codec (`x264-GROUP`): the first half is
        // already a tag, so drop the whole word.
        || word.starts_with("x264-")
        || word.starts_with("x265-")
        || word.starts_with("h264-")
        || word.starts_with("h265-")
        || word.starts_with("hevc-")
}

/// How sure the match is, in 0..1.
///
/// Deliberately strict: an exact title is 1.0, a title the candidate merely
/// contains is 0.8, anything looser is discarded.
pub fn confidence(requested: &str, candidate: &str) -> f64 {
    let requested = normalize(requested);
    let candidate = normalize(candidate);
    if requested.is_empty() || candidate.is_empty() {
        return 0.0;
    }
    if requested == candidate {
        return 1.0;
    }
    // "The Matrix" vs "The Matrix Reloaded" must not count as a match.
    if candidate.starts_with(&requested) || requested.starts_with(&candidate) {
        let (longer, shorter) = if candidate.len() >= requested.len() {
            (&candidate, &requested)
        } else {
            (&requested, &candidate)
        };
        let ratio = shorter.len() as f64 / longer.len() as f64;
        return if ratio >= 0.75 { 0.8 } else { 0.5 };
    }
    0.0
}

fn normalize(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect()
}

/// The TMDB client.
pub struct TmdbProvider {
    api_key: String,
    language: String,
    agent: ureq::Agent,
}

impl TmdbProvider {
    pub fn new(api_key: String, language: String) -> Self {
        Self {
            api_key,
            language,
            agent: ureq::AgentBuilder::new().timeout(NETWORK_TIMEOUT).build(),
        }
    }

    fn search_path(&self, query: &MetadataQuery) -> String {
        let kind = if query.series { "tv" } else { "movie" };
        let mut path = format!(
            "{TMDB_API}/search/{kind}?api_key={}&language={}&query={}",
            self.api_key,
            urlencode(&self.language),
            urlencode(&query.title)
        );
        if !query.series {
            if let Some(year) = query.year {
                path.push_str(&format!("&year={year}"));
            }
        }
        path
    }
}

impl MetadataProvider for TmdbProvider {
    fn name(&self) -> &'static str {
        "tmdb"
    }

    fn search(&self, query: &MetadataQuery) -> Result<Option<MetadataCandidate>, String> {
        let path = self.search_path(query);
        let response = self
            .agent
            .get(&path)
            .call()
            .map_err(|error| format!("TMDB request failed: {error}"))?;
        let mut body = String::new();
        response
            .into_reader()
            .take(4 * 1024 * 1024)
            .read_to_string(&mut body)
            .map_err(|error| format!("TMDB response could not be read: {error}"))?;

        let Some(mut candidate) = parse_tmdb_search(&body)? else {
            return Ok(None);
        };
        candidate.confidence = confidence(&query.title, &candidate.title);
        if candidate.confidence < CONFIDENCE_THRESHOLD {
            return Ok(None);
        }
        Ok(Some(candidate))
    }

    fn download(&self, url: &str) -> Result<Vec<u8>, String> {
        if !url.starts_with("https://image.tmdb.org/") {
            return Err("refusing to download a poster from an unexpected host".to_string());
        }
        let response = self
            .agent
            .get(url)
            .call()
            .map_err(|error| format!("poster download failed: {error}"))?;
        let mut bytes = Vec::new();
        response
            .into_reader()
            .take(8 * 1024 * 1024)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("poster could not be read: {error}"))?;
        Ok(bytes)
    }
}

/// Parse the first result of a TMDB search.
///
/// Pure, so the JSON contract is tested without a network. `title` and
/// `first_air_date` are the TV spellings of `name`/`release_date`.
pub fn parse_tmdb_search(body: &str) -> Result<Option<MetadataCandidate>, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("invalid TMDB JSON: {error}"))?;
    let Some(first) = value["results"]
        .as_array()
        .and_then(|results| results.first())
    else {
        return Ok(None);
    };
    let Some(id) = first["id"].as_i64() else {
        return Ok(None);
    };
    let title = first["title"]
        .as_str()
        .or_else(|| first["name"].as_str())
        .unwrap_or_default()
        .to_string();
    if title.is_empty() {
        return Ok(None);
    }
    let image = |key: &str| {
        first[key]
            .as_str()
            .filter(|path| !path.is_empty())
            .map(|path| format!("{TMDB_IMAGE}{path}"))
    };
    Ok(Some(MetadataCandidate {
        provider_id: id.to_string(),
        title,
        overview: first["overview"]
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        release_date: first["release_date"]
            .as_str()
            .or_else(|| first["first_air_date"].as_str())
            .filter(|text| !text.is_empty())
            .map(str::to_string),
        vote_average: first["vote_average"].as_f64(),
        poster_url: image("poster_path"),
        backdrop_url: image("backdrop_path"),
        confidence: 0.0,
    }))
}

/// Minimal percent-encoding for query values (space, `&`, `=`, non-ASCII).
fn urlencode(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                output.push(*byte as char)
            }
            _ => output.push_str(&format!("%{byte:02X}")),
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scene_names_become_titles_and_years() {
        assert_eq!(
            clean_title("The.Matrix.1999.1080p.BluRay.x264-GROUP.mkv"),
            ("The Matrix".to_string(), Some(1999))
        );
        assert_eq!(
            clean_title("Blade_Runner_2049_2017_2160p_HDR_10bit_x265.mkv"),
            // The last year token is the release year; 2049 is part of the title.
            ("Blade Runner 2049".to_string(), Some(2017))
        );
        assert_eq!(
            clean_title("Some Movie (2003) [1080p].mp4"),
            ("Some Movie".to_string(), Some(2003))
        );
        // A name with no year keeps its whole title.
        assert_eq!(
            clean_title("Holiday.Tape.mkv"),
            ("Holiday Tape".to_string(), None)
        );
    }

    #[test]
    fn release_tags_are_dropped() {
        let (title, _) = clean_title("Show.S01E02.1080p.WEB-DL.AAC2.0.H.264.mkv");
        assert_eq!(title, "Show S01E02");
        assert!(!title.contains("1080p"));
        assert!(!title.to_ascii_lowercase().contains("web"));
    }

    #[test]
    fn confidence_rejects_a_different_film_in_the_same_series() {
        assert_eq!(confidence("The Matrix", "The Matrix"), 1.0);
        assert_eq!(confidence("the matrix", "The Matrix"), 1.0);
        assert_eq!(confidence("Alien", "Alien Resurrection"), 0.5);
        assert_eq!(confidence("Dune", "Dune"), 1.0);
        assert_eq!(confidence("", "Dune"), 0.0);
        assert_eq!(confidence("Completely Different", "Dune"), 0.0);
    }

    #[test]
    fn a_movie_result_parses_into_a_candidate() {
        let body = r#"{
            "results": [
                {"id": 603, "title": "The Matrix", "overview": "A hacker learns the truth.",
                 "release_date": "1999-03-31", "vote_average": 8.2,
                 "poster_path": "/abc.jpg", "backdrop_path": "/bg.jpg"}
            ]
        }"#;
        let candidate = parse_tmdb_search(body).unwrap().unwrap();
        assert_eq!(candidate.provider_id, "603");
        assert_eq!(candidate.title, "The Matrix");
        assert_eq!(
            candidate.poster_url.as_deref(),
            Some("https://image.tmdb.org/t/p/w500/abc.jpg")
        );
        assert_eq!(candidate.release_date.as_deref(), Some("1999-03-31"));
        assert_eq!(candidate.vote_average, Some(8.2));
    }

    #[test]
    fn a_tv_result_uses_its_own_field_names() {
        let body = r#"{"results": [{"id": 1399, "name": "Game of Thrones",
            "first_air_date": "2011-04-17"}]}"#;
        let candidate = parse_tmdb_search(body).unwrap().unwrap();
        assert_eq!(candidate.title, "Game of Thrones");
        assert_eq!(candidate.release_date.as_deref(), Some("2011-04-17"));
        assert_eq!(candidate.poster_url, None);
    }

    #[test]
    fn an_empty_or_malformed_result_is_not_an_error() {
        assert!(parse_tmdb_search(r#"{"results": []}"#).unwrap().is_none());
        assert!(parse_tmdb_search(r#"{"results": [{"overview": "x"}]}"#)
            .unwrap()
            .is_none());
        assert!(parse_tmdb_search("not json").is_err());
    }

    #[test]
    fn the_query_is_url_encoded() {
        assert_eq!(urlencode("The Matrix"), "The%20Matrix");
        assert_eq!(urlencode("你好"), "%E4%BD%A0%E5%A5%BD");
        assert_eq!(urlencode("a&b=c"), "a%26b%3Dc");
    }

    #[test]
    fn a_provider_without_a_key_is_not_built() {
        let mut config = MetadataConfig {
            provider: ConfiguredProvider::Tmdb,
            api_key: None,
            language: "en-US".into(),
        };
        assert!(provider_from_config(&config).is_none());
        config.api_key = Some("  ".into());
        assert!(provider_from_config(&config).is_none());
        config.api_key = Some("secret".into());
        assert!(provider_from_config(&config).is_some());
    }
}
