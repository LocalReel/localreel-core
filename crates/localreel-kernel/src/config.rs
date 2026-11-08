use std::env;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{KernelError, Result};
use crate::paths::{self, Dirs, Env, Os};
use crate::time::now_secs;

/// How this process should present itself.
///
/// The desktop and server personas share one binary; only the launch
/// behaviour, the security defaults and the directory layout differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunMode {
    /// Started by a person: open the UI, heal a busy port, stay visible.
    Desktop,
    /// Started by a service manager or container: fixed port, no UI popup.
    Server,
}

impl RunMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            RunMode::Desktop => "desktop",
            RunMode::Server => "server",
        }
    }

    pub const fn is_desktop(self) -> bool {
        matches!(self, RunMode::Desktop)
    }
}

impl std::fmt::Display for RunMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for RunMode {
    type Err = KernelError;

    fn from_str(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "desktop" => Ok(RunMode::Desktop),
            "server" => Ok(RunMode::Server),
            other => Err(KernelError::Config(format!(
                "invalid mode '{other}': expected 'desktop' or 'server'"
            ))),
        }
    }
}

/// Full server configuration.
///
/// Precedence: built-in defaults → optional `localreel.toml` → `LOCALREEL_*`
/// environment variables. `LOCALREEL_CONFIG` can point at a different file.
#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,
    /// `desktop` opens the UI and heals a busy port; `server` stays strict.
    pub mode: RunMode,
    /// Where the persona came from: `--mode`, `LOCALREEL_MODE` or `detected`.
    pub mode_source: &'static str,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// Where rolling log files are written.
    pub log_dir: PathBuf,
    pub web_dir: Option<PathBuf>,
    /// Must be explicitly enabled to bind beyond loopback. The default is
    /// local-only, matching the trust model of the previous LocalReel app.
    pub allow_remote: bool,
    pub jobs: JobsConfig,
    pub media: MediaConfig,
    pub scan: ScanConfig,
    pub metadata: MetadataConfig,
    /// Sidecar plugins to start with the server (packages on disk).
    pub plugins: Vec<PluginConfig>,
    /// Trusted publisher keys used by `plugins[].publisher`.
    pub publishers: Vec<PluginPublisherConfig>,
    /// What happens when one of them cannot start.
    pub plugin_runtime: PluginRuntimeConfig,
    /// Whether the admin page may ask GitHub for the latest release.
    pub update: UpdateConfig,
}

/// Remote release checking: a tracker, not an installer.
///
/// `check` is on by default; the request goes to the public GitHub API with a
/// six second budget at most once per `interval_hours`. `feed` exists for
/// mirrors and for tests.
#[derive(Debug, Clone)]
pub struct UpdateConfig {
    pub check: bool,
    pub feed: Option<String>,
    pub interval_hours: u64,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self {
            check: true,
            feed: None,
            interval_hours: 24,
        }
    }
}

/// Where optional posters and descriptions come from.
///
/// `none` (the default) is a complete, working configuration: the library uses
/// thumbnails and filenames and never talks to the internet. Choosing `tmdb`
/// requires an API key; without one the provider is treated as `none` and the
/// server says so instead of failing.
#[derive(Debug, Clone)]
pub struct MetadataConfig {
    pub provider: MetadataProvider,
    pub api_key: Option<String>,
    /// TMDB language tag, for example `zh-CN` or `en-US`.
    pub language: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MetadataProvider {
    #[default]
    None,
    Tmdb,
}

impl MetadataProvider {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "off" | "" => Some(Self::None),
            "tmdb" => Some(Self::Tmdb),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Tmdb => "tmdb",
        }
    }
}

/// How sources are rescanned.
///
/// Watching is on by default: a library that only updates when someone presses
/// "scan" is not a media server. It degrades quietly — an unwatchable source
/// (a WebDAV plugin, an SMB mount, a kernel without inotify slots left) is
/// skipped and reported, never fatal.
#[derive(Debug, Clone)]
pub struct ScanConfig {
    /// Watch enabled local sources and rescan them when files appear.
    pub watch: bool,
}

/// One configured sidecar plugin.
#[derive(Debug, Clone)]
pub struct PluginConfig {
    /// Path to the `.lrplug` package.
    pub package: PathBuf,
    pub enabled: bool,
    /// Optional SHA-256 pin. When present, the package bytes must match
    /// before the plugin is inspected or extracted.
    pub sha256: Option<String>,
    /// Publisher id from `[[plugin_publishers]]`.
    pub publisher: Option<String>,
    /// Ed25519 signature (128 hex chars) over the package SHA-256 digest.
    pub signature: Option<String>,
    /// JSON handed to the plugin via `LOCALREEL_PLUGIN_CONFIG`.
    pub config: serde_json::Value,
    /// Per-plugin override for `[plugin_runtime] app_container`.
    pub app_container: Option<bool>,
}

/// A trusted publisher public key.
#[derive(Debug, Clone)]
pub struct PluginPublisherConfig {
    pub id: String,
    /// Ed25519 public key, 32 bytes as 64 hex characters.
    pub public_key: String,
    /// Unix timestamp after which this key is no longer trusted.
    pub not_after: Option<i64>,
    /// Explicit revocation. The key stays in the file for history but is not
    /// used for verification.
    pub revoked: bool,
}

impl PluginPublisherConfig {
    pub fn is_active_at(&self, now: i64) -> bool {
        !self.revoked && self.not_after.is_none_or(|expires| expires > now)
    }
}

/// How the host treats plugins that fail to start.
///
/// The default is the desktop-friendly one: a broken plugin is reported and the
/// server keeps running, because a media server that refuses to open its own UI
/// because one optional provider is misconfigured is worse than a degraded
/// library. Servers that would rather fail loudly can opt in with
/// `[plugin_runtime] strict = true`.
#[derive(Debug, Clone)]
pub struct PluginRuntimeConfig {
    /// Abort startup when an enabled plugin cannot be loaded.
    pub strict: bool,
    /// How long a stop waits for in-flight plugin requests before the process
    /// is killed. Also used by the shutdown path.
    pub stop_timeout: Duration,
    /// Address-space ceiling for one plugin process tree.
    pub max_memory_bytes: u64,
    /// Process-count ceiling (Windows Job Object; Unix enforced elsewhere).
    pub max_processes: u32,
    /// OS network isolation: AppContainer on Windows, netns via `unshare` on
    /// Linux. macOS is rejected; there is no fallback to ambient networking.
    pub app_container: bool,
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
    /// Progress-bar preview sprite sheets.
    pub preview_dir: PathBuf,
    /// Downloaded posters, served at `/posters`.
    pub poster_dir: PathBuf,
    /// Generate preview sprites for long videos.
    pub preview_sprites: bool,
    /// Skip the sprite for anything shorter than this.
    pub preview_min_secs: u64,
    pub cache_max_bytes: u64,
    pub cache_max_age: Duration,
    /// How many ffmpeg jobs (transcode/HLS packaging) may run at once.
    ///
    /// One CPU and one memory budget is what a NAS actually has; letting every
    /// viewer start their own encode is how a media server becomes the reason
    /// the household network is slow.
    pub max_concurrent_transcodes: usize,
    /// A playback session with no request for this long is reclaimed.
    pub idle_session_secs: u64,
    /// Hardware acceleration preference: `auto` (default), `none`, `nvenc`,
    /// `qsv`, `vaapi` or `videotoolbox`.
    pub hwaccel: crate::media_ops::HwAccel,
}

impl Default for Config {
    fn default() -> Self {
        let dirs = platform_dirs();
        let data_dir = dirs
            .as_ref()
            .map(|dirs| dirs.data.clone())
            .unwrap_or_else(|| PathBuf::from("data"));
        let log_dir = default_log_dir();
        Self {
            host: "127.0.0.1".to_string(),
            port: 3001,
            mode: RunMode::Desktop,
            mode_source: "detected",
            db_path: data_dir.join("localreel.db"),
            log_dir,
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
                preview_dir: data_dir.join("previews"),
                poster_dir: data_dir.join("cache").join("posters"),
                preview_sprites: true,
                preview_min_secs: 600,
                cache_max_bytes: 20 * 1024 * 1024 * 1024,
                cache_max_age: Duration::from_secs(30 * 24 * 60 * 60),
                max_concurrent_transcodes: 2,
                idle_session_secs: 900,
                hwaccel: crate::media_ops::HwAccel::Auto,
            },
            scan: ScanConfig { watch: true },
            metadata: MetadataConfig {
                provider: MetadataProvider::None,
                api_key: None,
                language: "zh-CN".to_string(),
            },
            plugins: Vec::new(),
            publishers: Vec::new(),
            plugin_runtime: PluginRuntimeConfig {
                strict: false,
                stop_timeout: Duration::from_secs(5),
                max_memory_bytes: 512 * 1024 * 1024,
                max_processes: 16,
                app_container: false,
            },
            update: UpdateConfig::default(),
        }
    }
}

impl Config {
    /// Load defaults + optional config file + environment overrides, validate,
    /// and return the file that was used (if any).
    pub fn load_with_source() -> Result<(Self, Option<PathBuf>)> {
        Self::load_with_mode(None)
    }

    /// Load configuration for a specific persona.
    ///
    /// `mode` comes from the command line when the operator passed `--mode`;
    /// otherwise the `LOCALREEL_MODE` variable or the built-in default applies.
    pub fn load_with_mode(mode: Option<RunMode>) -> Result<(Self, Option<PathBuf>)> {
        let mut config = Self::default();
        let platform_data = config.data_dir.clone();
        let dirs = platform_dirs();
        let source_path = find_config_file(&platform_data, dirs.as_ref())?;

        let mut explicit_data = false;
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
            explicit_data = parsed.data.dir.is_some();
            config.apply_file(parsed)?;
        }

        let mode_from_env = env::var_os("LOCALREEL_MODE").is_some();
        config.apply_env()?;
        if let Some(mode) = mode {
            config.mode = mode;
            config.mode_source = "--mode";
        } else if mode_from_env {
            config.mode_source = "LOCALREEL_MODE";
        }
        explicit_data |= env::var_os("LOCALREEL_DATA_DIR").is_some();

        // A layout from before platform directories existed keeps working:
        // adopt `<exe dir>/data` or `<cwd>/data` when no explicit data dir was
        // configured and the platform location is still empty.
        if !explicit_data {
            if let Some(legacy) = adopt_legacy_data_dir(&platform_data) {
                tracing::warn!(
                    path = %legacy.display(),
                    "using a legacy data directory next to the install; set \
                     LOCALREEL_DATA_DIR to silence this"
                );
                config.set_data_dir(legacy);
            }
        }

        config.resolve_paths(source_path.as_deref());
        config.validate()?;
        Ok((config, source_path))
    }

    /// Make every configured path absolute.
    ///
    /// A relative `[data] dir` used to be interpreted against the process
    /// working directory, which changes with the launch method and moved the
    /// library with it. It is anchored to the program's own directory instead:
    /// the library follows the installation, so moving or copying the installed
    /// folder takes the library along, which is how a self-contained household
    /// server is expected to behave.
    fn resolve_paths(&mut self, source: Option<&Path>) {
        let base = paths::exe_dir()
            .or_else(|| source.and_then(Path::parent).map(Path::to_path_buf))
            .or_else(|| std::env::current_dir().ok());
        let Some(base) = base else {
            return;
        };
        self.data_dir = anchored(&base, self.data_dir.clone());
        self.db_path = anchored(&base, self.db_path.clone());
        self.log_dir = anchored(&base, self.log_dir.clone());
        if let Some(web) = &self.web_dir {
            self.web_dir = Some(anchored(&base, web.clone()));
        }
        self.media.thumbnail_dir = anchored(&base, self.media.thumbnail_dir.clone());
        self.media.transcode_dir = anchored(&base, self.media.transcode_dir.clone());
        self.media.hls_dir = anchored(&base, self.media.hls_dir.clone());
        self.media.preview_dir = anchored(&base, self.media.preview_dir.clone());
        self.media.poster_dir = anchored(&base, self.media.poster_dir.clone());
    }

    /// Convenience wrapper for callers that do not need the config origin.
    pub fn load() -> Result<Self> {
        Self::load_with_source().map(|(config, _)| config)
    }

    /// Point the data directory (and everything derived from it) at `dir`.
    pub fn set_data_dir(&mut self, dir: PathBuf) {
        self.db_path = dir.join("localreel.db");
        self.media.thumbnail_dir = dir.join("thumbnails");
        self.media.transcode_dir = dir.join("transcodes");
        self.media.hls_dir = dir.join("hls");
        self.media.preview_dir = dir.join("previews");
        self.media.poster_dir = dir.join("cache").join("posters");
        self.data_dir = dir;
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
            self.set_data_dir(dir);
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
        if let Some(value) = file.media.previews_dir {
            self.media.preview_dir = value;
        }
        if let Some(value) = file.media.preview_sprites {
            self.media.preview_sprites = value;
        }
        if let Some(value) = file.media.preview_min_secs {
            self.media.preview_min_secs = value;
        }
        if let Some(value) = file.media.cache_max_bytes {
            self.media.cache_max_bytes = value;
        }
        if let Some(value) = file.media.cache_max_age_hours {
            self.media.cache_max_age = Duration::from_secs(value.saturating_mul(60 * 60));
        }
        if let Some(value) = file.media.max_concurrent_transcodes {
            self.media.max_concurrent_transcodes = value;
        }
        if let Some(value) = file.media.idle_session_secs {
            self.media.idle_session_secs = value;
        }
        if let Some(value) = &file.media.hwaccel {
            self.media.hwaccel = crate::media_ops::HwAccel::parse(value).ok_or_else(|| {
                KernelError::Config(format!(
                    "invalid hwaccel '{value}': expected auto, none, nvenc, qsv, vaapi or videotoolbox"
                ))
            })?;
        }
        if let Some(value) = file.scan.watch {
            self.scan.watch = value;
        }
        if let Some(value) = &file.metadata.provider {
            self.metadata.provider = MetadataProvider::parse(value).ok_or_else(|| {
                KernelError::Config(format!(
                    "invalid metadata provider '{value}': expected none or tmdb"
                ))
            })?;
        }
        if let Some(value) = &file.metadata.api_key {
            let trimmed = value.trim();
            self.metadata.api_key = (!trimmed.is_empty()).then(|| trimmed.to_string());
        }
        if let Some(value) = &file.metadata.language {
            self.metadata.language = value.clone();
        }
        if !file.plugins.is_empty() {
            self.plugins = file
                .plugins
                .into_iter()
                .map(|plugin| PluginConfig {
                    package: plugin.package,
                    enabled: plugin.enabled,
                    sha256: plugin.sha256,
                    publisher: plugin.publisher,
                    signature: plugin.signature,
                    config: plugin.config,
                    app_container: plugin.app_container,
                })
                .collect();
        }
        if !file.publishers.is_empty() {
            self.publishers = file
                .publishers
                .into_iter()
                .map(|publisher| PluginPublisherConfig {
                    id: publisher.id,
                    public_key: publisher.public_key,
                    not_after: publisher.not_after,
                    revoked: publisher.revoked,
                })
                .collect();
        }
        if let Some(value) = file.plugin_runtime.strict {
            self.plugin_runtime.strict = value;
        }
        if let Some(value) = file.plugin_runtime.stop_timeout_secs {
            self.plugin_runtime.stop_timeout = Duration::from_secs(value);
        }
        if let Some(value) = file.plugin_runtime.max_memory_mb {
            self.plugin_runtime.max_memory_bytes = value.saturating_mul(1024 * 1024);
        }
        if let Some(value) = file.plugin_runtime.max_processes {
            self.plugin_runtime.max_processes = value;
        }
        if let Some(value) = file.plugin_runtime.app_container {
            self.plugin_runtime.app_container = value;
        }
        if let Some(value) = file.update.check {
            self.update.check = value;
        }
        if let Some(value) = file.update.feed {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                self.update.feed = Some(trimmed.to_string());
            }
        }
        if let Some(value) = file.update.interval_hours {
            self.update.interval_hours = value.max(1);
        }
        Ok(())
    }

    fn apply_env(&mut self) -> Result<()> {
        // Environment values are command-line-like: a relative path means
        // "relative to where I ran this", so it is resolved here and stays
        // absolute even if the process changes directory later.
        let cwd = std::env::current_dir().unwrap_or_default();
        if let Some(value) = env_string("LOCALREEL_HOST") {
            self.host = value;
        }
        if let Some(value) = env_u64("LOCALREEL_PORT")? {
            self.port = u16::try_from(value)
                .map_err(|_| KernelError::Config("LOCALREEL_PORT must be <= 65535".into()))?;
        }
        if let Some(value) = env_string("LOCALREEL_DATA_DIR") {
            self.set_data_dir(anchored(&cwd, PathBuf::from(value)));
        }
        if let Some(value) = env_string("LOCALREEL_LOG_DIR") {
            self.log_dir = anchored(&cwd, PathBuf::from(value));
        }
        if let Some(value) = env_string("LOCALREEL_MODE") {
            self.mode = value.parse()?;
        }
        if let Some(value) = env_string("LOCALREEL_DB") {
            self.db_path = anchored(&cwd, PathBuf::from(value));
        }
        if let Some(value) = env_string("LOCALREEL_WEB_DIR") {
            self.web_dir = Some(anchored(&cwd, PathBuf::from(value)));
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
            self.media.hls_dir = anchored(&cwd, PathBuf::from(value));
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
        if let Some(value) = env_bool("LOCALREEL_WATCH")? {
            self.scan.watch = value;
        }
        if let Some(value) = env_u64("LOCALREEL_MAX_CONCURRENT_TRANSCODES")? {
            self.media.max_concurrent_transcodes = usize::try_from(value).map_err(|_| {
                KernelError::Config("LOCALREEL_MAX_CONCURRENT_TRANSCODES is too large".into())
            })?;
        }
        if let Some(value) = env_u64("LOCALREEL_IDLE_SESSION_SECS")? {
            self.media.idle_session_secs = value;
        }
        if let Some(value) = env_string("LOCALREEL_PREVIEW_DIR") {
            self.media.preview_dir = anchored(&cwd, PathBuf::from(value));
        }
        if let Some(value) = env_bool("LOCALREEL_PREVIEW_SPRITES")? {
            self.media.preview_sprites = value;
        }
        if let Some(value) = env_string("LOCALREEL_METADATA_PROVIDER") {
            self.metadata.provider = MetadataProvider::parse(&value).ok_or_else(|| {
                KernelError::Config(format!(
                    "invalid LOCALREEL_METADATA_PROVIDER '{value}': expected none or tmdb"
                ))
            })?;
        }
        if let Some(value) = env_string("LOCALREEL_TMDB_API_KEY") {
            self.metadata.api_key = Some(value);
        }
        if let Some(value) = env_string("LOCALREEL_METADATA_LANGUAGE") {
            self.metadata.language = value;
        }
        if let Some(value) = env_u64("LOCALREEL_PREVIEW_MIN_SECS")? {
            self.media.preview_min_secs = value;
        }
        if let Some(value) = env_string("LOCALREEL_HWACCEL") {
            self.media.hwaccel = crate::media_ops::HwAccel::parse(&value).ok_or_else(|| {
                KernelError::Config(format!(
                    "invalid LOCALREEL_HWACCEL '{value}': expected auto, none, nvenc, qsv, vaapi or videotoolbox"
                ))
            })?;
        }
        Ok(())
    }

    /// Reject configurations that cannot work.
    pub fn validate(&self) -> Result<()> {
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
        if !(1..=64).contains(&self.media.max_concurrent_transcodes) {
            return Err(KernelError::Config(
                "max concurrent transcodes must be between 1 and 64".into(),
            ));
        }
        if self.media.idle_session_secs == 0 {
            return Err(KernelError::Config(
                "idle session seconds must be greater than 0".into(),
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
            if let Some(digest) = &plugin.sha256 {
                if digest.len() != 64 || !digest.chars().all(|ch| ch.is_ascii_hexdigit()) {
                    return Err(KernelError::Config(
                        "plugin sha256 must be 64 hexadecimal characters".into(),
                    ));
                }
            }
            if let Some(publisher) = &plugin.publisher {
                if publisher.trim().is_empty() {
                    return Err(KernelError::Config(
                        "plugin publisher must not be empty".into(),
                    ));
                }
                let now = now_secs();
                if !self
                    .publishers
                    .iter()
                    .any(|known| &known.id == publisher && known.is_active_at(now))
                {
                    return Err(KernelError::Config(format!(
                        "plugin references publisher '{publisher}' with no active key"
                    )));
                }
                let Some(signature) = &plugin.signature else {
                    return Err(KernelError::Config(format!(
                        "plugin publisher '{publisher}' requires a signature"
                    )));
                };
                if signature.len() != 128 || !signature.chars().all(|ch| ch.is_ascii_hexdigit()) {
                    return Err(KernelError::Config(
                        "plugin signature must be 128 hexadecimal characters".into(),
                    ));
                }
            } else if plugin.signature.is_some() {
                return Err(KernelError::Config(
                    "plugin signature requires a publisher".into(),
                ));
            }
        }
        for (index, publisher) in self.publishers.iter().enumerate() {
            if publisher.id.trim().is_empty() {
                return Err(KernelError::Config("publisher id must not be empty".into()));
            }
            if publisher.public_key.len() != 64
                || !publisher
                    .public_key
                    .chars()
                    .all(|ch| ch.is_ascii_hexdigit())
            {
                return Err(KernelError::Config(
                    "publisher public_key must be 64 hexadecimal characters".into(),
                ));
            }
            if self.publishers[..index]
                .iter()
                .any(|known| known.id == publisher.id && known.public_key == publisher.public_key)
            {
                return Err(KernelError::Config(format!(
                    "duplicate publisher key for '{}'",
                    publisher.id
                )));
            }
        }
        if self.plugin_runtime.stop_timeout.is_zero() {
            return Err(KernelError::Config(
                "plugin stop timeout must be greater than 0".into(),
            ));
        }
        if self.plugin_runtime.app_container && !cfg!(windows) && !cfg!(target_os = "linux") {
            return Err(KernelError::Config(
                "plugin_runtime.app_container is supported on Windows (AppContainer) and Linux (netns) only".into(),
            ));
        }
        let memory = self.plugin_runtime.max_memory_bytes;
        if !(16 * 1024 * 1024..=16 * 1024 * 1024 * 1024).contains(&memory) {
            return Err(KernelError::Config(
                "plugin max_memory_mb must be 16..=16384".into(),
            ));
        }
        if !(1..=256).contains(&self.plugin_runtime.max_processes) {
            return Err(KernelError::Config(
                "plugin max_processes must be 1..=256".into(),
            ));
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
            &self.media.preview_dir,
            &self.media.poster_dir,
            &self.log_dir,
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

/// Turn one `[[plugins]]` entry on or off in the configuration file.
///
/// Rewrites only the `enabled` key of the matching entry and keeps every other
/// byte the operator wrote (comments, ordering, spacing). Returns `Ok(false)`
/// when no entry names `package`, so the caller can leave a runtime-only switch
/// in place instead of inventing an entry.
///
/// `package` is matched after normalising path separators: the TOML value uses
/// `/`, a `PathBuf` printed on Windows uses `\`.
pub fn set_plugin_enabled(path: &Path, package: &str, enabled: bool) -> Result<bool> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| KernelError::Config(format!("cannot read {}: {error}", path.display())))?;
    let mut document: toml_edit::DocumentMut = text.parse().map_err(|error| {
        KernelError::Config(format!("cannot parse {}: {error}", path.display()))
    })?;
    let wanted = normalize_package(package);
    let mut found = false;
    if let Some(plugins) = document
        .get_mut("plugins")
        .and_then(|item| item.as_array_of_tables_mut())
    {
        for entry in plugins.iter_mut() {
            let Some(value) = entry.get("package").and_then(|item| item.as_str()) else {
                continue;
            };
            if normalize_package(value) != wanted {
                continue;
            }
            entry["enabled"] = toml_edit::value(enabled);
            found = true;
        }
    }
    if !found {
        return Ok(false);
    }

    // Replace the file in one step so a crash cannot leave a half-written
    // configuration behind.
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "localreel.toml".to_string());
    let temp = path.with_file_name(format!(".{file_name}.tmp"));
    std::fs::write(&temp, document.to_string()).map_err(|error| {
        KernelError::Config(format!("cannot write {}: {error}", temp.display()))
    })?;
    #[cfg(windows)]
    {
        // `rename` does not replace an existing file on Windows.
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        KernelError::Config(format!("cannot replace {}: {error}", path.display()))
    })?;
    Ok(true)
}

fn normalize_package(value: &str) -> String {
    value.replace('\\', "/").trim().to_string()
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
    scan: ScanFile,
    #[serde(default)]
    metadata: MetadataFile,
    #[serde(default)]
    plugins: Vec<PluginFile>,
    #[serde(default)]
    publishers: Vec<PluginPublisherFile>,
    #[serde(default)]
    plugin_runtime: PluginRuntimeFile,
    #[serde(default)]
    update: UpdateFile,
}

/// `[update]` in the configuration file.
#[derive(Debug, Default, Deserialize)]
struct UpdateFile {
    check: Option<bool>,
    feed: Option<String>,
    interval_hours: Option<u64>,
}

/// `[plugin_runtime]` in the configuration file.
#[derive(Debug, Default, Deserialize)]
struct PluginRuntimeFile {
    strict: Option<bool>,
    stop_timeout_secs: Option<u64>,
    max_memory_mb: Option<u64>,
    max_processes: Option<u32>,
    app_container: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PluginFile {
    package: PathBuf,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    sha256: Option<String>,
    #[serde(default)]
    publisher: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    config: serde_json::Value,
    #[serde(default)]
    app_container: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct PluginPublisherFile {
    id: String,
    public_key: String,
    #[serde(default)]
    not_after: Option<i64>,
    #[serde(default)]
    revoked: bool,
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
    previews_dir: Option<PathBuf>,
    preview_sprites: Option<bool>,
    preview_min_secs: Option<u64>,
    cache_max_bytes: Option<u64>,
    cache_max_age_hours: Option<u64>,
    max_concurrent_transcodes: Option<usize>,
    idle_session_secs: Option<u64>,
    hwaccel: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ScanFile {
    watch: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct MetadataFile {
    provider: Option<String>,
    api_key: Option<String>,
    language: Option<String>,
}

/// Platform log directory, independent of the configuration file.
///
/// The server initialises logging before it loads the configuration (so load
/// warnings are captured), which makes this the authoritative source until a
/// config file can override `data.dir`.
pub fn default_log_dir() -> PathBuf {
    if let Some(value) = env_string("LOCALREEL_LOG_DIR") {
        return PathBuf::from(value);
    }
    platform_dirs()
        .map(|dirs| dirs.log)
        .unwrap_or_else(|| PathBuf::from("data").join("logs"))
}

fn find_config_file(data_dir: &Path, dirs: Option<&Dirs>) -> Result<Option<PathBuf>> {
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

    // Platform config directory first, then the data directory (containers keep
    // everything under /data), then the working directory for old checkouts.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dirs) = dirs {
        candidates.push(dirs.config.join("localreel.toml"));
    }
    candidates.push(data_dir.join("localreel.toml"));
    candidates.push(PathBuf::from("localreel.toml"));

    Ok(candidates.into_iter().find(|path| path.is_file()))
}

/// Platform directories for this process, when the environment names them.
fn platform_dirs() -> Option<Dirs> {
    Dirs::resolve(Os::current(), &Env::from_process())
}

/// A `data/` directory written before platform directories existed.
///
/// Only consulted when the platform location is still empty and the operator
/// did not configure a data directory, so existing libraries are not orphaned.
fn adopt_legacy_data_dir(platform_data: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(dir) = paths::exe_dir() {
        candidates.push(dir.join("data"));
    }
    if let Some(dir) = paths::current_dir() {
        candidates.push(dir.join("data"));
    }
    adopt_from(platform_data, candidates)
}

/// Join `path` onto `base` when it is relative; leave absolute paths alone.
fn anchored(base: &Path, path: PathBuf) -> PathBuf {
    if path.is_absolute() || path.as_os_str().is_empty() {
        path
    } else {
        base.join(path)
    }
}

fn adopt_from(platform_data: &Path, candidates: Vec<PathBuf>) -> Option<PathBuf> {
    if platform_data.join("localreel.db").is_file() {
        return None;
    }
    candidates
        .into_iter()
        .find(|dir| dir != platform_data && dir.join("localreel.db").is_file())
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
            [[publishers]]
            id = "acme"
            public_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

            [[plugins]]
            package = "C:/plugins/demo.lrplug"
            enabled = false
            sha256 = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
            publisher = "acme"
            signature = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            app_container = true
            [plugins.config]
            root = "D:/library"
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(file).unwrap();
        config.validate().unwrap();
        assert_eq!(config.plugins.len(), 1);
        assert!(!config.plugins[0].enabled);
        assert_eq!(
            config.plugins[0].sha256.as_deref(),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(config.plugins[0].publisher.as_deref(), Some("acme"));
        assert_eq!(config.plugins[0].config["root"], "D:/library");
        assert_eq!(config.plugins[0].app_container, Some(true));
        assert_eq!(config.publishers.len(), 1);
        assert_eq!(config.publishers[0].id, "acme");
        // Failure isolation is the default; strictness is opt-in.
        assert!(!config.plugin_runtime.strict);
    }

    #[test]
    fn rejects_a_malformed_sha256_pin() {
        let file: ConfigFile = toml::from_str(
            r#"
            [[plugins]]
            package = "C:/plugins/demo.lrplug"
            sha256 = "not-a-digest"
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(file).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn accepts_rotated_publisher_keys_and_requires_an_active_one() {
        let file: ConfigFile = toml::from_str(
            r#"
            [[publishers]]
            id = "acme"
            public_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            revoked = true

            [[publishers]]
            id = "acme"
            public_key = "fedcba9876543210fedcba9876543210fedcba9876543210fedcba9876543210"

            [[plugins]]
            package = "C:/plugins/demo.lrplug"
            publisher = "acme"
            signature = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(file).unwrap();
        config.validate().unwrap();
        assert_eq!(config.publishers.len(), 2);

        config.publishers[1].revoked = true;
        assert!(config.validate().is_err());

        config.publishers[1].revoked = false;
        config.publishers[1].not_after = Some(1);
        assert!(config.validate().is_err());
    }

    #[test]
    fn rejects_a_plugin_with_an_unknown_publisher() {
        let file: ConfigFile = toml::from_str(
            r#"
            [[plugins]]
            package = "C:/plugins/demo.lrplug"
            publisher = "nobody"
            signature = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(file).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn disabling_a_plugin_rewrites_only_that_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localreel.toml");
        std::fs::write(
            &path,
            r#"# operator notes
[[plugins]]
package = "plugins/demo.lrplug"

[[plugins]]
package = "plugins/other.lrplug"
enabled = false
"#,
        )
        .unwrap();

        // The caller passes a `PathBuf` printed on Windows; the file uses `/`.
        assert!(set_plugin_enabled(&path, "plugins\\demo.lrplug", false).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# operator notes"), "comments must survive");
        let parsed: ConfigFile = toml::from_str(&text).unwrap();
        assert!(!parsed.plugins[0].enabled);
        assert!(!parsed.plugins[1].enabled);

        // An entry that is not in the file is not invented.
        assert!(!set_plugin_enabled(&path, "plugins/missing.lrplug", true).unwrap());
        let parsed: ConfigFile = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(parsed.plugins.len(), 2);
    }

    #[test]
    fn parses_plugin_runtime_policy() {
        let file: ConfigFile = toml::from_str(
            r#"
            [[plugins]]
            package = "plugins/demo.lrplug"

            [plugin_runtime]
            strict = true
            stop_timeout_secs = 2
            max_memory_mb = 256
            max_processes = 8
            app_container = true
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(file).unwrap();
        assert!(config.plugin_runtime.strict);
        assert_eq!(config.plugin_runtime.stop_timeout, Duration::from_secs(2));
        assert_eq!(config.plugin_runtime.max_memory_bytes, 256 * 1024 * 1024);
        assert_eq!(config.plugin_runtime.max_processes, 8);
        assert!(config.plugin_runtime.app_container);
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
    fn transcode_budget_defaults_and_overrides() {
        let config = Config::default();
        assert_eq!(config.media.max_concurrent_transcodes, 2);
        assert_eq!(config.media.idle_session_secs, 900);

        let parsed: ConfigFile = toml::from_str(
            r#"
            [media]
            max_concurrent_transcodes = 1
            idle_session_secs = 120
            "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.apply_file(parsed).unwrap();
        assert_eq!(config.media.max_concurrent_transcodes, 1);
        assert_eq!(config.media.idle_session_secs, 120);

        // A zero budget would deadlock every transcode.
        let mut invalid = Config::default();
        invalid.media.max_concurrent_transcodes = 0;
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn hwaccel_defaults_to_auto_and_rejects_nonsense() {
        use crate::media_ops::HwAccel;

        assert_eq!(Config::default().media.hwaccel, HwAccel::Auto);

        let parsed: ConfigFile = toml::from_str("[media]\nhwaccel = \"nvenc\"").unwrap();
        let mut config = Config::default();
        config.apply_file(parsed).unwrap();
        assert_eq!(config.media.hwaccel, HwAccel::Nvenc);

        // A typo must fail at load time instead of silently encoding on the CPU.
        let parsed: ConfigFile = toml::from_str("[media]\nhwaccel = \"nvdec\"").unwrap();
        let mut broken = Config::default();
        assert!(broken.apply_file(parsed).is_err());

        assert_eq!(HwAccel::parse("QSV"), Some(HwAccel::Qsv));
        assert_eq!(HwAccel::parse("off"), Some(HwAccel::None));
        assert!(HwAccel::parse("nvdec").is_none());
    }

    #[test]
    fn relative_paths_are_anchored_to_the_install_directory() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("localreel.toml");
        std::fs::write(&config_path, "[data]\ndir = \"library\"\n").unwrap();

        let parsed: ConfigFile = toml::from_str("[data]\ndir = \"library\"").unwrap();
        let mut config = Config::default();
        config.apply_file(parsed).unwrap();
        config.resolve_paths(Some(&config_path));

        let install = crate::paths::exe_dir().expect("the test binary has a directory");
        assert_eq!(config.data_dir, install.join("library"));
        assert_eq!(config.db_path, install.join("library/localreel.db"));
        assert_eq!(
            config.media.thumbnail_dir,
            install.join("library/thumbnails")
        );
    }

    #[test]
    fn absolute_paths_survive_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let absolute = dir.path().join("library");
        let text = format!(
            "[data]\ndir = \"{}\"",
            absolute.to_string_lossy().replace('\\', "/")
        );
        let parsed: ConfigFile = toml::from_str(&text).unwrap();
        let mut config = Config::default();
        config.apply_file(parsed).unwrap();
        config.resolve_paths(Some(Path::new("C:/somewhere/localreel.toml")));

        assert_eq!(config.data_dir, absolute);
    }

    #[test]
    fn anchored_keeps_absolute_paths_and_joins_relative_ones() {
        let base = Path::new("C:/base");
        assert_eq!(anchored(base, PathBuf::from("data")), base.join("data"));
        let absolute = std::env::temp_dir().join("library");
        assert_eq!(anchored(base, absolute.clone()), absolute);
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

    #[test]
    fn run_mode_parses_case_insensitively() {
        assert_eq!("desktop".parse::<RunMode>().unwrap(), RunMode::Desktop);
        assert_eq!(" SERVER ".parse::<RunMode>().unwrap(), RunMode::Server);
        assert!("headless".parse::<RunMode>().is_err());
    }

    #[test]
    fn server_mode_is_not_desktop() {
        assert!(RunMode::Desktop.is_desktop());
        assert!(!RunMode::Server.is_desktop());
        assert_eq!(RunMode::Server.as_str(), "server");
    }

    #[test]
    fn set_data_dir_moves_derived_directories() {
        let mut config = Config::default();
        config.set_data_dir(PathBuf::from("/tmp/library"));
        assert_eq!(config.db_path, PathBuf::from("/tmp/library/localreel.db"));
        assert_eq!(
            config.media.thumbnail_dir,
            PathBuf::from("/tmp/library/thumbnails")
        );
        assert_eq!(
            config.media.transcode_dir,
            PathBuf::from("/tmp/library/transcodes")
        );
        assert_eq!(config.media.hls_dir, PathBuf::from("/tmp/library/hls"));
        assert_eq!(
            config.media.preview_dir,
            PathBuf::from("/tmp/library/previews")
        );
    }

    #[test]
    fn defaults_never_live_in_the_working_directory() {
        // The platform directory is used when the environment names one; the
        // relative fallback only exists for environments without a home.
        let config = Config::default();
        let relative = config.data_dir == Path::new("data");
        assert_eq!(relative, platform_dirs().is_none());
    }

    #[test]
    fn config_file_is_found_in_the_data_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("localreel.toml");
        std::fs::write(&path, "[server]\nport = 4321\n").unwrap();
        let found = find_config_file(dir.path(), None).unwrap();
        assert_eq!(found.as_deref(), Some(path.as_path()));
    }

    #[test]
    fn legacy_data_directory_is_not_adopted_when_platform_db_exists() {
        let platform = tempfile::tempdir().unwrap();
        std::fs::write(platform.path().join("localreel.db"), b"").unwrap();
        assert!(adopt_legacy_data_dir(platform.path()).is_none());
    }

    #[test]
    fn adoption_prefers_the_first_directory_with_a_database() {
        let platform = tempfile::tempdir().unwrap();
        let empty = tempfile::tempdir().unwrap();
        let populated = tempfile::tempdir().unwrap();
        std::fs::write(populated.path().join("localreel.db"), b"").unwrap();

        let candidates = vec![empty.path().to_path_buf(), populated.path().to_path_buf()];
        assert_eq!(
            adopt_from(platform.path(), candidates),
            Some(populated.path().to_path_buf())
        );
    }

    #[test]
    fn adoption_ignores_the_platform_directory_itself() {
        let platform = tempfile::tempdir().unwrap();
        std::fs::write(platform.path().join("localreel.db"), b"").unwrap();
        let candidates = vec![platform.path().to_path_buf()];
        assert!(adopt_from(platform.path(), candidates).is_none());
    }
}
