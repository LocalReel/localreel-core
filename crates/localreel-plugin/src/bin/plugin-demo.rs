//! Reference storage plugin: exposes one local directory over the sidecar
//! protocol. It exists to exercise the package/runtime contract end to end and
//! as the template for real providers (WebDAV, SMB, …).
//!
//! Configuration (`LOCALREEL_PLUGIN_CONFIG`):
//! - `root` — directory to serve (required);
//! - `public_http_base` — when set (e.g. `http://plugin.local`), entries are
//!   reported as HTTP URIs under that base instead of `file://` URIs, which is
//!   how a remote provider behaves.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use localreel_kernel::storage::MediaUri;
use localreel_plugin::server::{self, Entry, Handler, HandlerError, Stat};
use serde_json::{json, Value};

struct LocalHandler {
    root: PathBuf,
    public_http_base: Option<String>,
}

impl LocalHandler {
    fn resolve(&self, uri: &str) -> Result<PathBuf, HandlerError> {
        let parsed = MediaUri::parse(uri)
            .map_err(|error| HandlerError::failed(format!("invalid uri: {error}")))?;
        let path = match parsed {
            MediaUri::Local(path) => path,
            MediaUri::Http(url) => {
                let base = self
                    .public_http_base
                    .as_deref()
                    .ok_or_else(|| HandlerError::failed("http uri without a public base"))?;
                let relative = url.strip_prefix(base).ok_or(HandlerError::NotFound)?;
                self.root
                    .join(server::percent_decode(relative.trim_start_matches('/')))
            }
            _ => return Err(HandlerError::NotFound),
        };
        let canonical = path.canonicalize().map_err(|_| HandlerError::NotFound)?;
        if canonical.starts_with(&self.root) {
            Ok(canonical)
        } else {
            Err(HandlerError::NotFound)
        }
    }

    fn describe(&self, path: &Path) -> Option<Entry> {
        let metadata = std::fs::metadata(path).ok()?;
        let name = path.file_name()?.to_string_lossy().into_owned();
        let uri = if let Some(base) = &self.public_http_base {
            let relative = path.strip_prefix(&self.root).ok()?;
            let suffix = relative
                .components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            format!("{base}/{suffix}")
        } else {
            MediaUri::Local(path.to_path_buf()).to_uri()
        };
        Some(Entry {
            uri,
            name,
            is_dir: metadata.is_dir(),
            size: metadata.len(),
            modified: modified(&metadata),
        })
    }
}

impl Handler for LocalHandler {
    fn stat(&self, uri: &str) -> Result<Stat, HandlerError> {
        let path = self.resolve(uri)?;
        let metadata = std::fs::metadata(&path).map_err(|_| HandlerError::NotFound)?;
        Ok(Stat {
            size: metadata.len(),
            modified: modified(&metadata),
            is_dir: metadata.is_dir(),
        })
    }

    fn list(&self, uri: &str, recursive: bool) -> Result<Vec<Entry>, HandlerError> {
        let root = self.resolve(uri)?;
        let mut entries = Vec::new();
        if recursive {
            for entry in walkdir::WalkDir::new(&root).follow_links(false) {
                let Ok(entry) = entry else { continue };
                if entry.path() == root {
                    continue;
                }
                if let Some(item) = self.describe(entry.path()) {
                    entries.push(item);
                }
            }
        } else {
            let directory = std::fs::read_dir(&root).map_err(|_| HandlerError::NotFound)?;
            for entry in directory.flatten() {
                if let Some(item) = self.describe(&entry.path()) {
                    entries.push(item);
                }
            }
        }
        Ok(entries)
    }

    fn read(&self, uri: &str, start: u64, len: u64) -> Result<Vec<u8>, HandlerError> {
        let path = self.resolve(uri)?;
        let mut file = std::fs::File::open(&path).map_err(|_| HandlerError::NotFound)?;
        let size = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        if start >= size {
            return Ok(Vec::new());
        }
        file.seek(SeekFrom::Start(start))
            .map_err(|error| HandlerError::failed(format!("cannot seek: {error}")))?;
        let length = len.min(size - start) as usize;
        let mut body = vec![0u8; length];
        let mut read = 0usize;
        while read < length {
            match file.read(&mut body[read..]) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(error) => return Err(HandlerError::failed(format!("cannot read: {error}"))),
            }
        }
        body.truncate(read);
        Ok(body)
    }
}

fn main() {
    let config: Value = std::env::var("LOCALREEL_PLUGIN_CONFIG")
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| json!({}));
    let root = config
        .get("root")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .expect("LOCALREEL_PLUGIN_CONFIG must contain root");
    let root = root.canonicalize().expect("plugin root must exist");
    let handler = LocalHandler {
        root,
        public_http_base: config
            .get("public_http_base")
            .and_then(Value::as_str)
            .map(|value| value.trim_end_matches('/').to_string()),
    };
    let transport = std::env::var("LOCALREEL_PLUGIN_TRANSPORT").unwrap_or_else(|_| "http".into());
    let result = if transport == "stdio" {
        server::run_stdio(Box::new(handler))
    } else {
        server::run(Box::new(handler))
    };
    if let Err(error) = result {
        eprintln!("reference plugin failed: {error}");
        std::process::exit(1);
    }
}

fn modified(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64)
}
