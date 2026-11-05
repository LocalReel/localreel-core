use std::env;
#[cfg(windows)]
use std::ffi::OsStr;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::{KernelError, Result};

/// Metadata extracted from `ffprobe`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProbeResult {
    pub duration: Option<f64>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub bitrate: Option<i64>,
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
        command_available(&self.ffmpeg) && command_available(&self.ffprobe)
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
            return Err(KernelError::Media(format!(
                "ffprobe failed for {}: {}",
                input.display(),
                tail_text(&output.stderr, 4096)
            )));
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
            Ok(output) => Err(KernelError::Media(format!(
                "ffmpeg thumbnail failed for {}: {}",
                input.display(),
                tail_text(&output.stderr, 4096)
            ))),
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
        segment_secs: u32,
        duration_secs: Option<f64>,
        cancel: &AtomicBool,
        mut on_progress: impl FnMut(f64),
    ) -> Result<()> {
        std::fs::create_dir_all(output_dir)?;
        let mut command = binary_command(&self.ffmpeg);
        command
            .args(["-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-i"])
            .arg(input.as_os())
            .args([
                "-map",
                "0:v:0",
                "-map",
                "0:a:0?",
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
            ])
            .arg(segment_secs.to_string())
            .args([
                "-hls_playlist_type",
                "vod",
                "-hls_flags",
                "independent_segments",
                "-hls_segment_filename",
            ])
            .arg(output_dir.join("seg_%05d.ts"))
            .arg(output_dir.join("index.m3u8"))
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
                Err(KernelError::Media(format!(
                    "ffmpeg hls packaging failed for {}: {}",
                    input.display(),
                    tail_text(&stderr, 4096)
                )))
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
        duration_secs: Option<f64>,
        cancel: &AtomicBool,
        mut on_progress: impl FnMut(f64),
    ) -> Result<()> {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut command = binary_command(&self.ffmpeg);
        command.args(transcode_args(input, output, preset));

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
                Err(KernelError::Media(format!(
                    "ffmpeg transcode failed: {}",
                    tail_text(&stderr, 8192)
                )))
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

/// Build the ffmpeg arguments for a typed preset.
///
/// `copy` means "remux this stream as-is": encoder-only flags (`-preset`,
/// `-crf`, `-b:a`) are dropped, which turns a multi-minute re-encode of an
/// H.264 file into a few seconds of container rewriting.
pub(crate) fn transcode_args(
    input: &MediaInput,
    output: &Path,
    preset: &TranscodePreset,
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
    // streams and chapters are dropped — the mp4 muxer otherwise writes
    // chapters as an extra `text` track that no browser player needs.
    args.extend(
        [
            "-map",
            "0:v:0",
            "-map",
            "0:a:0?",
            "-sn",
            "-dn",
            "-map_chapters",
            "-1",
            "-c:v",
        ]
        .iter()
        .map(|value| text(value)),
    );
    args.push(text(&preset.video_codec));
    if preset.video_codec != "copy" {
        args.extend([
            text("-preset"),
            text(&preset.speed),
            text("-crf"),
            text(&preset.crf.to_string()),
        ]);
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
    fn missing_audio_is_recorded_as_none() {
        let json = br#"{
            "format": {"duration": "1"},
            "streams": [{"codec_type": "video", "codec_name": "vp9"}]
        }"#;
        let result = parse_probe_json(json).unwrap();
        assert_eq!(result.audio_codec.as_deref(), Some("none"));
    }

    fn args_of(preset: &TranscodePreset) -> Vec<String> {
        transcode_args(&MediaInput::path("in.mkv"), Path::new("out.mp4"), preset)
            .into_iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn transcode_args_accept_a_url_input() {
        let args: Vec<String> = transcode_args(
            &MediaInput::url("http://127.0.0.1:3001/api/v1/media/7/stream"),
            Path::new("out.mp4"),
            &TranscodePreset::default(),
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
            1,
            Some(2.0),
            &AtomicBool::new(false),
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
