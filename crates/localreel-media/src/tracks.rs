//! Track helpers: language normalization and default-track selection.

/// Map the language tags real containers carry onto ISO 639-1 when possible.
///
/// ffprobe reports whatever the muxer stored: `chi`, `zho`, `zh-CN`, `und`. The
/// player wants something it can show and compare, so the common cases are
/// normalized and anything unknown is passed through lowercased.
pub fn normalize_language(raw: &str) -> Option<String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() || value == "und" || value == "unknown" {
        return None;
    }
    let normalized = match value.as_str() {
        // ISO 639-2 bibliographic codes real muxers write.
        "chi" | "zho" | "zh" | "zh-cn" | "zh-hans" | "chs" | "cmn" => "zh",
        "cht" | "zh-tw" | "zh-hk" | "zh-hant" => "zh-Hant",
        "eng" | "en" | "en-us" | "en-gb" => "en",
        "jpn" | "ja" | "jp" => "ja",
        "kor" | "ko" => "ko",
        "fre" | "fra" | "fr" => "fr",
        "ger" | "deu" | "de" => "de",
        "spa" | "es" => "es",
        "ita" | "it" => "it",
        "rus" | "ru" => "ru",
        "por" | "pt" | "pt-br" => "pt",
        "hin" | "hi" => "hi",
        "ara" | "ar" => "ar",
        "tha" | "th" => "th",
        "vie" | "vi" => "vi",
        other => return Some(other.to_string()),
    };
    Some(normalized.to_string())
}

/// Whether `language` is a Chinese tag, used to pick a sensible default.
pub fn is_chinese(language: Option<&str>) -> bool {
    matches!(language, Some(tag) if tag.starts_with("zh"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_letter_codes_become_two_letter_ones() {
        assert_eq!(normalize_language("chi").as_deref(), Some("zh"));
        assert_eq!(normalize_language("zho").as_deref(), Some("zh"));
        assert_eq!(normalize_language("eng").as_deref(), Some("en"));
        assert_eq!(normalize_language("JPN").as_deref(), Some("ja"));
        assert_eq!(normalize_language("ger").as_deref(), Some("de"));
    }

    #[test]
    fn regional_tags_keep_their_script_when_it_matters() {
        assert_eq!(normalize_language("zh-CN").as_deref(), Some("zh"));
        assert_eq!(normalize_language("zh-Hans").as_deref(), Some("zh"));
        assert_eq!(normalize_language("zh-TW").as_deref(), Some("zh-Hant"));
        assert_eq!(normalize_language("zh-Hant").as_deref(), Some("zh-Hant"));
        assert_eq!(normalize_language("en-US").as_deref(), Some("en"));
    }

    #[test]
    fn unknown_and_undefined_languages_are_not_invented() {
        assert_eq!(normalize_language("und"), None);
        assert_eq!(normalize_language("  "), None);
        assert_eq!(normalize_language("tlh").as_deref(), Some("tlh"));
    }

    #[test]
    fn chinese_detection_covers_both_scripts() {
        assert!(is_chinese(Some("zh")));
        assert!(is_chinese(Some("zh-Hant")));
        assert!(!is_chinese(Some("en")));
        assert!(!is_chinese(None));
    }
}
