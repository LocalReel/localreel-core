//! Logging setup: console plus a rolling file.
//!
//! A desktop app launched from a shortcut loses its console when it exits, and
//! a service has no console at all — so every run also appends to
//! `<log dir>/localreel.log`. When that file grows past [`MAX_LOG_BYTES`] it is
//! rotated once to `localreel.log.1`.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

/// Rotate when the log file reaches this size.
pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

const LOG_FILE: &str = "localreel.log";
const ROTATED_FILE: &str = "localreel.log.1";

/// Initialise tracing once. Console output always happens; file output is best
/// effort so a read-only install directory cannot stop the server.
pub fn init(log_dir: &Path) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    let console = fmt::layer()
        .with_writer(std::io::stdout)
        .with_ansi(std::io::stdout().is_terminal_for_ansi());

    let file_layer = open_log_file(log_dir).map(|file| {
        fmt::layer()
            .with_writer(FileWriter::new(file))
            .with_ansi(false)
    });

    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file_layer)
        .try_init();
}

/// Path of the log file for a log directory.
pub fn log_file_path(log_dir: &Path) -> PathBuf {
    log_dir.join(LOG_FILE)
}

fn open_log_file(log_dir: &Path) -> Option<File> {
    if std::fs::create_dir_all(log_dir).is_err() {
        return None;
    }
    let path = log_file_path(log_dir);
    rotate_if_needed(&path);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()
}

fn rotate_if_needed(path: &Path) {
    let too_big = std::fs::metadata(path)
        .map(|metadata| metadata.len() >= MAX_LOG_BYTES)
        .unwrap_or(false);
    if !too_big {
        return;
    }
    let rotated = path.with_file_name(ROTATED_FILE);
    let _ = std::fs::remove_file(&rotated);
    let _ = std::fs::rename(path, rotated);
}

/// A `MakeWriter` that appends to one shared file handle.
#[derive(Clone)]
struct FileWriter {
    file: Arc<Mutex<File>>,
}

impl FileWriter {
    fn new(file: File) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
        }
    }
}

struct FileGuard<'a> {
    file: std::sync::MutexGuard<'a, File>,
}

impl Write for FileGuard<'_> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.file.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

impl<'a> fmt::MakeWriter<'a> for FileWriter {
    type Writer = FileGuard<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        FileGuard {
            file: self
                .file
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }
}

/// `std::io::IsTerminal` for ANSI decisions without importing the trait at the
/// call site.
trait AnsiTerminal {
    fn is_terminal_for_ansi(&self) -> bool;
}

impl AnsiTerminal for std::io::Stdout {
    fn is_terminal_for_ansi(&self) -> bool {
        use std::io::IsTerminal;
        self.is_terminal()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_path_is_inside_the_directory() {
        let dir = PathBuf::from("C:/logs");
        assert_eq!(log_file_path(&dir), PathBuf::from("C:/logs/localreel.log"));
    }

    #[test]
    fn opens_and_appends() {
        let dir = tempfile::tempdir().unwrap();
        let file = open_log_file(dir.path()).expect("log file opens");
        drop(file);
        let file = open_log_file(dir.path()).expect("log file reopens");
        drop(file);
        let contents = std::fs::read_to_string(log_file_path(dir.path())).unwrap();
        assert!(contents.is_empty());
    }

    #[test]
    fn rotates_when_too_big() {
        let dir = tempfile::tempdir().unwrap();
        let path = log_file_path(dir.path());
        let big = vec![b'x'; MAX_LOG_BYTES as usize + 1];
        std::fs::write(&path, &big).unwrap();
        let file = open_log_file(dir.path()).unwrap();
        drop(file);
        assert!(dir.path().join(ROTATED_FILE).is_file());
        let size = std::fs::metadata(&path).unwrap().len();
        assert!(size < MAX_LOG_BYTES);
    }
}
