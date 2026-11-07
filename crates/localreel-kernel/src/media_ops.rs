use std::env;
#[cfg(windows)]
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{KernelError, Result};

/// What the installed ffmpeg/ffprobe pair can do.
///
/// A full ffmpeg build is ~100 MB, which is a real cost on a Windows first
/// install or a small NAS. Probing only needs `ffprobe`, so a "probe-only"
/// install is a supported configuration: the library is catalogued, posters are
/// generated, and nothing encodes until a full ffmpeg appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FfmpegCapability {
    /// ffmpeg and ffprobe: probing, thumbnails, transcodes and HLS.
    Full,
    /// ffprobe only: the library can be scanned and catalogued, nothing encodes.
    ProbeOnly,
    /// Neither binary is usable.
    Missing,
}

impl FfmpegCapability {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::ProbeOnly => "probe-only",
            Self::Missing => "missing",
        }
    }

    /// Whether thumbnails, transcodes and HLS packaging are possible.
    pub const fn can_transcode(self) -> bool {
        matches!(self, Self::Full)
    }

    /// Whether a file can be probed (scan metadata, thumbnails aside).
    pub const fn can_probe(self) -> bool {
        matches!(self, Self::Full | Self::ProbeOnly)
    }
}

/// One stream reported by `ffprobe`.
///
/// Everything the track menu needs: which audio streams exist, what language the
/// container says they are, and whether one is marked default.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeStream {
    /// ffmpeg stream index, the value `-map 0:<index>` expects.
    pub index: i64,
    /// `video`, `audio`, `subtitle`, `data`, ...
    pub codec_type: String,
    pub codec_name: Option<String>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub channels: Option<i32>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub is_default: bool,
    pub is_forced: bool,
}

/// Metadata extracted from `ffprobe`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub duration: Option<f64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub bitrate: Option<i64>,
    /// Every stream, in container order.
    pub streams: Vec<ProbeStream>,
}

/// Input to one ffprobe/ffmpeg invocation.
///
/// Local files are opened directly. Remote sources are served by the host's own
/// stream endpoint (`/api/v1/media/<id>/stream`), which supports byte ranges, so
/// ffmpeg reads remote media without downloading it first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MediaInput {
    Path(PathBuf),
    Url(String),
}

impl MediaInput {
    pub fn path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }

    pub fn url(url: impl Into<String>) -> Self {
        Self::Url(url.into())
    }

    pub fn is_url(&self) -> bool {
        matches!(self, Self::Url(_))
    }

    pub fn display(&self) -> String {
        match self {
            Self::Path(path) => path.display().to_string(),
            Self::Url(url) => url.clone(),
        }
    }

    fn as_os(&self) -> std::ffi::OsString {
        match self {
            Self::Path(path) => path.as_os_str().to_owned(),
            Self::Url(url) => url.into(),
        }
    }
}

/// Hardware acceleration requested for transcoding.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HwAccel {
    /// Use the best encoder this machine actually has, else the CPU.
    #[default]
    Auto,
    /// Always encode on the CPU.
    None,
    Nvenc,
    Qsv,
    Vaapi,
    Videotoolbox,
}

/// `(accelerator, codec family, ffmpeg encoder name)`.
const HARDWARE_ENCODERS: &[(HwAccel, &str, &str)] = &[
    (HwAccel::Nvenc, "h264", "h264_nvenc"),
    (HwAccel::Nvenc, "hevc", "hevc_nvenc"),
    (HwAccel::Qsv, "h264", "h264_qsv"),
    (HwAccel::Qsv, "hevc", "hevc_qsv"),
    (HwAccel::Vaapi, "h264", "h264_vaapi"),
    (HwAccel::Vaapi, "hevc", "hevc_vaapi"),
    (HwAccel::Videotoolbox, "h264", "h264_videotoolbox"),
    (HwAccel::Videotoolbox, "hevc", "hevc_videotoolbox"),
];

impl HwAccel {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(Self::Auto),
            "none" | "off" | "cpu" => Some(Self::None),
            "nvenc" => Some(Self::Nvenc),
            "qsv" => Some(Self::Qsv),
            "vaapi" => Some(Self::Vaapi),
            "videotoolbox" => Some(Self::Videotoolbox),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::None => "none",
            Self::Nvenc => "nvenc",
            Self::Qsv => "qsv",
            Self::Vaapi => "vaapi",
            Self::Videotoolbox => "videotoolbox",
        }
    }

    /// The ffmpeg encoder that implements this accelerator for `video_codec`.
    ///
    /// Presets name an encoder (`libx264`), not a codec family, so both spellings
    /// are accepted. `copy` and codecs nobody accelerates return `None`, which
    /// keeps the CPU path in charge.
    pub fn encoder_for(self, video_codec: &str) -> Option<&'static str> {
        let family = match video_codec {
            "h264" | "libx264" | "x264" => "h264",
            "hevc" | "h265" | "libx265" | "x265" => "hevc",
            _ => return None,
        };
        HARDWARE_ENCODERS
            .iter()
            .find(|(accel, codec, _)| *accel == self && *codec == family)
            .map(|(_, _, name)| *name)
    }
}

/// Options for one transcode. Extend this instead of growing the argument list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranscodeOptions {
    /// The accelerator to use. `Auto` must be resolved before it gets here.
    pub hwaccel: HwAccel,
    /// ffmpeg stream index of the audio track to keep.
    ///
    /// `None` means "the first audio track" 鈥?the behaviour every earlier
    /// release had, which keeps the existing variant names valid.
    pub audio_stream: Option<usize>,
}

/// Everything one transcode needs besides the input, output and preset.
#[derive(Debug, Clone, Copy)]
pub struct TranscodeRequest<'a> {
    pub options: TranscodeOptions,
    /// Source length, used to turn ffmpeg's progress into a 0..1 ratio.
    pub duration_secs: Option<f64>,
    /// Polled while ffmpeg runs; set it to stop the encode.
    pub cancel: &'a AtomicBool,
}

/// Everything one HLS packaging run needs.
#[derive(Debug, Clone, Copy)]
pub struct HlsRequest<'a> {
    pub segment_secs: u32,
    pub duration_secs: Option<f64>,
    /// ffmpeg stream index of the audio to keep; `None` keeps the first track.
    pub audio_stream: Option<usize>,
    pub cancel: &'a AtomicBool,
}

/// The encoders an ffmpeg build reports.
///
/// `ffmpeg -encoders` is the only portable way to know whether `h264_nvenc`
/// exists on this machine; the list is parsed once per process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncoderSet {
    names: std::collections::BTreeSet<String>,
}

impl EncoderSet {
    /// Parse the table printed by `ffmpeg -hide_banner -encoders`.
    pub fn parse(text: &str) -> Self {
        let mut names = std::collections::BTreeSet::new();
        for line in text.lines() {
            let mut fields = line.split_whitespace();
            let Some(_flags) = fields.next() else {
                continue;
            };
            let Some(name) = fields.next() else {
                continue;
            };
            if name.len() > 1
                && name
                    .chars()
                    .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_')
            {
                names.insert(name.to_string());
            }
        }
        Self { names }
    }

    pub fn has(&self, name: &str) -> bool {
        self.names.contains(name)
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    /// The first accelerator this build supports on this platform.
    pub fn hardware(&self) -> Option<HwAccel> {
        // VAAPI is a Linux interface and VideoToolbox is Apple's; asking for
        // them elsewhere only produces a failed transcode.
        let candidates: &[HwAccel] = if cfg!(target_os = "linux") {
            &[HwAccel::Nvenc, HwAccel::Qsv, HwAccel::Vaapi]
        } else if cfg!(target_os = "macos") {
            &[HwAccel::Videotoolbox]
        } else {
            &[HwAccel::Nvenc, HwAccel::Qsv]
        };
        candidates.iter().copied().find(|accel| {
            accel
                .encoder_for("h264")
                .is_some_and(|encoder| self.has(encoder))
        })
    }
}

/// Resolve a configured preference against what the build can do.
pub fn resolve_hwaccel(preference: HwAccel, encoders: &EncoderSet) -> HwAccel {
    match preference {
        HwAccel::None => HwAccel::None,
        HwAccel::Auto => encoders.hardware().unwrap_or(HwAccel::None),
        explicit => {
            let usable = explicit
                .encoder_for("h264")
                .is_some_and(|encoder| encoders.has(encoder));
            if usable {
                explicit
            } else {
                HwAccel::None
            }
        }
    }
}

/// Whether ffmpeg's failure looks like "this machine cannot do hardware
/// encoding" rather than "this file is broken".
///
/// Only these failures are retried on the CPU: retrying a corrupt input would
/// just fail twice and hide the real error.
pub fn is_hardware_failure(stderr: &str) -> bool {
    const MARKERS: &[&str] = &[
        "no capable devices",
        "cannot load",
        "invalid device",
        "device not found",
        "failed to initialise",
        "hardware accelerator failed",
        "impossible to convert between the formats",
    ];
    let lower = stderr.to_ascii_lowercase();
    MARKERS.iter().any(|marker| lower.contains(marker))
}

/// Typed transcode profile. Extensions never pass raw ffmpeg argv; they pick a
/// named preset that the host knows how to execute.
#[derive(Debug, Clone)]
pub struct TranscodePreset {
    pub name: String,
    pub video_codec: String,
    pub audio_codec: String,
    pub crf: u8,
    pub speed: String,
    pub audio_bitrate: String,
}

impl Default for TranscodePreset {
    fn default() -> Self {
        Self {
            name: "h264_aac".to_string(),
            video_codec: "libx264".to_string(),
            audio_codec: "aac".to_string(),
            crf: 23,
            speed: "veryfast".to_string(),
            audio_bitrate: "160k".to_string(),
        }
    }
}

/// Typed media operations backed by external `ffmpeg`/`ffprobe`.
///
/// All calls have a timeout and a cancellation flag, and children are killed on
/// timeout/cancel. This is the only place in the workspace that spawns ffmpeg.
#[derive(Debug, Clone)]
pub struct MediaOps {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    probe_timeout: Duration,
    thumbnail_timeout: Duration,
    transcode_timeout: Duration,
    /// Configured preference; resolved against [`MediaOps::encoders`].
    hwaccel_preference: HwAccel,
    /// `ffmpeg -encoders`, detected once (the call costs a spawn).
    encoders: Arc<std::sync::OnceLock<EncoderSet>>,
}

impl MediaOps {
    pub fn new(
        ffmpeg: PathBuf,
        ffprobe: PathBuf,
        probe_timeout: Duration,
        thumbnail_timeout: Duration,
        transcode_timeout: Duration,
    ) -> Self {
        Self {
            ffmpeg,
            ffprobe,
            probe_timeout,
            thumbnail_timeout,
            transcode_timeout,
            hwaccel_preference: HwAccel::Auto,
            encoders: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Choose how hardware acceleration is picked (`auto` by default).
    pub fn with_hwaccel(mut self, preference: HwAccel) -> Self {
        self.hwaccel_preference = preference;
        self
    }

    /// The encoders this ffmpeg build reports, detected on first use.
    pub fn encoders(&self) -> &EncoderSet {
        self.encoders.get_or_init(|| self.detect_encoders())
    }

    /// The accelerator to use right now: the configured one when it works,
    /// otherwise the best available, otherwise the CPU.
    pub fn hwaccel(&self) -> HwAccel {
        resolve_hwaccel(self.hwaccel_preference, self.encoders())
    }

    fn detect_encoders(&self) -> EncoderSet {
        if !command_available(&self.ffmpeg) {
            return EncoderSet::default();
        }
        let mut command = binary_command(&self.ffmpeg);
        command.args(["-hide_banner", "-encoders"]);
        match run_capture(
            &mut command,
            Duration::from_secs(10),
            &AtomicBool::new(false),
        ) {
            Ok(output) if output.status_success => {
                EncoderSet::parse(&String::from_utf8_lossy(&output.stdout))
            }
            Ok(_) | Err(_) => EncoderSet::default(),
        }
    }

    /// Build with ffmpeg/ffprobe discovery.
    ///
    /// Explicit config/PATH wins; otherwise look next to the executable, in
    /// `ffmpeg/bin`, `tools/ffmpeg/bin`, and `LOCALREEL_FFMPEG_DIR`. This keeps
    /// a portable package working when ffmpeg is shipped beside `localreel.exe`
    /// without requiring PATH changes.
    pub fn discover(
        ffmpeg: PathBuf,
        ffprobe: PathBuf,
        probe_timeout: Duration,
        thumbnail_timeout: Duration,
        transcode_timeout: Duration,
    ) -> Self {
        let (ffmpeg, ffprobe) = discover_binaries(&ffmpeg, &ffprobe);
        Self::new(
            ffmpeg,
            ffprobe,
            probe_timeout,
            thumbnail_timeout,
            transcode_timeout,
        )
    }

    pub fn ffmpeg_path(&self) -> &Path {
        &self.ffmpeg
    }

    pub fn ffprobe_path(&self) -> &Path {
        &self.ffprobe
    }

    pub fn available(&self) -> bool {
        self.capability().can_transcode()
    }

    /// What the resolved binaries can actually do.
    pub fn capability(&self) -> FfmpegCapability {
        capability_from(
            command_available(&self.ffmpeg),
            command_available(&self.ffprobe),
        )
    }

    pub fn probe(&self, input: &MediaInput, cancel: &AtomicBool) -> Result<ProbeResult> {
        let mut command = binary_command(&self.ffprobe);
        command.args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ]);
        command.arg(input.as_os());
        let output = run_capture(&mut command, self.probe_timeout, cancel)?;
        if !output.status_success {
            // The input path and ffprobe's stderr are diagnostics: the response
            // body gets the summary, the administrator gets the rest.
            return Err(KernelError::media_detail(
                "ffprobe could not read this file",
                format!(
                    "input={}; stderr: {}",
                    input.display(),
                    tail_text(&output.stderr, 4096)
                ),
            ));
        }
        parse_probe_json(&output.stdout)
    }

    pub fn thumbnail(
        &self,
        input: &MediaInput,
        output: &Path,
        at_secs: f64,
        width: u32,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut command = binary_command(&self.ffmpeg);
        command
            .args(["-y", "-v", "error", "-ss"])
            .arg(format!("{at_secs:.3}"))
            .arg("-i")
            .arg(input.as_os())
            .args(["-frames:v", "1", "-vf"])
            .arg(format!("scale={width}:-1"))
            .args(["-q:v", "5"])
            .arg(output);
        let result = run_capture(&mut command, self.thumbnail_timeout, cancel);
        match result {
            Ok(output) if output.status_success => Ok(()),
            Ok(output) => Err(KernelError::media_detail(
                "ffmpeg could not generate a thumbnail",
                format!(
                    "input={}; stderr: {}",
                    input.display(),
                    tail_text(&output.stderr, 4096)
                ),
            )),
            Err(error) => {
                let _ = std::fs::remove_file(output);
                Err(error)
            }
        }
    }

    /// Render one sprite sheet of `input` into `output`.
    ///
    /// The sheet is a 10x10 grid sampled every `interval` seconds; the player
    /// turns it into progress-bar previews through the WebVTT the media layer
    /// generates.
    pub fn preview_sprite(
        &self,
        input: &MediaInput,
        output: &Path,
        interval: f64,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut command = binary_command(&self.ffmpeg);
        command.args(build_preview_args(input, output, interval));
        let result = run_capture(&mut command, self.thumbnail_timeout, cancel);
        match result {
            Ok(captured) if captured.status_success => Ok(()),
            Ok(captured) => Err(KernelError::media_detail(
                "ffmpeg could not build a preview sprite",
                format!(
                    "input={}; stderr: {}",
                    input.display(),
                    tail_text(&captured.stderr, 4096)
                ),
            )),
            Err(error) => {
                let _ = std::fs::remove_file(output);
                Err(error)
            }
        }
    }

    /// Package `input` as an HLS VOD playlist into `output_dir`.
    ///
    /// Segments are MPEG-TS slices with a keyframe at each boundary; the
    /// playlist is written last, so a truncated run never leaves a valid-looking
    /// playlist behind.
    pub fn package_hls(
        &self,
        input: &MediaInput,
        output_dir: &Path,
        request: &HlsRequest<'_>,
        mut on_progress: impl FnMut(f64),
    ) -> Result<()> {
        let request = *request;
        let duration_secs = request.duration_secs;
        let cancel = request.cancel;
        std::fs::create_dir_all(output_dir)?;
        let mut command = binary_command(&self.ffmpeg);
        command.args(build_hls_args(input, output_dir, &request));
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|error| {
            KernelError::Media(format!(
                "failed to spawn {}: {error}",
                self.ffmpeg.display()
            ))
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| KernelError::Media("ffmpeg stdout unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| KernelError::Media("ffmpeg stderr unavailable".into()))?;

        let (progress_tx, progress_rx) = mpsc::channel::<f64>();
        let progress_reader = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(|line| line.ok()) {
                if let Some(value) = line.strip_prefix("out_time_ms=") {
                    if let Ok(micros) = value.trim().parse::<f64>() {
                        let _ = progress_tx.send(micros / 1_000_000.0);
                    }
                }
            }
        });
        let stderr_reader = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = Vec::new();
            let _ = reader.read_to_end(&mut buffer);
            buffer
        });

        let started = Instant::now();
        let result = loop {
            for seconds in progress_rx.try_iter() {
                if let Some(total) = duration_secs.filter(|total| *total > 0.0) {
                    on_progress((seconds / total).clamp(0.0, 0.999));
                }
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.success()),
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(KernelError::Media(format!(
                        "failed to query ffmpeg status: {error}"
                    )));
                }
            }
            if cancel.load(Ordering::Relaxed) {
                terminate(&mut child);
                break Err(KernelError::Media("hls packaging canceled".into()));
            }
            if started.elapsed() >= self.transcode_timeout {
                terminate(&mut child);
                break Err(KernelError::Media(format!(
                    "hls packaging timed out after {} seconds",
                    self.transcode_timeout.as_secs()
                )));
            }
            thread::sleep(Duration::from_millis(50));
        };

        for seconds in progress_rx.try_iter() {
            if let Some(total) = duration_secs.filter(|total| *total > 0.0) {
                on_progress((seconds / total).clamp(0.0, 0.999));
            }
        }
        let _ = progress_reader.join();
        let stderr = stderr_reader.join().unwrap_or_default();

        match result {
            Ok(true) => {
                if duration_secs.is_some() {
                    on_progress(1.0);
                }
                Ok(())
            }
            Ok(false) => {
                let _ = std::fs::remove_dir_all(output_dir);
                Err(KernelError::media_detail(
                    "ffmpeg could not package this file for HLS",
                    format!(
                        "input={}; stderr: {}",
                        input.display(),
                        tail_text(&stderr, 4096)
                    ),
                ))
            }
            Err(error) => {
                let _ = std::fs::remove_dir_all(output_dir);
                Err(error)
            }
        }
    }

    /// Transcode `input` into `output` using a typed preset.
    ///
    /// `duration_secs` is used to turn ffmpeg's `out_time_ms` progress into a
    /// 0..1 ratio. The callback is invoked from the calling thread.
    pub fn transcode(
        &self,
        input: &MediaInput,
        output: &Path,
        preset: &TranscodePreset,
        request: &TranscodeRequest<'_>,
        mut on_progress: impl FnMut(f64),
    ) -> Result<()> {
        let request = *request;
        let duration_secs = request.duration_secs;
        let cancel = request.cancel;
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut command = binary_command(&self.ffmpeg);
        command.args(build_transcode_args(
            input,
            output,
            preset,
            &request.options,
        ));

        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = command.spawn().map_err(|error| {
            KernelError::Media(format!(
                "failed to spawn {}: {error}",
                self.ffmpeg.display()
            ))
        })?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| KernelError::Media("ffmpeg stdout unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| KernelError::Media("ffmpeg stderr unavailable".into()))?;

        let (progress_tx, progress_rx) = mpsc::channel::<f64>();
        let progress_reader = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(|line| line.ok()) {
                if let Some(value) = line.strip_prefix("out_time_ms=") {
                    if let Ok(micros) = value.trim().parse::<f64>() {
                        let _ = progress_tx.send(micros / 1_000_000.0);
                    }
                }
            }
        });
        let stderr_reader = thread::spawn(move || {
            let mut reader = BufReader::new(stderr);
            let mut buffer = Vec::new();
            let _ = reader.read_to_end(&mut buffer);
            buffer
        });

        let started = Instant::now();
        let result = loop {
            for seconds in progress_rx.try_iter() {
                if let Some(total) = duration_secs.filter(|total| *total > 0.0) {
                    on_progress((seconds / total).clamp(0.0, 0.999));
                }
            }

            match child.try_wait() {
                Ok(Some(status)) => break Ok(status.success()),
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(KernelError::Media(format!(
                        "failed to query ffmpeg status: {error}"
                    )));
                }
            }

            if cancel.load(Ordering::Relaxed) {
                terminate(&mut child);
                break Err(KernelError::Media("transcode canceled".into()));
            }
            if started.elapsed() >= self.transcode_timeout {
                terminate(&mut child);
                break Err(KernelError::Media(format!(
                    "transcode timed out after {} seconds",
                    self.transcode_timeout.as_secs()
                )));
            }
            thread::sleep(Duration::from_millis(50));
        };

        // Drain any last progress events before joining the reader.
        for seconds in progress_rx.try_iter() {
            if let Some(total) = duration_secs.filter(|total| *total > 0.0) {
                on_progress((seconds / total).clamp(0.0, 0.999));
            }
        }
        let _ = progress_reader.join();
        let stderr = stderr_reader.join().unwrap_or_default();

        match result {
            Ok(true) => {
                if duration_secs.is_some() {
                    on_progress(1.0);
                }
                Ok(())
            }
            Ok(false) => {
                let _ = std::fs::remove_file(output);
                Err(KernelError::media_detail(
                    "ffmpeg could not transcode this file",
                    format!(
                        "input={}; output={}; stderr: {}",
                        input.display(),
                        output.display(),
                        tail_text(&stderr, 8192)
                    ),
                ))
            }
            Err(error) => {
                let _ = std::fs::remove_file(output);
                Err(error)
            }
        }
    }
}

/// Parse ffprobe JSON into the subset LocalReel needs.
pub fn parse_probe_json(bytes: &[u8]) -> Result<ProbeResult> {
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| KernelError::Media(format!("invalid ffprobe JSON: {error}")))?;
    let duration = value["format"]["duration"]
        .as_str()
        .and_then(|value| value.parse::<f64>().ok());
    let bitrate = value["format"]["bit_rate"]
        .as_str()
        .and_then(|value| value.parse::<i64>().ok());

    let streams = value["streams"].as_array();
    let video = streams.and_then(|streams| {
        streams
            .iter()
            .find(|stream| stream["codec_type"] == "video")
    });
    let audio = streams.and_then(|streams| {
        streams
            .iter()
            .find(|stream| stream["codec_type"] == "audio")
    });
    let details: Vec<ProbeStream> = streams
        .map(|streams| streams.iter().filter_map(parse_stream).collect())
        .unwrap_or_default();

    Ok(ProbeResult {
        duration,
        width: video.and_then(|stream| stream["width"].as_i64().map(|value| value as i32)),
        height: video.and_then(|stream| stream["height"].as_i64().map(|value| value as i32)),
        video_codec: video
            .and_then(|stream| stream["codec_name"].as_str())
            .map(str::to_string),
        audio_codec: match audio {
            Some(stream) => stream["codec_name"].as_str().map(str::to_string),
            None => Some("none".to_string()),
        },
        bitrate,
        streams: details,
    })
}

/// Parse one element of `streams[]`. A stream without an index cannot be mapped
/// later, so it is dropped rather than stored half-known.
fn parse_stream(value: &serde_json::Value) -> Option<ProbeStream> {
    let index = value["index"].as_i64()?;
    Some(ProbeStream {
        index,
        codec_type: value["codec_type"].as_str().unwrap_or_default().to_string(),
        codec_name: value["codec_name"].as_str().map(str::to_string),
        language: value["tags"]["language"].as_str().map(str::to_string),
        title: value["tags"]["title"].as_str().map(str::to_string),
        channels: value["channels"].as_i64().map(|value| value as i32),
        width: value["width"].as_i64().map(|value| value as i32),
        height: value["height"].as_i64().map(|value| value as i32),
        is_default: value["disposition"]["default"].as_i64() == Some(1),
        is_forced: value["disposition"]["forced"].as_i64() == Some(1),
    })
}

struct CommandOutput {
    status_success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn run_capture(
    command: &mut Command,
    timeout: Duration,
    cancel: &AtomicBool,
) -> Result<CommandOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().map_err(|error| {
        KernelError::Media(format!(
            "failed to spawn '{}': {error}",
            command.get_program().to_string_lossy()
        ))
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| KernelError::Media("child stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| KernelError::Media("child stderr unavailable".into()))?;

    let stdout_reader = thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    });
    let stderr_reader = thread::spawn(move || {
        let mut reader = BufReader::new(stderr);
        let mut buffer = Vec::new();
        let _ = reader.read_to_end(&mut buffer);
        buffer
    });

    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => {
                terminate(&mut child);
                break Err(KernelError::Media(format!(
                    "failed to query child status: {error}"
                )));
            }
        }
        if cancel.load(Ordering::Relaxed) {
            terminate(&mut child);
            break Err(KernelError::Media("operation canceled".into()));
        }
        if started.elapsed() >= timeout {
            terminate(&mut child);
            break Err(KernelError::Media(format!(
                "operation timed out after {} seconds",
                timeout.as_secs()
            )));
        }
        thread::sleep(Duration::from_millis(25));
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();
    let status = status?;
    Ok(CommandOutput {
        status_success: status.success(),
        stdout,
        stderr,
    })
}

/// VAAPI render node. The usual one on a Linux desktop or NAS with an iGPU.
pub const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Preview sprite geometry: a 10x10 grid of 160 px wide tiles.
///
/// The grid is fixed and the sampling interval is derived from the duration, so
/// a two-hour film and a ten-minute episode both produce one small image.
pub const PREVIEW_COLUMNS: u32 = 10;
pub const PREVIEW_ROWS: u32 = 10;
pub const PREVIEW_TILE_WIDTH: u32 = 160;

/// Seconds between two preview frames for a file of `duration`.
///
/// Never below one second: a sprite sheet full of duplicate frames is useless.
pub fn preview_interval(duration: Option<f64>) -> f64 {
    let total = duration.filter(|value| *value > 0.0).unwrap_or(600.0);
    let slots = f64::from(PREVIEW_COLUMNS * PREVIEW_ROWS);
    (total / slots).max(1.0)
}

/// Build the ffmpeg arguments for a sprite sheet.
///
/// Pure so the sampling maths is unit-tested; the encode itself runs through
/// [`MediaOps::preview_sprite`].
pub fn build_preview_args(
    input: &MediaInput,
    output: &Path,
    interval: f64,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    let mut args: Vec<OsString> = ["-y", "-v", "error", "-i"]
        .iter()
        .map(OsString::from)
        .collect();
    args.push(input.as_os());
    args.push(OsString::from("-vf"));
    args.push(OsString::from(format!(
        "fps=1/{interval:.6},scale={PREVIEW_TILE_WIDTH}:-1,tile={PREVIEW_COLUMNS}x{PREVIEW_ROWS}"
    )));
    args.extend(["-frames:v", "1", "-q:v", "5"].iter().map(OsString::from));
    args.push(output.as_os_str().to_owned());
    args
}

/// The `-map` value for an audio track.
///
/// `?` keeps a video-only file working: without it ffmpeg fails when the source
/// has no audio at all, which is a legitimate library item.
fn audio_map(stream: Option<usize>) -> String {
    match stream {
        Some(index) => format!("0:{index}?"),
        None => "0:a:0?".to_string(),
    }
}

/// Build the ffmpeg arguments for an HLS package.
///
/// Pure, like [`build_transcode_args`], so the audio mapping is testable without
/// running ffmpeg.
pub fn build_hls_args(
    input: &MediaInput,
    output_dir: &Path,
    request: &HlsRequest<'_>,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    fn text(value: &str) -> OsString {
        OsString::from(value)
    }

    let mut args: Vec<OsString> = ["-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-i"]
        .iter()
        .map(|value| text(value))
        .collect();
    args.push(input.as_os());
    args.extend(["-map", "0:v:0", "-map"].iter().map(|value| text(value)));
    args.push(text(&audio_map(request.audio_stream)));
    args.extend(
        [
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-crf",
            "23",
            "-pix_fmt",
            "yuv420p",
            "-c:a",
            "aac",
            "-b:a",
            "160k",
            "-f",
            "hls",
            "-hls_time",
        ]
        .iter()
        .map(|value| text(value)),
    );
    args.push(text(&request.segment_secs.to_string()));
    args.extend(
        [
            "-hls_playlist_type",
            "vod",
            "-hls_flags",
            "independent_segments",
            "-hls_segment_filename",
        ]
        .iter()
        .map(|value| text(value)),
    );
    args.push(output_dir.join("seg_%05d.ts").into_os_string());
    args.push(output_dir.join("index.m3u8").into_os_string());
    args
}

/// Build the ffmpeg arguments for a typed preset.
///
/// Pure on purpose: every hardware/CPU combination is covered by a snapshot test
/// instead of by watching a real encode.
///
/// `copy` means "remux this stream as-is": encoder-only flags (`-preset`,
/// `-crf`, `-b:a`) are dropped, which turns a multi-minute re-encode of an
/// H.264 file into a few seconds of container rewriting.
pub fn build_transcode_args(
    input: &MediaInput,
    output: &Path,
    preset: &TranscodePreset,
    options: &TranscodeOptions,
) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;

    fn text(value: &str) -> OsString {
        OsString::from(value)
    }

    let mut args: Vec<OsString> = ["-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-i"]
        .iter()
        .map(|value| text(value))
        .collect();
    args.push(input.as_os());
    // Keep the web output clean: video + one audio track. Subtitles, data
    // streams and chapters are dropped 鈥?the mp4 muxer otherwise writes
    // chapters as an extra `text` track that no browser player needs.
    args.extend(["-map", "0:v:0", "-map"].iter().map(|value| text(value)));
    args.push(text(&audio_map(options.audio_stream)));
    args.extend(
        ["-sn", "-dn", "-map_chapters", "-1"]
            .iter()
            .map(|value| text(value)),
    );

    let hardware = options
        .hwaccel
        .encoder_for(&preset.video_codec)
        .filter(|_| preset.video_codec != "copy");
    match hardware {
        Some(encoder) => {
            if options.hwaccel == HwAccel::Vaapi {
                // Surfaces must be uploaded to the render node before the
                // encoder sees them, so the device and the filter precede `-c:v`.
                args.extend(
                    [
                        "-vaapi_device",
                        DEFAULT_VAAPI_DEVICE,
                        "-vf",
                        "format=nv12,hwupload",
                    ]
                    .iter()
                    .map(|value| text(value)),
                );
            }
            args.push(text("-c:v"));
            args.push(text(encoder));
            // Hardware encoders have their own quality knobs; `-crf`/`-preset`
            // belong to libx264 and would be rejected here.
            let quality: &[&str] = match options.hwaccel {
                HwAccel::Nvenc => &["-cq", "23", "-preset", "p5"],
                HwAccel::Qsv => &["-global_quality", "23"],
                HwAccel::Vaapi => &["-qp", "23"],
                HwAccel::Videotoolbox => &["-q:v", "65"],
                HwAccel::Auto | HwAccel::None => &[],
            };
            args.extend(quality.iter().map(|value| text(value)));
        }
        None => {
            args.push(text("-c:v"));
            args.push(text(&preset.video_codec));
            if preset.video_codec != "copy" {
                args.extend([
                    text("-preset"),
                    text(&preset.speed),
                    text("-crf"),
                    text(&preset.crf.to_string()),
                ]);
            }
        }
    }

    args.push(text("-c:a"));
    args.push(text(&preset.audio_codec));
    if preset.audio_codec != "copy" {
        args.push(text("-b:a"));
        args.push(text(&preset.audio_bitrate));
    }
    args.extend(
        ["-movflags", "+faststart", "-f", "mp4"]
            .iter()
            .map(|value| text(value)),
    );
    args.push(output.as_os_str().to_owned());
    args
}

fn terminate(child: &mut Child) {
    #[cfg(windows)]
    {
        // Kill the whole tree. On Windows the resolved ffmpeg can be a shim or
        // wrapper that spawns the real encoder; killing only the direct child
        // would leave the encoder running as an orphan.
        let _ = Command::new("taskkill")
            .args(["/PID", &child.id().to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn discover_binaries(configured_ffmpeg: &Path, configured_ffprobe: &Path) -> (PathBuf, PathBuf) {
    let explicit = |path: &Path| {
        path.parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
    };
    if explicit(configured_ffmpeg)
        && explicit(configured_ffprobe)
        && command_available(configured_ffmpeg)
        && command_available(configured_ffprobe)
    {
        return (
            configured_ffmpeg.to_path_buf(),
            configured_ffprobe.to_path_buf(),
        );
    }

    let ffmpeg_names: &[&str] = if cfg!(windows) {
        &["ffmpeg.exe", "ffmpeg.cmd", "ffmpeg.bat"]
    } else {
        &["ffmpeg"]
    };
    let ffprobe_names: &[&str] = if cfg!(windows) {
        &["ffprobe.exe", "ffprobe.cmd", "ffprobe.bat"]
    } else {
        &["ffprobe"]
    };
    let push_names = |candidates: &mut Vec<PathBuf>, base: &Path| {
        for name in ffmpeg_names {
            candidates.push(base.join(name));
        }
    };

    let mut candidates = Vec::new();
    if let Some(dir) = env::var_os("LOCALREEL_FFMPEG_DIR") {
        let dir = PathBuf::from(dir);
        push_names(&mut candidates, &dir);
        push_names(&mut candidates, &dir.join("bin"));
    }
    if let Ok(executable) = env::current_exe() {
        if let Some(parent) = executable.parent() {
            push_names(&mut candidates, parent);
            for subdir in ["ffmpeg", "ffmpeg/bin", "tools/ffmpeg/bin", "bin"] {
                push_names(&mut candidates, &parent.join(subdir));
            }
        }
    }
    if let Ok(cwd) = env::current_dir() {
        push_names(&mut candidates, &cwd);
        for subdir in ["ffmpeg", "ffmpeg/bin", "tools/ffmpeg/bin", "bin"] {
            push_names(&mut candidates, &cwd.join(subdir));
        }
    }
    for configured in [configured_ffmpeg, configured_ffprobe] {
        if let Some(parent) = configured.parent() {
            if !parent.as_os_str().is_empty() {
                push_names(&mut candidates, parent);
            }
        }
    }

    for candidate in candidates {
        if !command_available(&candidate) {
            continue;
        }
        for probe_name in ffprobe_names {
            let ffprobe = candidate.with_file_name(probe_name);
            if command_available(&ffprobe) {
                return (candidate, ffprobe);
            }
        }
    }

    // Bare configured names (the default) resolve through PATH last, after
    // bundled/explicit directories.
    if command_available(configured_ffmpeg) && command_available(configured_ffprobe) {
        return (
            configured_ffmpeg.to_path_buf(),
            configured_ffprobe.to_path_buf(),
        );
    }

    if command_available(configured_ffmpeg) {
        for probe_name in ffprobe_names {
            let ffprobe = configured_ffmpeg.with_file_name(probe_name);
            if command_available(&ffprobe) {
                return (configured_ffmpeg.to_path_buf(), ffprobe);
            }
        }
    }

    (
        configured_ffmpeg.to_path_buf(),
        configured_ffprobe.to_path_buf(),
    )
}

fn binary_command(binary: &Path) -> Command {
    #[cfg(windows)]
    {
        let extension = binary
            .extension()
            .and_then(OsStr::to_str)
            .unwrap_or_default();
        if extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat") {
            let mut command = Command::new("cmd.exe");
            command.arg("/C").arg(binary);
            return command;
        }
    }
    Command::new(binary)
}

/// Decide the capability from the two availability probes.
///
/// Pure so the decision (and its table) can be tested without binaries.
fn capability_from(ffmpeg: bool, ffprobe: bool) -> FfmpegCapability {
    match (ffmpeg, ffprobe) {
        (true, true) => FfmpegCapability::Full,
        (false, true) => FfmpegCapability::ProbeOnly,
        // ffprobe is what makes the library readable; ffmpeg alone cannot probe,
        // so the pair is meaningless without it.
        _ => FfmpegCapability::Missing,
    }
}

fn command_available(binary: &Path) -> bool {
    binary_command(binary)
        .arg("-version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn tail_text(bytes: &[u8], max: usize) -> String {
    let start = bytes.len().saturating_sub(max);
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_probe_json() {
        let json = br#"{
            "format": {"duration": "12.5", "bit_rate": "800000"},
            "streams": [
                {"codec_type": "video", "codec_name": "h264", "width": 1280, "height": 720},
                {"codec_type": "audio", "codec_name": "aac"}
            ]
        }"#;
        let result = parse_probe_json(json).unwrap();
        assert_eq!(result.duration, Some(12.5));
        assert_eq!(result.width, Some(1280));
        assert_eq!(result.height, Some(720));
        assert_eq!(result.video_codec.as_deref(), Some("h264"));
        assert_eq!(result.audio_codec.as_deref(), Some("aac"));
        assert_eq!(result.bitrate, Some(800_000));
    }

    #[cfg(windows)]
    #[test]
    fn probe_runs_a_fake_cmd_harness() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake_ffprobe.cmd");
        std::fs::write(
            &script,
            "@echo off\r\necho {\"format\":{\"duration\":\"2.0\",\"bit_rate\":\"123\"},\"streams\":[{\"codec_type\":\"video\",\"codec_name\":\"h264\",\"width\":320,\"height\":240},{\"codec_type\":\"audio\",\"codec_name\":\"aac\"}]}\r\n",
        )
        .unwrap();
        let input = dir.path().join("a.mp4");
        std::fs::write(&input, b"not a real video").unwrap();

        let ops = MediaOps::new(
            script.clone(),
            script,
            Duration::from_secs(5),
            Duration::from_secs(5),
            Duration::from_secs(5),
        );
        assert!(ops.available());
        let probe = ops
            .probe(&MediaInput::path(input), &AtomicBool::new(false))
            .unwrap();
        assert_eq!(probe.duration, Some(2.0));
        assert_eq!(probe.width, Some(320));
        assert_eq!(probe.height, Some(240));
        assert_eq!(probe.video_codec.as_deref(), Some("h264"));
        assert_eq!(probe.audio_codec.as_deref(), Some("aac"));
        assert_eq!(probe.bitrate, Some(123));
    }

    #[test]
    fn capability_depends_on_the_binaries_that_exist() {
        assert_eq!(capability_from(true, true), FfmpegCapability::Full);
        assert_eq!(capability_from(false, true), FfmpegCapability::ProbeOnly);
        // ffmpeg without ffprobe cannot catalogue anything.
        assert_eq!(capability_from(true, false), FfmpegCapability::Missing);
        assert_eq!(capability_from(false, false), FfmpegCapability::Missing);

        assert!(FfmpegCapability::Full.can_probe());
        assert!(FfmpegCapability::Full.can_transcode());
        assert!(FfmpegCapability::ProbeOnly.can_probe());
        assert!(!FfmpegCapability::ProbeOnly.can_transcode());
        assert!(!FfmpegCapability::Missing.can_probe());
        assert_eq!(FfmpegCapability::ProbeOnly.as_str(), "probe-only");
    }

    #[test]
    fn missing_audio_is_recorded_as_none() {
        let json = br#"{
            "format": {"duration": "1"},
            "streams": [{"codec_type": "video", "codec_name": "vp9"}]
        }"#;
        let result = parse_probe_json(json).unwrap();
        assert_eq!(result.audio_codec.as_deref(), Some("none"));
    }

    fn args_of(preset: &TranscodePreset) -> Vec<String> {
        args_with(preset, HwAccel::None)
    }

    fn args_with(preset: &TranscodePreset, hwaccel: HwAccel) -> Vec<String> {
        args_with_audio(preset, hwaccel, None)
    }

    fn args_with_audio(
        preset: &TranscodePreset,
        hwaccel: HwAccel,
        audio_stream: Option<usize>,
    ) -> Vec<String> {
        build_transcode_args(
            &MediaInput::path("in.mkv"),
            Path::new("out.mp4"),
            preset,
            &TranscodeOptions {
                hwaccel,
                audio_stream,
            },
        )
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect()
    }

    #[test]
    fn transcode_args_accept_a_url_input() {
        let args: Vec<String> = build_transcode_args(
            &MediaInput::url("http://127.0.0.1:3001/api/v1/media/7/stream"),
            Path::new("out.mp4"),
            &TranscodePreset::default(),
            &TranscodeOptions::default(),
        )
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-i", "http://127.0.0.1:3001/api/v1/media/7/stream"]));
    }

    #[test]
    fn transcode_args_drop_subtitles_data_and_chapters() {
        // The mp4 muxer would otherwise turn chapters into an extra `text`
        // track, which no browser player uses and which bloats the variant.
        let args = args_of(&TranscodePreset::default());
        assert!(args.windows(2).any(|pair| pair == ["-sn", "-dn"]));
        assert!(args.windows(2).any(|pair| pair == ["-map_chapters", "-1"]));
        assert_eq!(args.iter().filter(|arg| *arg == "-map_chapters").count(), 1);
    }

    #[test]
    fn transcode_args_keep_encoder_flags_for_a_reencode() {
        let args = args_of(&TranscodePreset::default());
        assert!(args.contains(&"libx264".to_string()));
        assert!(args.contains(&"-preset".to_string()));
        assert!(args.contains(&"veryfast".to_string()));
        assert!(args.contains(&"-crf".to_string()));
        assert!(args.contains(&"23".to_string()));
        assert!(args.contains(&"aac".to_string()));
        assert!(args.contains(&"-b:a".to_string()));
        assert_eq!(args.last().map(String::as_str), Some("out.mp4"));
    }

    #[test]
    fn hardware_encoders_replace_libx264_and_its_quality_flags() {
        let preset = TranscodePreset::default();
        let nvenc = args_with(&preset, HwAccel::Nvenc);
        assert!(nvenc.windows(2).any(|pair| pair == ["-c:v", "h264_nvenc"]));
        assert!(nvenc.windows(2).any(|pair| pair == ["-cq", "23"]));
        assert!(nvenc.windows(2).any(|pair| pair == ["-preset", "p5"]));
        // `-crf`/`-preset veryfast` are libx264 flags; ffmpeg rejects them here.
        assert!(!nvenc.contains(&"-crf".to_string()));
        assert!(!nvenc.contains(&"veryfast".to_string()));
        assert!(!nvenc.contains(&"libx264".to_string()));

        let qsv = args_with(&preset, HwAccel::Qsv);
        assert!(qsv.windows(2).any(|pair| pair == ["-c:v", "h264_qsv"]));
        assert!(qsv.windows(2).any(|pair| pair == ["-global_quality", "23"]));

        let apple = args_with(&preset, HwAccel::Videotoolbox);
        assert!(apple
            .windows(2)
            .any(|pair| pair == ["-c:v", "h264_videotoolbox"]));
        assert!(apple.windows(2).any(|pair| pair == ["-q:v", "65"]));

        // Audio handling is unchanged on every path.
        for args in [&nvenc, &qsv, &apple] {
            assert!(args.windows(2).any(|pair| pair == ["-c:a", "aac"]));
            assert_eq!(args.last().map(String::as_str), Some("out.mp4"));
        }
    }

    #[test]
    fn vaapi_uploads_to_the_render_node_before_the_encoder() {
        let args = args_with(&TranscodePreset::default(), HwAccel::Vaapi);
        let device = args
            .iter()
            .position(|arg| arg == "-vaapi_device")
            .expect("vaapi needs a device");
        assert_eq!(args[device + 1], DEFAULT_VAAPI_DEVICE);
        let upload = args
            .iter()
            .position(|arg| arg == "-vf")
            .expect("vaapi needs the upload filter");
        assert_eq!(args[upload + 1], "format=nv12,hwupload");
        let encoder = args
            .iter()
            .position(|arg| arg == "-c:v")
            .expect("an encoder is required");
        // The device and the filter must be configured before the encoder.
        assert!(device < encoder);
        assert!(upload < encoder);
        assert_eq!(args[encoder + 1], "h264_vaapi");
        assert!(args.windows(2).any(|pair| pair == ["-qp", "23"]));
        assert!(!args.contains(&"-crf".to_string()));
    }

    #[test]
    fn hevc_presets_pick_the_hevc_encoder() {
        let preset = TranscodePreset {
            name: "hevc".into(),
            video_codec: "hevc".into(),
            audio_codec: "aac".into(),
            crf: 26,
            speed: "veryfast".into(),
            audio_bitrate: "160k".into(),
        };
        let args = args_with(&preset, HwAccel::Nvenc);
        assert!(args.windows(2).any(|pair| pair == ["-c:v", "hevc_nvenc"]));
        // A codec no accelerator provides stays on the CPU.
        let av1 = TranscodePreset {
            video_codec: "libsvtav1".into(),
            ..preset.clone()
        };
        let cpu = args_with(&av1, HwAccel::Nvenc);
        assert!(cpu.windows(2).any(|pair| pair == ["-c:v", "libsvtav1"]));
        assert!(!cpu.contains(&"hevc_nvenc".to_string()));
    }

    #[test]
    fn a_remux_ignores_hardware_entirely() {
        let preset = TranscodePreset {
            name: "remux".into(),
            video_codec: "copy".into(),
            audio_codec: "copy".into(),
            crf: 23,
            speed: "veryfast".into(),
            audio_bitrate: "160k".into(),
        };
        let args = args_with(&preset, HwAccel::Nvenc);
        assert!(args.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(!args.contains(&"h264_nvenc".to_string()));
        assert!(!args.contains(&"-vaapi_device".to_string()));
    }

    #[test]
    fn the_encoder_table_is_parsed_from_ffmpeg_output() {
        let table = "\
Encoders:
 V..... = Video
 A..... = Audio
 ------
 V....D libx264              libx264 H.264 / AVC (codec h264)
 V....D h264_nvenc           NVIDIA NVENC H.264 encoder (codec h264)
 V....D h264_vaapi           H.264/MPEG-4 AVC (VAAPI) (codec h264)
 A....D aac                  AAC (Advanced Audio Coding)
";
        let set = EncoderSet::parse(table);
        assert!(set.has("libx264"));
        assert!(set.has("h264_nvenc"));
        assert!(set.has("h264_vaapi"));
        assert!(set.has("aac"));
        // Header and legend lines must not become encoder names.
        assert!(!set.has("Video"));
        assert!(!set.has("Encoders:"));
        assert!(!EncoderSet::parse("").has("libx264"));
    }

    #[test]
    fn acceleration_resolution_prefers_what_the_machine_has() {
        let nvenc = EncoderSet::parse(" V....D h264_nvenc NVIDIA\n");
        assert_eq!(resolve_hwaccel(HwAccel::Nvenc, &nvenc), HwAccel::Nvenc);
        // Asked for something this build does not have: CPU.
        assert_eq!(resolve_hwaccel(HwAccel::Vaapi, &nvenc), HwAccel::None);
        assert_eq!(resolve_hwaccel(HwAccel::None, &nvenc), HwAccel::None);
        // Nothing detected at all: CPU.
        assert_eq!(
            resolve_hwaccel(HwAccel::Auto, &EncoderSet::default()),
            HwAccel::None
        );
        // Auto picks the first usable accelerator on this platform.
        let both = EncoderSet::parse(" V....D h264_nvenc NVIDIA\n V....D h264_qsv Intel\n");
        let resolved = resolve_hwaccel(HwAccel::Auto, &both);
        if cfg!(any(target_os = "linux", target_os = "windows")) {
            assert_eq!(resolved, HwAccel::Nvenc);
        } else if cfg!(target_os = "macos") {
            assert_eq!(resolved, HwAccel::None);
        }
    }

    #[test]
    fn only_hardware_failures_justify_a_cpu_retry() {
        assert!(is_hardware_failure(
            "Cannot load libnvidia-encode.so.1\n Impossible to convert between the formats"
        ));
        assert!(is_hardware_failure(
            "[h264_nvenc @ 0x1] No capable devices found"
        ));
        assert!(is_hardware_failure("Invalid device: /dev/dri/renderD128"));
        // A broken file must not be retried: it would fail twice and hide why.
        assert!(!is_hardware_failure(
            "Invalid data found when processing input"
        ));
        assert!(!is_hardware_failure(""));
    }

    #[test]
    fn a_selected_audio_track_maps_its_stream_index() {
        let preset = TranscodePreset::default();
        let default = args_with(&preset, HwAccel::None);
        assert!(default.windows(2).any(|pair| pair == ["-map", "0:a:0?"]));

        let chosen = args_with_audio(&preset, HwAccel::None, Some(3));
        assert!(chosen.windows(2).any(|pair| pair == ["-map", "0:3?"]));
        assert!(!chosen.contains(&"0:a:0?".to_string()));
        // Exactly one audio map: two would mux both tracks into the output.
        assert_eq!(chosen.iter().filter(|arg| arg.starts_with("0:")).count(), 2);
    }

    #[test]
    fn hls_packaging_can_pick_an_audio_track_too() {
        let dir = Path::new("hls-out");
        let request = HlsRequest {
            segment_secs: 4,
            duration_secs: None,
            audio_stream: Some(1),
            cancel: &AtomicBool::new(false),
        };
        let args: Vec<String> = build_hls_args(&MediaInput::path("in.mkv"), dir, &request)
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|pair| pair == ["-map", "0:1?"]));
        assert!(args.contains(&"4".to_string()));
        assert!(args.iter().any(|arg| arg.ends_with("index.m3u8")));
    }

    #[test]
    fn probe_json_reports_every_stream() {
        let json = br#"{
            "format": {"duration": "60.0"},
            "streams": [
                {"index": 0, "codec_type": "video", "codec_name": "h264",
                 "width": 1920, "height": 1080, "disposition": {"default": 1}},
                {"index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2,
                 "tags": {"language": "jpn", "title": "Japanese"},
                 "disposition": {"default": 1}},
                {"index": 2, "codec_type": "audio", "codec_name": "ac3", "channels": 6,
                 "tags": {"language": "eng"}, "disposition": {"default": 0, "forced": 0}}
            ]
        }"#;
        let probe = parse_probe_json(json).unwrap();
        assert_eq!(probe.streams.len(), 3);

        let video = &probe.streams[0];
        assert_eq!(video.codec_type, "video");
        assert_eq!(video.width, Some(1920));
        assert!(video.is_default);

        let japanese = &probe.streams[1];
        assert_eq!(japanese.index, 1);
        assert_eq!(japanese.codec_name.as_deref(), Some("aac"));
        assert_eq!(japanese.language.as_deref(), Some("jpn"));
        assert_eq!(japanese.title.as_deref(), Some("Japanese"));
        assert_eq!(japanese.channels, Some(2));
        assert!(japanese.is_default);

        let english = &probe.streams[2];
        assert_eq!(english.index, 2);
        assert_eq!(english.language.as_deref(), Some("eng"));
        assert!(!english.is_default);
        assert!(!english.is_forced);

        // The legacy summary fields are still derived from the same payload.
        assert_eq!(probe.width, Some(1920));
        assert_eq!(probe.video_codec.as_deref(), Some("h264"));
        assert_eq!(probe.audio_codec.as_deref(), Some("aac"));
    }

    #[test]
    fn a_stream_without_an_index_is_dropped() {
        let json = br#"{"streams": [{"codec_type": "audio", "codec_name": "aac"}]}"#;
        let probe = parse_probe_json(json).unwrap();
        assert!(probe.streams.is_empty(), "an unmappable stream is useless");
    }

    #[test]
    fn transcode_args_drop_encoder_flags_for_copy() {
        let preset = TranscodePreset {
            name: "remux".into(),
            video_codec: "copy".into(),
            audio_codec: "copy".into(),
            crf: 23,
            speed: "veryfast".into(),
            audio_bitrate: "160k".into(),
        };
        let args = args_of(&preset);
        assert!(args.contains(&"copy".to_string()));
        // Encoder-only flags would make ffmpeg fail when copying streams.
        assert!(!args.contains(&"-preset".to_string()));
        assert!(!args.contains(&"-crf".to_string()));
        assert!(!args.contains(&"-b:a".to_string()));
        assert!(args.windows(2).any(|pair| pair == ["-c:v", "copy"]));
        assert!(args.windows(2).any(|pair| pair == ["-c:a", "copy"]));
    }

    #[test]
    fn packages_a_short_video_as_hls_when_ffmpeg_is_available() {
        let ops = MediaOps::discover(
            PathBuf::from("ffmpeg"),
            PathBuf::from("ffprobe"),
            Duration::from_secs(30),
            Duration::from_secs(60),
            Duration::from_secs(120),
        );
        if !ops.available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("localreel-hls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("in.mp4");
        let status = std::process::Command::new(ops.ffmpeg_path())
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=160x120:rate=10",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440",
                "-t",
                "2",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
            ])
            .arg(&input)
            .status()
            .expect("run ffmpeg");
        assert!(status.success());

        let output = dir.join("hls");
        ops.package_hls(
            &MediaInput::path(&input),
            &output,
            &HlsRequest {
                segment_secs: 1,
                duration_secs: Some(2.0),
                audio_stream: None,
                cancel: &AtomicBool::new(false),
            },
            |_| {},
        )
        .unwrap();
        assert!(output.join("index.m3u8").is_file());
        let segments = std::fs::read_dir(&output)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "ts"))
            .count();
        assert!(segments >= 1, "expected at least one HLS segment");
        let playlist = std::fs::read_to_string(output.join("index.m3u8")).unwrap();
        assert!(playlist.starts_with("#EXTM3U"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
