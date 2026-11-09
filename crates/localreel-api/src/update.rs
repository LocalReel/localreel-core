//! Remote release lookup behind the admin "update" card.
//!
//! The server never downloads or runs anything: it reads the latest release
//! feed, compares versions, and hands the operator the release page and the
//! installer asset URL. Silent self-update without signatures would be a
//! supply-chain hole, so this is a tracker with a link, not an installer.

use std::time::Duration;

/// Public GitHub API: the latest published release of the product repository.
pub const DEFAULT_FEED: &str = "https://api.github.com/repos/LocalReel/LocalReel/releases/latest";

/// The whole check must never hold an admin request for longer than this.
const TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Normalized: no leading `v`.
    pub version: String,
    /// Human page for the release.
    pub url: String,
    pub published_at: Option<String>,
    /// Direct download of `localreel-<version>-x64-setup.exe`, when attached.
    pub setup_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CachedCheck {
    pub checked_at: i64,
    pub release: Release,
}

/// Ask the feed. Blocking; callers run it on a blocking thread.
pub fn fetch(feed: &str) -> Result<Release, String> {
    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let response = agent
        .get(feed)
        .set("Accept", "application/vnd.github+json")
        .set(
            "User-Agent",
            concat!("LocalReel/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|error| format!("release feed: {error}"))?;
    let body = response
        .into_string()
        .map_err(|error| format!("release feed body: {error}"))?;
    parse_release(&body)
}

/// Parse the subset of the GitHub release JSON this feature needs.
pub fn parse_release(body: &str) -> Result<Release, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("release feed is not JSON: {error}"))?;
    let tag = value
        .get("tag_name")
        .and_then(|tag| tag.as_str())
        .ok_or_else(|| "release feed has no tag_name".to_string())?;
    let url = value
        .get("html_url")
        .and_then(|url| url.as_str())
        .unwrap_or(DEFAULT_FEED)
        .to_string();
    let published_at = value
        .get("published_at")
        .and_then(|at| at.as_str())
        .map(str::to_owned);
    let setup_url = value
        .get("assets")
        .and_then(|assets| assets.as_array())
        .and_then(|assets| {
            assets.iter().find_map(|asset| {
                let name = asset.get("name").and_then(|name| name.as_str())?;
                if !name.ends_with("-setup.exe") {
                    return None;
                }
                asset
                    .get("browser_download_url")
                    .and_then(|url| url.as_str())
                    .map(str::to_owned)
            })
        });
    Ok(Release {
        version: normalize_version(tag),
        url,
        published_at,
        setup_url,
    })
}

pub fn normalize_version(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// Whether `latest` is a newer release than `current`.
///
/// Numeric components only: `0.5.10` is newer than `0.5.9`, and a pre-release
/// suffix (`0.6.0-rc1`) sorts as its base version. Good enough for "there is
/// something newer to look at", which is all this decides.
pub fn is_newer(latest: &str, current: &str) -> bool {
    let parse = |version: &str| -> Vec<u64> {
        version
            .split(['-', '+'])
            .next()
            .unwrap_or(version)
            .split('.')
            .map(|part| part.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let latest = parse(latest);
    let current = parse(current);
    for index in 0..latest.len().max(current.len()) {
        let newer = latest.get(index).copied().unwrap_or(0);
        let older = current.get(index).copied().unwrap_or(0);
        if newer != older {
            return newer > older;
        }
    }
    false
}

/// What the admin page shows. Errors are reported alongside the last good
/// check instead of turning the card into a dead end.
#[derive(Debug, Clone)]
pub struct Status {
    pub enabled: bool,
    pub current: String,
    pub latest: Option<Release>,
    pub update_available: bool,
    pub checked_at: Option<i64>,
    pub error: Option<String>,
}

impl Status {
    pub fn build(
        enabled: bool,
        current: &str,
        cached: Option<&CachedCheck>,
        error: Option<String>,
    ) -> Self {
        let latest = cached.map(|cached| cached.release.clone());
        let update_available = latest
            .as_ref()
            .map(|release| is_newer(&release.version, current))
            .unwrap_or(false);
        Self {
            enabled,
            current: current.to_string(),
            latest,
            update_available,
            checked_at: cached.map(|cached| cached.checked_at),
            error,
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "enabled": self.enabled,
            "current": self.current,
            "latest": self.latest.as_ref().map(|release| serde_json::json!({
                "version": release.version,
                "url": release.url,
                "published_at": release.published_at,
                "setup_url": release.setup_url,
            })),
            "update_available": self.update_available,
            "checked_at": self.checked_at,
            "error": self.error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(is_newer("0.5.10", "0.5.9"));
        assert!(is_newer("0.6.0", "0.5.99"));
        assert!(!is_newer("0.5.4", "0.5.4"));
        assert!(!is_newer("0.4.9", "0.5.0"));
        assert!(is_newer("v0.5.5", "0.5.4"));
    }

    #[test]
    fn parses_a_github_release() {
        let body = r#"{
            "tag_name": "v0.5.5",
            "html_url": "https://github.com/LocalReel/LocalReel/releases/tag/v0.5.5",
            "published_at": "2026-11-09T10:00:00Z",
            "assets": [
                {"name": "localreel-0.5.5-x64.msi", "browser_download_url": "https://example.invalid/app.msi"},
                {"name": "localreel-0.5.5-x64-setup.exe", "browser_download_url": "https://example.invalid/setup.exe"}
            ]
        }"#;
        let release = parse_release(body).unwrap();
        assert_eq!(release.version, "0.5.5");
        assert_eq!(
            release.setup_url.as_deref(),
            Some("https://example.invalid/setup.exe")
        );
        assert!(is_newer(&release.version, "0.5.4"));
    }

    #[test]
    fn a_release_without_the_setup_asset_still_parses() {
        let body = r#"{"tag_name": "v0.6.0", "assets": []}"#;
        let release = parse_release(body).unwrap();
        assert_eq!(release.version, "0.6.0");
        assert!(release.setup_url.is_none());
    }

    #[test]
    fn status_reports_availability() {
        let release = Release {
            version: "0.5.5".into(),
            url: "https://example.invalid".into(),
            published_at: None,
            setup_url: None,
        };
        let cached = CachedCheck {
            checked_at: 1,
            release,
        };
        let status = Status::build(true, "0.5.4", Some(&cached), None);
        assert!(status.update_available);
        assert_eq!(status.latest.unwrap().version, "0.5.5");
    }
}
