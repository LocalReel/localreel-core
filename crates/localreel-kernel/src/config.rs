use std::env;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::error::{KernelError, Result};

/// Full server configuration.
///
/// Precedence: built-in defaults → optional `localreel.toml` → `LOCALREEL_*`
/// environment variables. `LOCALREEL_CONFIG` can point at a different file.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    pub web_dir: Option<PathBuf>,
    /// Must be explicitly enabled to bind beyond loopback. The default is
    /// local-only, matching the trust model of the previous LocalReel app.
    pub allow_remote: bool,
    pub jobs: JobsConfig,
    pub media: MediaConfig,
    /// Sidecar plugins to start with the server (packages on disk).
    pub plugins: Vec<PluginConfig>,
}

/// One configured sidecar plugin.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Path to the `.lrplug` package.
    pub package: PathBuf,
    pub enabled: bool,
    /// JSON handed to the plugin via `LOCALREEL_PLUGIN_CONFIG`.
    pub config: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct JobsConfig {
    pub workers: usize,
    pub poll_interval: Duration,
    pub lease: Duration,
    pub max_attempts: i64,
    /// How often the server enqueues a `cache_gc` job. `Duration::ZERO` disables
    /// the periodic trigger (manual Admin GC still works).
    pub cache_gc_interval: Duration,
}

#[derive(Debug, Clone)]
pub struct MediaConfig {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub probe_timeout: Duration,
    pub thumbnail_timeout: Duration,
    pub transcode_timeout: Duration,
    pub thumbnail_dir: PathBuf,
    pub transcode_dir: PathBuf,
    /// HLS playlists and segments produced on demand.
    pub hls_dir: PathBuf,
    pub cache_max_bytes: u64,
    pub cache_max_age: Duration,
}

impl Default for Config {
    fn default() -> Self {
        let data_dir = PathBuf::from("data");
        Self {
            host: "127.0.0.1".to_string(),
            port: 3001,
            db_path: data_dir.join("localreel.db"),
            web_dir: None,
            allow_remote: false,
            data_dir: data_dir.clone(),
            jobs: JobsConfig {
                workers: default_worker_count(),
                poll_interval: Duration::from_millis(250),
                lease: Duration::from_secs(60),
                max_attempts: 3,
                cache_gc_interval: Duration::from_secs(24 * 60 * 60),
            },
            media: MediaConfig {
                ffmpeg: PathBuf::from("ffmpeg"),
                ffprobe: PathBuf::from("ffprobe"),
                probe_timeout: Duration::from_secs(30),
                thumbnail_timeout: Duration::from_secs(60),
                transcode_timeout: Duration::from_secs(30 * 60),
                thumbnail_dir: data_dir.join("thumbnails"),
                transcode_dir: data_dir.join("transcodes"),
                hls_dir: data_dir.join("hls"),
                cache_max_bytes: 20 * 1024 * 1024 * 1024,
                cache_max_age: Duration::from_secs(30 * 24 * 60 * 60),
            },
            plugins: Vec::new(),
        }
    }
}

impl Config {
    /// Load defaults + optional config file + environment overrides, validate,
    /// and return the file that was used (if any).
    pub fn load_with_source() -> Result<(Self, Option<PathBuf>)> {
        let mut config = Self::default();
        let source_path = find_config_file()?;

        if let Some(path) = &source_path {
            let text = std::fs::read_to_string(path).map_err(|error| {
                KernelError::Config(format!(
                    "failed to read config file {}: {error}",
                    path.display()
                ))
            })?;
            let parsed: ConfigFile = toml::from_str(&text).map_err(|error| {
                KernelError::Config(format!("invalid config file {}: {error}", path.display()))
            })?;
            config.apply_file(parsed)?;
        }

        config.apply_env()?;
        config.validate()?;
        Ok((config, source_path))
    }

    /// Convenience wrapper for callers that do not need the config origin.
    pub fn load() -> Result<Self> {
        Self::load_with_source().map(|(config, _)| config)
    }

    fn apply_file(&mut self, file: ConfigFile) -> Result<()> {
        if let Some(value) = file.server.host {
            self.host = value;
        }
        if let Some(value) = file.server.port {
            self.port = value;
        }
        if let Some(value) = file.server.allow_remote {
            self.allow_remote = value;
        }
        if let Some(value) = file.server.web_dir {
            self.web_dir = Some(value);
        }

        if let Some(dir) = file.data.dir {
            self.data_dir = dir;
            self.db_path = self.data_dir.join("localreel.db");
            self.media.thumbnail_dir = self.data_dir.join("thumbnails");
            self.media.transcode_dir = self.data_dir.join("transcodes");
            self.media.hls_dir = self.data_dir.join("hls");
        }
        if let Some(db) = file.data.db {
            self.db_path = db;
        }

        if let Some(value) = file.jobs.workers {
            self.jobs.workers = value;
        }
        if let Some(value) = file.jobs.poll_interval_ms {
            self.jobs.poll_interval = Duration::from_millis(value);
        }
        if let Some(value) = file.jobs.lease_secs {
            self.jobs.lease = Duration::from_secs(value);
        }
        if let Some(value) = file.jobs.max_attempts {
            self.jobs.max_attempts = value;
        }
        if let Some(value) = file.jobs.cache_gc_interval_secs {
            self.jobs.cache_gc_interval = Duration::from_secs(value);
        }

        if let Some(value) = file.media.ffmpeg {
            self.media.ffmpeg = value;
        }
        if let Some(value) = file.media.ffprobe {
            self.media.ffprobe = value;
        }
        if let Some(value) = file.media.probe_timeout_secs {
            self.media.probe_timeout = Duration::from_secs(value);
        }
        if let Some(value) = file.media.thumbnail_timeout_secs {
            self.media.thumbnail_timeout = Duration::from_secs(value);
        }
        if let Some(value) = file.media.transcode_timeout_secs {
            self.media.transcode_timeout = Duration::from_secs(value);
        }
        if let Some(value) = file.media.thumbnails_dir {
            self.media.thumbnail_dir = value;
        }
        if let Some(value) = file.media.transcodes_dir {
            self.media.transcode_dir = value;
        }
        if let Some(value) = file.media.hls_dir {
            self.media.hls_dir = value;
        }
        if let Some(value) = file.media.cache_max_bytes {
            self.media.cache_max_bytes = value;
        }
        if let Some(value) = file.media.cache_max_age_hours {
            self.media.cache_max_age = Duration::from_secs(value.saturating_mul(60 * 60));
        }
        if !file.plugins.is_empty() {
            self.plugins = file
                .plugins
                .into_iter()
                .map(|plugin| PluginConfig {
                    package: plugin.package,
                    enabled: plugin.enabled,
                    config: plugin.config,
                })
                .collect();
        }
        Ok(())
    }

    fn apply_env(&mut self) -> Result<()> {
        if let Some(value) = env_string("LOCALREEL_HOST") {
            self.host = value;
        }
        if let Some(value) = env_u64("LOCALREEL_PORT")? {
            self.port = u16::try_from(value)
                .map_err(|_| KernelError::Config("LOCALREEL_PORT must be <= 65535".into()))?;
        }
        if let Some(value) = env_string("LOCALREEL_DATA_DIR") {
            self.data_dir = PathBuf::from(value);
            self.db_path = self.data_dir.join("localreel.db");
            self.media.thumbnail_dir = self.data_dir.join("thumbnails");
            self.media.transcode_dir = self.data_dir.join("transcodes");
            self.media.hls_dir = self.data_dir.join("hls");
        }
        if let Some(value) = env_string("LOCALREEL_DB") {
            self.db_path = PathBuf::from(value);
        }
        if let Some(value) = env_string("LOCALREEL_WEB_DIR") {
            self.web_dir = Some(PathBuf::from(value));
        }
        if let Some(value) = env_bool("LOCALREEL_ALLOW_REMOTE")? {
            self.allow_remote = value;
        }
        if let Some(value) = env_u64("LOCALREEL_JOB_WORKERS")? {
            self.jobs.workers = usize::try_from(value)
                .map_err(|_| KernelError::Config("LOCALREEL_JOB_WORKERS is too large".into()))?;
        }
        if let Some(value) = env_u64("LOCALREEL_JOB_LEASE_SECS")? {
            self.jobs.lease = Duration::from_secs(value);
        }
        if let Some(value) = env_u64("LOCALREEL_JOB_MAX_ATTEMPTS")? {
            self.jobs.max_attempts = i64::try_from(value).map_err(|_| {
                KernelError::Config("LOCALREEL_JOB_MAX_ATTEMPTS is too large".into())
            })?;
        }
        if let Some(value) = env_u64("LOCALREEL_CACHE_GC_INTERVAL_SECS")? {
            self.jobs.cache_gc_interval = Duration::from_secs(value);
        }
        if let Some(value) = env_string("LOCALREEL_HLS_DIR") {
            self.media.hls_dir = PathBuf::from(value);
        }
        if let Some(value) = env_string("LOCALREEL_FFMPEG") {
            self.media.ffmpeg = PathBuf::from(value);
        }
        if let Some(value) = env_string("LOCALREEL_FFPROBE") {
            self.media.ffprobe = PathBuf::from(value);
        }
        if let Some(value) = env_u64("LOCALREEL_TRANSCODE_CACHE_MAX_BYTES")? {
            self.media.cache_max_bytes = value;
        }
        if let Some(value) = env_u64("LOCALREEL_TRANSCODE_CACHE_MAX_AGE_HOURS")? {
            self.media.cache_max_age = Duration::from_secs(value.saturating_mul(60 * 60));
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.port == 0 {
            return Err(KernelError::Config("port must not be 0".into()));
        }
        if !(1..=64).contains(&self.jobs.workers) {
            return Err(KernelError::Config(
                "job workers must be between 1 and 64".into(),
            ));
        }
        if !(1..=100).contains(&self.jobs.max_attempts) {
            return Err(KernelError::Config(
                "job max attempts must be between 1 and 100".into(),
            ));
        }
        if self.jobs.lease.is_zero() {
            return Err(KernelError::Config("job lease must be > 0".into()));
        }
        if self.media.ffmpeg.as_os_str().is_empty() || self.media.ffprobe.as_os_str().is_empty() {
            return Err(KernelError::Config(
                "ffmpeg and ffprobe paths must not be empty".into(),
            ));
        }
        if !self.allow_remote && !is_loopback_host(&self.host) {
            return Err(KernelError::Config(format!(
                "refusing to bind non-loopback host '{}' without LOCALREEL_ALLOW_REMOTE=1",
                self.host
            )));
        }
        for plugin in &self.plugins {
            if plugin.package.as_os_str().is_empty() {
                return Err(KernelError::Config(
                    "plugin package path must not be empty".into(),
                ));
            }
        }
        Ok(())
    }

    /// Create the data/thumbnail/transcode directories. Idempotent.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.data_dir,
            &self.media.thumbnail_dir,
            &self.media.transcode_dir,
            &self.media.hls_dir,
        ] {
            std::fs::create_dir_all(dir).map_err(|error| {
                KernelError::Config(format!(
                    "failed to create directory {}: {error}",
                    dir.display()
                ))
            })?;
        }
        Ok(())
    }

    /// Resolve the built web UI directory.
    ///
    /// An explicit `web_dir` (config/env) wins if it contains `index.html`.
    /// Otherwise try paths relative to a `cargo run` from `localreel-core`.
    /// Returns `None` when no UI has been built yet; the API still works.
    pub fn resolved_web_dir(&self) -> Option<PathBuf> {
        if let Some(dir) = &self.web_dir {
            if dir.join("index.html").is_file() {
                return Some(dir.clone());
            }
        }
        for candidate in ["../localreel-web/dist", "web/dist", "localreel-web/dist"] {
            let path = PathBuf::from(candidate);
            if path.join("index.html").is_file() {
                return Some(path);
            }
        }
        None
    }

    pub fn db_path_string(&self) -> String {
        self.db_path.to_string_lossy().into_owned()
    }
}

/// On-disk config schema. Every field is optional so a partial file only
/// overrides what it mentions.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    server: ServerFile,
    #[serde(default)]
    data: DataFile,
    #[serde(default)]
    jobs: JobsFile,
    #[serde(default)]
    media: MediaFile,
    #[serde(default)]
    plugins: Vec<PluginFile>,
}

#[derive(Debug, Deserialize)]
struct PluginFile {
    package: PathBuf,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    config: serde_json::Value,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Default, Deserialize)]
struct ServerFile {
    host: Option<String>,
    port: Option<u16>,
    allow_remote: Option<bool>,
    web_dir: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct DataFile {
    dir: Option<PathBuf>,
    db: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
struct JobsFile {
    workers: Option<usize>,
    poll_interval_ms: Option<u64>,
    lease_secs: Option<u64>,
    max_attempts: Option<i64>,
    cache_gc_interval_secs: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct MediaFile {
    ffmpeg: Option<PathBuf>,
    ffprobe: Option<PathBuf>,
    probe_timeout_secs: Option<u64>,
    thumbnail_timeout_secs: Option<u64>,
    transcode_timeout_secs: Option<u64>,
    thumbnails_dir: Option<PathBuf>,
    transcodes_dir: Option<PathBuf>,
    hls_dir: Option<PathBuf>,
    cache_max_bytes: Option<u64>,
    cache_max_age_hours: Option<u64>,
}

fn find_config_file() -> Result<Option<PathBuf>> {
    if let Some(raw) = env_string("LOCALREEL_CONFIG") {
        let path = PathBuf::from(raw);
        if !path.is_file() {
            return Err(KernelError::Config(format!(
                "LOCALREEL_CONFIG points at a missing file: {}",
                path.display()
            )));
        }
        return Ok(Some(path));
    }
    let default = Path::new("localreel.toml");
    if default.is_file() {
        Ok(Some(default.to_path_buf()))
    } else {
        Ok(None)
    }
}

fn default_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|count| count.get().clamp(1, 4))
        .unwrap_or(2)
}

fn is_loopback_host(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    let trimmed = host.trim_start_matches('[').trim_end_matches(']');
    trimmed
        .parse::<IpAddr>()
        .map(|address| address.is_loopback())
        .unwrap_or(false)
}

fn env_string(key: &str) -> Option<String> {
    env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn env_u64(key: &str) -> Result<Option<u64>> {
    match env::var(key) {
        Ok(raw) => raw
            .trim()
            .parse::<u64>()
            .map(Some)
            .map_err(|_| KernelError::Config(format!("{key} must be an unsigned integer"))),
        Err(_) => Ok(None),
    }
}

fn env_bool(key: &str) -> Result<Option<bool>> {
    match env::var(key) {
        Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => Ok(Some(true)),
            "0" | "false" | "no" | "off" => Ok(Some(false)),
            _ => Err(KernelError::Config(format!(
                "{key} must be one of 1/0/true/false"
            ))),
        },
        Err(_) => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plugin_entries() {
        let file: ConfigFile = toml::from_str(
            r#"
            [[plugins]]
            package = "C:/plugins/demo.lrplug"
            enabled = false
            [plugins.config]
            root = "D:/library"
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(file).unwrap();
        assert_eq!(config.plugins.len(), 1);
        assert!(!config.plugins[0].enabled);
        assert_eq!(config.plugins[0].config["root"], "D:/library");
    }

    #[test]
    fn loopback_hosts_are_accepted() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(is_loopback_host("[::1]"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn default_config_is_valid() {
        Config::default().validate().unwrap();
    }

    #[test]
    fn remote_binding_requires_opt_in() {
        let mut config = Config {
            host: "0.0.0.0".into(),
            ..Default::default()
        };
        assert!(config.validate().is_err());
        config.allow_remote = true;
        config.validate().unwrap();
    }

    #[test]
    fn config_file_overrides_defaults() {
        let parsed: ConfigFile = toml::from_str(
            r#"
            [server]
            host = "127.0.0.1"
            port = 4100
            allow_remote = false

            [data]
            dir = "D:/LocalReelData"

            [jobs]
            workers = 3
            lease_secs = 90
            max_attempts = 4

            [media]
            ffmpeg = "C:/tools/ffmpeg.exe"
            probe_timeout_secs = 12
            cache_max_age_hours = 48
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(parsed).unwrap();
        assert_eq!(config.port, 4100);
        assert_eq!(config.jobs.workers, 3);
        assert_eq!(config.jobs.lease, Duration::from_secs(90));
        assert_eq!(config.jobs.max_attempts, 4);
        assert_eq!(config.media.probe_timeout, Duration::from_secs(12));
        assert_eq!(config.media.cache_max_age, Duration::from_secs(48 * 3600));
        assert_eq!(config.data_dir, PathBuf::from("D:/LocalReelData"));
        assert_eq!(
            config.media.thumbnail_dir,
            PathBuf::from("D:/LocalReelData/thumbnails")
        );
        assert_eq!(config.media.ffmpeg, PathBuf::from("C:/tools/ffmpeg.exe"));
    }

    #[test]
    fn invalid_config_file_is_rejected() {
        let parsed: std::result::Result<ConfigFile, _> = toml::from_str("server = 3");
        assert!(parsed.is_err());
    }
}
