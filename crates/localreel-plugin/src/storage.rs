//! `Storage` implementation backed by a sidecar process.
//!
//! The host asks the plugin over loopback HTTP; the plugin decides how to reach
//! the bytes (WebDAV, SMB, S3, …). The core never learns the remote protocol.

use std::io::Read;
use std::sync::Arc;

use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::storage::{FileStat, MediaUri, Storage, StorageEntry};
use serde::Deserialize;

use crate::http;
use crate::runtime::PluginRuntime;

#[derive(Deserialize)]
struct StatBody {
    size: u64,
    #[serde(default)]
    modified: Option<i64>,
    #[serde(default)]
    is_dir: bool,
}

#[derive(Deserialize)]
struct EntryBody {
    uri: String,
    name: String,
    #[serde(default)]
    is_dir: bool,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    modified: Option<i64>,
}

#[derive(Deserialize)]
struct EntriesBody {
    entries: Vec<EntryBody>,
}

pub struct PluginStorage {
    runtime: Arc<PluginRuntime>,
}

impl PluginStorage {
    pub fn new(runtime: Arc<PluginRuntime>) -> Self {
        Self { runtime }
    }

    fn request_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let response = self.runtime.request(path).map_err(map_error)?;
        let status = response.status;
        if status != 200 {
            return Err(map_status(status));
        }
        let mut body = Vec::new();
        response
            .take(8 * 1024 * 1024)
            .read_to_end(&mut body)
            .map_err(|error| KernelError::Storage(format!("plugin response: {error}")))?;
        serde_json::from_slice(&body)
            .map_err(|error| KernelError::Storage(format!("invalid plugin response: {error}")))
    }
}

impl Storage for PluginStorage {
    fn kind(&self) -> &'static str {
        "plugin"
    }

    fn stat(&self, uri: &MediaUri) -> Result<FileStat> {
        let body: StatBody =
            self.request_json(&format!("/v1/stat?uri={}", http::encode(&uri.to_uri())))?;
        Ok(FileStat {
            size: body.size,
            modified: body.modified,
            is_dir: body.is_dir,
        })
    }

    fn list(&self, uri: &MediaUri) -> Result<Vec<StorageEntry>> {
        let body: EntriesBody =
            self.request_json(&format!("/v1/list?uri={}", http::encode(&uri.to_uri())))?;
        Ok(body.entries.into_iter().map(entry).collect())
    }

    fn walk(
        &self,
        root: &MediaUri,
    ) -> Result<Box<dyn Iterator<Item = Result<StorageEntry>> + Send>> {
        let body: EntriesBody =
            self.request_json(&format!("/v1/walk?uri={}", http::encode(&root.to_uri())))?;
        let entries: Vec<Result<StorageEntry>> =
            body.entries.into_iter().map(entry).map(Ok).collect();
        Ok(Box::new(entries.into_iter()))
    }

    fn open_range(&self, uri: &MediaUri, start: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        let path = format!(
            "/v1/read?uri={}&start={start}&len={len}",
            http::encode(&uri.to_uri())
        );
        let response = self.runtime.request(&path).map_err(map_error)?;
        if response.status != 200 {
            return Err(map_status(response.status));
        }
        Ok(Box::new(response.take(len)))
    }
}

fn entry(body: EntryBody) -> StorageEntry {
    StorageEntry {
        uri: body.uri,
        name: body.name,
        is_dir: body.is_dir,
        size: body.size,
        modified: body.modified,
    }
}

fn map_error(error: crate::manifest::PluginError) -> KernelError {
    KernelError::Storage(format!("plugin request failed: {error}"))
}

fn map_status(status: u16) -> KernelError {
    match status {
        404 => KernelError::NotFound("plugin reported a missing object".into()),
        _ => KernelError::Storage(format!("plugin returned HTTP {status}")),
    }
}
