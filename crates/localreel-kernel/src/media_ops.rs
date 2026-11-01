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

    pub fn probe(&self, input: &Path, cancel: &AtomicBool) -> Result<ProbeResult> {
        let mut command = binary_command(&self.ffprobe);
        command.args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ]);
        command.arg(input);
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
        input: &Path,
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
            .arg(input)
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

    /// Transcode `input` into `output` using a typed preset.
    ///
    /// `duration_secs` is used to turn ffmpeg's `out_time_ms` progress into a
    /// 0..1 ratio. The callback is invoked from the calling thread.
    pub fn transcode(
        &self,
        input: &Path,
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
        command
            .args(["-y", "-v", "error", "-nostats", "-progress", "pipe:1", "-i"])
            .arg(input)
            .args(["-map", "0:v:0", "-map", "0:a:0?", "-c:v"])
            .arg(&preset.video_codec)
            .args(["-preset", &preset.speed])
            .args(["-crf", &preset.crf.to_string(), "-c:a"])
            .arg(&preset.audio_codec)
            .args(["-b:a", &preset.audio_bitrate])
            .args(["-movflags", "+faststart", "-f", "mp4"])
            .arg(output);

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

fn terminate(child: &mut Child) {
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
        let probe = ops.probe(&input, &AtomicBool::new(false)).unwrap();
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
}
