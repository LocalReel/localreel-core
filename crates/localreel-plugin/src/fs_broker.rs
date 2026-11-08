//! Host-side `fs.read` broker.
//!
//! A plugin names a file URI; the host resolves it, canonicalises it, and
//! refuses anything outside the roots granted to that plugin version. The
//! plugin never receives a host path it can open itself.
//!
//! Residual risk: the check and the open are separate syscalls. A hostile
//! process able to swap a symlink between them could escape the root. Plugins
//! in this design are single-user and read-only, so the practical exposure is
//! limited; a handle-based final-path check is the next hardening step.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use base64::Engine as _;
use localreel_kernel::error::{KernelError, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use url::Url;

pub const MAX_READ_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct FsRequest {
    /// `stat`, `list`, or `read`.
    pub op: String,
    /// `file://` URI.
    pub uri: String,
    #[serde(default)]
    pub start: u64,
    #[serde(default)]
    pub len: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FsEntry {
    pub uri: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified: Option<i64>,
}

pub fn execute(roots: &[String], request: &FsRequest) -> Result<Value> {
    let roots = canonical_roots(roots)?;
    if roots.is_empty() {
        return Err(KernelError::PermissionDenied(
            "fs.read has no granted roots".into(),
        ));
    }
    let target = resolve(&request.uri)?;
    if !under_any_root(&target, &roots) {
        return Err(KernelError::PermissionDenied(
            "fs.read target is outside the granted roots".into(),
        ));
    }
    match request.op.as_str() {
        "stat" => {
            let metadata = std::fs::metadata(&target)
                .map_err(|error| KernelError::Storage(format!("stat failed: {error}")))?;
            Ok(json!({
                "size": metadata.len(),
                "modified": modified(&metadata),
                "is_dir": metadata.is_dir(),
            }))
        }
        "list" => {
            let metadata = std::fs::metadata(&target)
                .map_err(|error| KernelError::Storage(format!("stat failed: {error}")))?;
            if !metadata.is_dir() {
                return Err(KernelError::InvalidInput(
                    "fs.read list target is not a directory".into(),
                ));
            }
            let mut entries = Vec::new();
            let reader = std::fs::read_dir(&target)
                .map_err(|error| KernelError::Storage(format!("list failed: {error}")))?;
            for entry in reader.flatten() {
                let path = entry.path();
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(_) => continue,
                };
                let uri = Url::from_file_path(&path)
                    .map_err(|_| KernelError::Storage("cannot encode a file URI".into()))?
                    .to_string();
                entries.push(FsEntry {
                    uri,
                    name: entry.file_name().to_string_lossy().into_owned(),
                    is_dir: metadata.is_dir(),
                    size: metadata.len(),
                    modified: modified(&metadata),
                });
            }
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(json!({ "entries": entries }))
        }
        "read" => {
            let len = request.len.min(MAX_READ_BYTES);
            let mut file = std::fs::File::open(&target)
                .map_err(|error| KernelError::Storage(format!("open failed: {error}")))?;
            verify_open_handle(&file, &roots)?;
            file.seek(SeekFrom::Start(request.start))
                .map_err(|error| KernelError::Storage(format!("seek failed: {error}")))?;
            let mut body = Vec::new();
            file.take(len)
                .read_to_end(&mut body)
                .map_err(|error| KernelError::Storage(format!("read failed: {error}")))?;
            Ok(json!({
                "body_b64": base64::engine::general_purpose::STANDARD.encode(body),
            }))
        }
        other => Err(KernelError::InvalidInput(format!(
            "unknown fs.read operation {other:?}"
        ))),
    }
}

fn canonical_roots(roots: &[String]) -> Result<Vec<PathBuf>> {
    let mut output = Vec::new();
    for root in roots {
        let path = root_to_path(root)?;
        let canonical = std::fs::canonicalize(&path).map_err(|error| {
            KernelError::PermissionDenied(format!(
                "fs.read root {} is not accessible: {error}",
                path.display()
            ))
        })?;
        output.push(canonical);
    }
    Ok(output)
}

fn root_to_path(root: &str) -> Result<PathBuf> {
    if root.starts_with("file://") {
        return url_to_path(root);
    }
    Ok(PathBuf::from(root))
}

fn resolve(uri: &str) -> Result<PathBuf> {
    let path = url_to_path(uri)?;
    std::fs::canonicalize(&path).map_err(|error| {
        KernelError::Storage(format!("cannot resolve {}: {error}", path.display()))
    })
}

fn url_to_path(uri: &str) -> Result<PathBuf> {
    let url = Url::parse(uri)
        .map_err(|error| KernelError::InvalidInput(format!("invalid file URI: {error}")))?;
    if url.scheme() != "file" {
        return Err(KernelError::InvalidInput(
            "fs.read only accepts file:// URIs".into(),
        ));
    }
    url.to_file_path()
        .map_err(|_| KernelError::InvalidInput("file URI has no local path".into()))
}

fn under_any_root(target: &Path, roots: &[PathBuf]) -> bool {
    let target = comparable(target);
    roots.iter().any(|root| {
        let root = comparable(root);
        if target == root {
            return true;
        }
        if root.ends_with(std::path::MAIN_SEPARATOR) {
            return target.starts_with(&root);
        }
        target
            .strip_prefix(&root)
            .is_some_and(|rest| rest.starts_with(std::path::MAIN_SEPARATOR))
    })
}

fn comparable(path: &Path) -> String {
    let mut text = path.to_string_lossy().into_owned();
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        text = stripped.to_string();
    }
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

#[cfg(windows)]
fn verify_open_handle(file: &std::fs::File, roots: &[PathBuf]) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFinalPathNameByHandleW, FILE_NAME_NORMALIZED, VOLUME_NAME_DOS,
    };

    let mut buffer = vec![0u16; 4096];
    let length = unsafe {
        GetFinalPathNameByHandleW(
            file.as_raw_handle() as _,
            buffer.as_mut_ptr(),
            buffer.len() as u32,
            FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
        )
    };
    if length == 0 || length as usize >= buffer.len() {
        return Err(KernelError::PermissionDenied(
            "cannot verify the opened file path".into(),
        ));
    }
    let path = String::from_utf16_lossy(&buffer[..length as usize]);
    let path = path.strip_prefix(r"\\?\").unwrap_or(&path);
    if under_any_root(Path::new(path), roots) {
        Ok(())
    } else {
        Err(KernelError::PermissionDenied(format!(
            "opened file escaped the granted roots: {path:?} not under {roots:?}"
        )))
    }
}

#[cfg(not(windows))]
fn verify_open_handle(_file: &std::fs::File, _roots: &[PathBuf]) -> Result<()> {
    Ok(())
}

fn modified(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> (tempfile::TempDir, tempfile::TempDir, String) {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), b"abcdef").unwrap();
        std::fs::create_dir(root.path().join("sub")).unwrap();
        std::fs::write(root.path().join("sub").join("b.txt"), b"12345").unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"no").unwrap();
        let root_uri = Url::from_file_path(root.path()).unwrap().to_string();
        (root, outside, root_uri)
    }

    fn request(op: &str, path: &Path) -> FsRequest {
        FsRequest {
            op: op.into(),
            uri: Url::from_file_path(path).unwrap().to_string(),
            start: 0,
            len: 0,
        }
    }

    #[test]
    fn stats_and_lists_inside_the_root() {
        let (dir, _outside, root_uri) = root();
        let stat = execute(
            std::slice::from_ref(&root_uri),
            &request("stat", &dir.path().join("a.txt")),
        )
        .unwrap();
        assert_eq!(stat["size"], 6);
        assert_eq!(stat["is_dir"], false);

        let list = execute(&[root_uri], &request("list", dir.path())).unwrap();
        let names: Vec<&str> = list["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["a.txt", "sub"]);
    }

    #[test]
    fn reads_a_bounded_range() {
        let (dir, _outside, root_uri) = root();
        let mut request = request("read", &dir.path().join("a.txt"));
        request.start = 1;
        request.len = 3;
        let reply = execute(&[root_uri], &request).unwrap();
        let body = base64::engine::general_purpose::STANDARD
            .decode(reply["body_b64"].as_str().unwrap())
            .unwrap();
        assert_eq!(body, b"bcd");
    }

    #[test]
    fn refuses_outside_paths_and_traversal() {
        let (dir, outside, root_uri) = root();
        let outside_file = outside.path().join("secret.txt");
        assert!(execute(
            std::slice::from_ref(&root_uri),
            &request("stat", &outside_file)
        )
        .is_err());

        let traversal = Url::parse(&format!(
            "{}/../x",
            Url::from_file_path(dir.path()).unwrap()
        ))
        .unwrap()
        .to_string();
        let error = execute(
            &[root_uri],
            &FsRequest {
                op: "stat".into(),
                uri: traversal,
                start: 0,
                len: 0,
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("outside") || error.to_string().contains("resolve"));
    }

    #[test]
    fn refuses_non_file_uris_and_empty_roots() {
        let (dir, _outside, _root_uri) = root();
        let request = FsRequest {
            op: "stat".into(),
            uri: "https://example.com/a".into(),
            start: 0,
            len: 0,
        };
        assert!(execute(&[], &request).is_err());
        assert!(execute(
            &[Url::from_file_path(dir.path()).unwrap().to_string()],
            &request
        )
        .is_err());
    }

    #[cfg(windows)]
    #[test]
    fn handle_final_path_must_stay_under_the_roots() {
        let (dir, outside, _root_uri) = root();
        let file = std::fs::File::open(dir.path().join("a.txt")).unwrap();
        let roots = vec![std::fs::canonicalize(outside.path()).unwrap()];
        assert!(verify_open_handle(&file, &roots).is_err());
        let roots = vec![std::fs::canonicalize(dir.path()).unwrap()];
        verify_open_handle(&file, &roots).unwrap();
    }
}
