//! Downloading a release asset, refusing anything that does not match the
//! manifest byte for byte.
//!
//! The write goes to `<name>.part` and is only renamed once the size and the
//! SHA-256 match, so an interrupted or tampered download can never be mistaken
//! for a ready-to-run installer.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use sha2::{Digest, Sha256};

use crate::manifest::{to_hex, Asset};

/// A release asset is an installer; 200 MiB is far above anything this project
/// ships and far below anything that could fill a disk by accident.
pub const MAX_ASSET_BYTES: u64 = 200 * 1024 * 1024;

/// Read timeout for the whole transfer. Local networks are fast; this is only
/// here so a stalled connection cannot hold the update state forever.
const TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("only https downloads are allowed: {0}")]
    InsecureUrl(String),
    #[error("the asset {0} is larger than the {1} MiB limit")]
    TooLarge(String, u64),
    #[error("the download ended at {got} bytes, expected {expected}")]
    SizeMismatch { got: u64, expected: u64 },
    #[error("the sha256 is {got}, the manifest says {expected}")]
    DigestMismatch { got: String, expected: String },
    #[error("download failed: {0}")]
    Network(String),
    #[error("cannot write {path}: {error}")]
    Io { path: String, error: String },
}

/// Where the asset will be written, derived from the URL basename.
pub fn file_name_for(asset: &Asset) -> String {
    let from_url = asset
        .url
        .split(['?', '#'])
        .next()
        .unwrap_or(&asset.url)
        .rsplit('/')
        .next()
        .unwrap_or_default();
    let cleaned: String = from_url
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        .collect();
    if cleaned.is_empty() || cleaned.starts_with('.') {
        "update.bin".to_string()
    } else {
        cleaned
    }
}

/// Download `asset` into `directory` and return the verified path.
pub fn download(asset: &Asset, directory: &Path) -> Result<PathBuf, DownloadError> {
    download_with_progress(asset, directory, |_| {})
}

/// Same as [`download`], reporting the running byte count after every chunk.
pub fn download_with_progress(
    asset: &Asset,
    directory: &Path,
    mut progress: impl FnMut(u64),
) -> Result<PathBuf, DownloadError> {
    if !asset.url.starts_with("https://") {
        return Err(DownloadError::InsecureUrl(asset.url.clone()));
    }
    if asset.size > MAX_ASSET_BYTES {
        return Err(DownloadError::TooLarge(asset.url.clone(), MAX_ASSET_BYTES));
    }

    fs::create_dir_all(directory).map_err(|error| DownloadError::Io {
        path: directory.display().to_string(),
        error: error.to_string(),
    })?;

    let name = file_name_for(asset);
    let final_path = directory.join(&name);
    let part_path = directory.join(format!("{name}.part"));

    let agent = ureq::AgentBuilder::new().timeout(TIMEOUT).build();
    let response = agent
        .get(&asset.url)
        .set(
            "User-Agent",
            concat!("LocalReel/", env!("CARGO_PKG_VERSION")),
        )
        .call()
        .map_err(|error| DownloadError::Network(format!("{error}")))?;

    let mut reader = response.into_reader();
    let mut file = File::create(&part_path).map_err(|error| DownloadError::Io {
        path: part_path.display().to_string(),
        error: error.to_string(),
    })?;

    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer).map_err(|error| {
            let _ = fs::remove_file(&part_path);
            DownloadError::Network(error.to_string())
        })?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_ASSET_BYTES || (asset.size > 0 && total > asset.size) {
            let _ = fs::remove_file(&part_path);
            return Err(DownloadError::SizeMismatch {
                got: total,
                expected: asset.size,
            });
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .map_err(|error| DownloadError::Io {
                path: part_path.display().to_string(),
                error: error.to_string(),
            })?;
        progress(total);
    }
    drop(file);

    if asset.size > 0 && total != asset.size {
        let _ = fs::remove_file(&part_path);
        return Err(DownloadError::SizeMismatch {
            got: total,
            expected: asset.size,
        });
    }

    let got = to_hex(&hasher.finalize());
    if !asset.sha256.trim().is_empty() && !got.eq_ignore_ascii_case(asset.sha256.trim()) {
        let _ = fs::remove_file(&part_path);
        return Err(DownloadError::DigestMismatch {
            got,
            expected: asset.sha256.trim().to_string(),
        });
    }

    if final_path.exists() {
        let _ = fs::remove_file(&final_path);
    }
    fs::rename(&part_path, &final_path).map_err(|error| DownloadError::Io {
        path: final_path.display().to_string(),
        error: error.to_string(),
    })?;
    Ok(final_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(url: &str, size: u64) -> Asset {
        Asset {
            platform: crate::manifest::Platform::WindowsX64,
            kind: crate::manifest::AssetKind::Setup,
            url: url.to_string(),
            size,
            sha256: String::new(),
        }
    }

    #[test]
    fn refuses_plain_http() {
        let error =
            download(&asset("http://example.invalid/a.exe", 10), Path::new(".")).unwrap_err();
        assert!(matches!(error, DownloadError::InsecureUrl(_)));
    }

    #[test]
    fn refuses_oversized_assets_before_connecting() {
        let big = asset("https://example.invalid/a.exe", MAX_ASSET_BYTES + 1);
        let error = download(&big, Path::new(".")).unwrap_err();
        assert!(matches!(error, DownloadError::TooLarge(_, _)));
    }

    #[test]
    fn derives_a_safe_file_name() {
        assert_eq!(
            file_name_for(&asset("https://x/y/localreel-0.5.7-x64-setup.exe?sig=1", 1)),
            "localreel-0.5.7-x64-setup.exe"
        );
        // Anything that could climb out of the directory is dropped.
        assert_eq!(
            file_name_for(&asset("https://x/y/..%2f..%2fevil", 1)),
            "update.bin"
        );
        assert_eq!(file_name_for(&asset("https://x/y/", 1)), "update.bin");
    }
}
