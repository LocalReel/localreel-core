use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

use crate::error::{KernelError, Result};

/// A media location. The URI is the serialized, cross-database identity; the
/// variants tell storage how to reach the bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MediaUri {
    Local(PathBuf),
    Http(String),
    S3 { bucket: String, key: String },
}

impl MediaUri {
    pub fn parse(input: &str) -> Result<Self> {
        let value = input.trim();
        if value.is_empty() {
            return Err(KernelError::InvalidInput(
                "media uri must not be empty".into(),
            ));
        }

        if has_prefix_ignore_ascii_case(value, "http://")
            || has_prefix_ignore_ascii_case(value, "https://")
        {
            return Ok(MediaUri::Http(value.to_string()));
        }

        if has_prefix_ignore_ascii_case(value, "s3://") {
            let rest = &value[5..];
            let (bucket, key) = rest.split_once('/').unwrap_or((rest, ""));
            if bucket.is_empty() {
                return Err(KernelError::InvalidInput("s3 uri requires a bucket".into()));
            }
            return Ok(MediaUri::S3 {
                bucket: bucket.to_string(),
                key: key.to_string(),
            });
        }

        if has_prefix_ignore_ascii_case(value, "file://") {
            return Ok(MediaUri::Local(parse_file_uri(&value[7..])?));
        }

        Ok(MediaUri::Local(PathBuf::from(value)))
    }

    pub fn to_uri(&self) -> String {
        match self {
            MediaUri::Local(path) => local_path_to_uri(path),
            MediaUri::Http(url) => url.clone(),
            MediaUri::S3 { bucket, key } => format!("s3://{bucket}/{key}"),
        }
    }

    pub fn local_path(&self) -> Option<&Path> {
        match self {
            MediaUri::Local(path) => Some(path),
            MediaUri::Http(_) | MediaUri::S3 { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FileStat {
    pub size: u64,
    pub modified: Option<i64>,
    pub is_dir: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct StorageEntry {
    pub uri: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
}

/// Storage backend contract. v1 only ships [`LocalStorage`]; HTTP/HLS is
/// passthrough handled by the media layer, and S3 is deliberately deferred.
pub trait Storage: Send + Sync {
    fn stat(&self, uri: &MediaUri) -> Result<FileStat>;
    fn list(&self, uri: &MediaUri) -> Result<Vec<StorageEntry>>;
    fn open_range(&self, uri: &MediaUri, start: u64, len: u64) -> Result<Box<dyn Read + Send>>;
}

/// Local filesystem backend with mandatory root confinement.
///
/// Every resolved path is canonicalized and must start with one of the
/// configured roots. This is the only place that enforces "which directories
/// may be read"; the media layer must not bypass it.
pub struct LocalStorage {
    roots: RwLock<Vec<PathBuf>>,
}

impl LocalStorage {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Result<Self> {
        let mut canonical_roots = Vec::new();
        for root in roots {
            canonical_roots.push(canonicalize_root(&root)?);
        }
        if canonical_roots.is_empty() {
            return Err(KernelError::Storage(
                "at least one storage root is required".into(),
            ));
        }
        Ok(Self {
            roots: RwLock::new(canonical_roots),
        })
    }

    pub fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Add a root at runtime; effective for the next `stat`/`list`/`open_range`.
    pub fn add_root(&self, root: &Path) -> Result<()> {
        let canonical = canonicalize_root(root)?;
        let mut roots = self
            .roots
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !roots.iter().any(|existing| existing == &canonical) {
            roots.push(canonical);
        }
        Ok(())
    }

    /// Remove a root at runtime. Removing the last root is rejected: an empty
    /// storage would deny every read and hide the configuration mistake.
    pub fn remove_root(&self, root: &Path) -> Result<bool> {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let mut roots = self
            .roots
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let before = roots.len();
        roots.retain(|existing| existing != &canonical);
        let removed = roots.len() != before;
        if roots.is_empty() {
            roots.push(canonical);
            return Err(KernelError::Storage(
                "cannot remove the last storage root".into(),
            ));
        }
        Ok(removed)
    }

    fn resolve(&self, uri: &MediaUri) -> Result<PathBuf> {
        let path = uri
            .local_path()
            .ok_or_else(|| KernelError::Storage("not a local media uri".into()))?;
        let canonical = path.canonicalize().map_err(|error| {
            KernelError::Storage(format!("cannot resolve {}: {error}", path.display()))
        })?;
        let roots = self
            .roots
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !roots.iter().any(|root| canonical.starts_with(root)) {
            return Err(KernelError::PermissionDenied(format!(
                "path is outside configured storage roots: {}",
                canonical.display()
            )));
        }
        Ok(canonical)
    }
}

fn canonicalize_root(root: &Path) -> Result<PathBuf> {
    let canonical = root.canonicalize().map_err(|error| {
        KernelError::Storage(format!(
            "cannot resolve storage root {}: {error}",
            root.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(KernelError::Storage(format!(
            "storage root is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

impl Storage for LocalStorage {
    fn stat(&self, uri: &MediaUri) -> Result<FileStat> {
        let path = self.resolve(uri)?;
        let metadata = std::fs::metadata(&path)?;
        Ok(FileStat {
            size: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|value| value.as_secs() as i64),
            is_dir: metadata.is_dir(),
        })
    }

    fn list(&self, uri: &MediaUri) -> Result<Vec<StorageEntry>> {
        let path = self.resolve(uri)?;
        let roots = self.roots();
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)? {
            let entry = entry?;
            let entry_path = entry.path();
            let metadata = entry.metadata()?;
            let canonical = entry_path.canonicalize().unwrap_or(entry_path);
            if !roots.iter().any(|root| canonical.starts_with(root)) {
                continue;
            }
            entries.push(StorageEntry {
                uri: MediaUri::Local(canonical).to_uri(),
                name: entry.file_name().to_string_lossy().into_owned(),
                is_dir: metadata.is_dir(),
                size: metadata.len(),
            });
        }
        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    fn open_range(&self, uri: &MediaUri, start: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        let path = self.resolve(uri)?;
        let mut file = File::open(&path)?;
        let size = file.metadata()?.len();
        if start >= size {
            return Ok(Box::new(std::io::empty()));
        }
        file.seek(SeekFrom::Start(start))?;
        Ok(Box::new(file.take(len.min(size - start))))
    }
}

fn has_prefix_ignore_ascii_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .map(|candidate| candidate.eq_ignore_ascii_case(prefix))
        .unwrap_or(false)
}

#[cfg(windows)]
fn parse_file_uri(rest: &str) -> Result<PathBuf> {
    // file:///D:/Videos/a.mp4  -> D:\Videos\a.mp4
    // file://server/share/x    -> \\server\share\x
    let decoded = percent_decode(rest);
    if let Some(stripped) = decoded.strip_prefix("//") {
        return Ok(PathBuf::from(format!(
            "\\\\{}",
            stripped.replace('/', "\\")
        )));
    }
    if decoded.len() >= 3
        && decoded.as_bytes()[0] == b'/'
        && decoded.as_bytes()[1].is_ascii_alphabetic()
        && decoded.as_bytes()[2] == b':'
    {
        return Ok(PathBuf::from(decoded[1..].replace('/', "\\")));
    }
    if decoded.len() >= 2
        && decoded.as_bytes()[0].is_ascii_alphabetic()
        && decoded.as_bytes()[1] == b':'
    {
        return Ok(PathBuf::from(decoded.replace('/', "\\")));
    }
    if decoded.starts_with('/') {
        return Ok(PathBuf::from(decoded.replace('/', "\\")));
    }
    // file://server/share
    Ok(PathBuf::from(format!("\\\\{}", decoded.replace('/', "\\"))))
}

#[cfg(not(windows))]
fn parse_file_uri(rest: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(percent_decode(rest)))
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let high = (bytes[index + 1] as char).to_digit(16);
            let low = (bytes[index + 2] as char).to_digit(16);
            if let (Some(high), Some(low)) = (high, low) {
                output.push((high * 16 + low) as u8);
                index += 3;
                continue;
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

#[cfg(windows)]
fn local_path_to_uri(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    // `canonicalize()` returns a verbatim path (`\\?\F:\...`). Strip that
    // prefix before building a URI, otherwise it becomes `file://?/F:/...`.
    let raw = strip_verbatim_prefix(&absolute.to_string_lossy()).replace('\\', "/");
    if let Some(rest) = raw.strip_prefix("//") {
        return format!("file://{rest}");
    }
    if raw.as_bytes().get(1) == Some(&b':') {
        return format!("file:///{raw}");
    }
    format!("file://{raw}")
}

/// Normalize a local path for prefix comparisons (strip Windows `\\?\`).
/// This does not canonicalize or touch the filesystem.
pub fn normalize_local_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(strip_verbatim_prefix(&path.to_string_lossy()))
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

#[cfg(windows)]
fn strip_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    if let Some(rest) = path.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    path.to_string()
}

#[cfg(not(windows))]
fn local_path_to_uri(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    format!("file://{}", absolute.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_windows_and_unc_file_uris() {
        assert_eq!(
            MediaUri::parse("file:///D:/Videos/a.mp4").unwrap(),
            MediaUri::Local(PathBuf::from(r"D:\Videos\a.mp4"))
        );
        assert_eq!(
            MediaUri::parse("file://server/share/a.mp4").unwrap(),
            MediaUri::Local(PathBuf::from(r"\\server\share\a.mp4"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn roundtrips_windows_paths() {
        let uri = MediaUri::Local(PathBuf::from(r"D:\Videos\a b.mp4"));
        assert_eq!(uri.to_uri(), "file:///D:/Videos/a b.mp4");
    }

    #[cfg(windows)]
    #[test]
    fn strips_verbatim_prefixes() {
        assert_eq!(
            strip_verbatim_prefix(r"\\?\F:\Videos\a.mp4"),
            r"F:\Videos\a.mp4"
        );
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\server\share\a.mp4"),
            r"\\server\share\a.mp4"
        );
    }

    #[test]
    fn rejects_uri_outside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("x.mp4"), b"x").unwrap();
        let storage = LocalStorage::new([root.clone()]).unwrap();
        let uri = MediaUri::Local(outside.join("x.mp4"));
        match storage.stat(&uri) {
            Err(KernelError::PermissionDenied(_)) => {}
            other => panic!("expected permission denied, got {other:?}"),
        }
    }

    #[test]
    fn dynamic_roots_take_effect_and_cjk_paths_work() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("视频库");
        let second = dir.path().join("第二个库");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let file = second.join("剧集 01.mp4");
        std::fs::write(&file, b"abc").unwrap();

        let storage = LocalStorage::new([first]).unwrap();
        let uri = MediaUri::Local(file.clone());
        assert!(matches!(
            storage.stat(&uri),
            Err(KernelError::PermissionDenied(_))
        ));

        storage.add_root(&second).unwrap();
        assert_eq!(storage.stat(&uri).unwrap().size, 3);
        assert!(storage.roots().contains(&second.canonicalize().unwrap()));

        assert!(storage.remove_root(&second).unwrap());
        assert!(matches!(
            storage.stat(&uri),
            Err(KernelError::PermissionDenied(_))
        ));
    }

    #[test]
    fn removing_the_last_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let storage = LocalStorage::new([dir.path().to_path_buf()]).unwrap();
        assert!(matches!(
            storage.remove_root(dir.path()),
            Err(KernelError::Storage(_))
        ));
        assert_eq!(storage.roots().len(), 1);
    }

    #[test]
    fn add_root_rejects_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let storage = LocalStorage::new([dir.path().to_path_buf()]).unwrap();
        assert!(matches!(
            storage.add_root(&file),
            Err(KernelError::Storage(_))
        ));
    }

    #[test]
    fn reads_byte_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"0123456789").unwrap();
        let storage = LocalStorage::new([dir.path().to_path_buf()]).unwrap();
        let mut reader = storage.open_range(&MediaUri::Local(file), 2, 3).unwrap();
        let mut buffer = Vec::new();
        reader.read_to_end(&mut buffer).unwrap();
        assert_eq!(buffer, b"234");
    }
}
