use std::env;
use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::error::{KernelError, Result};

/// Full server configuration.
///
/// Defaults are safe for a local install: bind to loopback, keep data under
/// `./data`, use the `ffmpeg`/`ffprobe` from `PATH`. Every value can be
/// overridden with a `LOCALREEL_*` environment variable; a config file can be
/// added later without changing call sites.
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
}

#[derive(Debug, Clone)]
pub struct JobsConfig {
    pub workers: usize,
    pub poll_interval: Duration,
    pub lease: Duration,
    pub max_attempts: i64,
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
            },
            media: MediaConfig {
                ffmpeg: PathBuf::from("ffmpeg"),
                ffprobe: PathBuf::from("ffprobe"),
                probe_timeout: Duration::from_secs(30),
                thumbnail_timeout: Duration::from_secs(60),
                transcode_timeout: Duration::from_secs(30 * 60),
                thumbnail_dir: data_dir.join("thumbnails"),
                transcode_dir: data_dir.join("transcodes"),
                cache_max_bytes: 20 * 1024 * 1024 * 1024,
                cache_max_age: Duration::from_secs(30 * 24 * 60 * 60),
            },
        }
    }
}

impl Config {
    /// Load defaults plus `LOCALREEL_*` environment overrides, then validate.
    pub fn load() -> Result<Self> {
        let mut config = Self::default();

        if let Some(value) = env_string("LOCALREEL_HOST") {
            config.host = value;
        }
        if let Some(value) = env_u64("LOCALREEL_PORT")? {
            config.port = u16::try_from(value)
                .map_err(|_| KernelError::Config("LOCALREEL_PORT must be <= 65535".into()))?;
        }
        if let Some(value) = env_string("LOCALREEL_DATA_DIR") {
            config.data_dir = PathBuf::from(value);
            config.db_path = config.data_dir.join("localreel.db");
            config.media.thumbnail_dir = config.data_dir.join("thumbnails");
            config.media.transcode_dir = config.data_dir.join("transcodes");
        }
        if let Some(value) = env_string("LOCALREEL_DB") {
            config.db_path = PathBuf::from(value);
        }
        if let Some(value) = env_string("LOCALREEL_WEB_DIR") {
            config.web_dir = Some(PathBuf::from(value));
        }
        if let Some(value) = env_bool("LOCALREEL_ALLOW_REMOTE")? {
            config.allow_remote = value;
        }
        if let Some(value) = env_u64("LOCALREEL_JOB_WORKERS")? {
            config.jobs.workers = usize::try_from(value)
                .map_err(|_| KernelError::Config("LOCALREEL_JOB_WORKERS is too large".into()))?;
        }
        if let Some(value) = env_u64("LOCALREEL_JOB_LEASE_SECS")? {
            config.jobs.lease = Duration::from_secs(value);
        }
        if let Some(value) = env_u64("LOCALREEL_JOB_MAX_ATTEMPTS")? {
            config.jobs.max_attempts = i64::try_from(value).map_err(|_| {
                KernelError::Config("LOCALREEL_JOB_MAX_ATTEMPTS is too large".into())
            })?;
        }
        if let Some(value) = env_string("LOCALREEL_FFMPEG") {
            config.media.ffmpeg = PathBuf::from(value);
        }
        if let Some(value) = env_string("LOCALREEL_FFPROBE") {
            config.media.ffprobe = PathBuf::from(value);
        }
        if let Some(value) = env_u64("LOCALREEL_TRANSCODE_CACHE_MAX_BYTES")? {
            config.media.cache_max_bytes = value;
        }
        if let Some(value) = env_u64("LOCALREEL_TRANSCODE_CACHE_MAX_AGE_HOURS")? {
            config.media.cache_max_age = Duration::from_secs(value.saturating_mul(60 * 60));
        }

        config.validate()?;
        Ok(config)
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
        Ok(())
    }

    /// Create the data/thumbnail/transcode directories. Idempotent.
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.data_dir,
            &self.media.thumbnail_dir,
            &self.media.transcode_dir,
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

    pub fn db_path_string(&self) -> String {
        self.db_path.to_string_lossy().into_owned()
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
}
