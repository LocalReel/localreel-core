//! `Storage` implementation backed by a sidecar process.
//!
//! The host asks the plugin over loopback HTTP; the plugin decides how to reach
//! the bytes (WebDAV, SMB, S3, …). The core never learns the remote protocol.
//!
//! Every call passes through a [`Gate`]: a stop closes the gate first, waits for
//! the in-flight counter to reach zero, and only then takes the provider away
//! and kills the process. `open_range` returns a reader that holds its guard for
//! as long as the stream lives, because that call outlives the function.

use std::io::{Cursor, Read};
use std::sync::Arc;

use base64::Engine as _;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::storage::{FileStat, MediaUri, Storage, StorageEntry};
use serde::Deserialize;
use serde_json::json;

use crate::gate::{Gate, GuardedRead, InflightGuard};
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
    gate: Arc<Gate>,
}

impl PluginStorage {
    /// A provider with its own gate.
    pub fn new(runtime: Arc<PluginRuntime>) -> Self {
        Self {
            runtime,
            gate: Gate::new(),
        }
    }

    /// A provider whose gate the supervisor already owns, so it can drain it.
    pub fn with_gate(runtime: Arc<PluginRuntime>, gate: Arc<Gate>) -> Self {
        Self { runtime, gate }
    }

    pub fn gate(&self) -> &Arc<Gate> {
        &self.gate
    }

    pub fn runtime(&self) -> &Arc<PluginRuntime> {
        &self.runtime
    }

    fn request_json<T: for<'de> Deserialize<'de>>(&self, path: &str) -> Result<T> {
        let _guard = self.gate.enter()?;
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

    fn stdio_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<T> {
        let _guard = self.gate.enter()?;
        let value = self.runtime.stdio_call(method, params).map_err(map_error)?;
        serde_json::from_value(value)
            .map_err(|error| KernelError::Storage(format!("invalid plugin response: {error}")))
    }
}

impl Storage for PluginStorage {
    fn kind(&self) -> &'static str {
        "plugin"
    }

    fn stat(&self, uri: &MediaUri) -> Result<FileStat> {
        let uri = uri.to_uri();
        let body: StatBody = if self.runtime.is_stdio() {
            self.stdio_json("stat", json!({ "uri": uri }))?
        } else {
            self.request_json(&format!("/v1/stat?uri={}", http::encode(&uri)))?
        };
        Ok(FileStat {
            size: body.size,
            modified: body.modified,
            is_dir: body.is_dir,
        })
    }

    fn list(&self, uri: &MediaUri) -> Result<Vec<StorageEntry>> {
        let uri = uri.to_uri();
        let body: EntriesBody = if self.runtime.is_stdio() {
            self.stdio_json("list", json!({ "uri": uri, "recursive": false }))?
        } else {
            self.request_json(&format!("/v1/list?uri={}", http::encode(&uri)))?
        };
        Ok(body.entries.into_iter().map(entry).collect())
    }

    fn walk(
        &self,
        root: &MediaUri,
    ) -> Result<Box<dyn Iterator<Item = Result<StorageEntry>> + Send>> {
        let uri = root.to_uri();
        let body: EntriesBody = if self.runtime.is_stdio() {
            self.stdio_json("walk", json!({ "uri": uri, "recursive": true }))?
        } else {
            self.request_json(&format!("/v1/walk?uri={}", http::encode(&uri)))?
        };
        let entries: Vec<Result<StorageEntry>> =
            body.entries.into_iter().map(entry).map(Ok).collect();
        Ok(Box::new(entries.into_iter()))
    }

    fn open_range(&self, uri: &MediaUri, start: u64, len: u64) -> Result<Box<dyn Read + Send>> {
        if self.runtime.is_stdio() {
            let _guard = self.gate.enter()?;
            let value = self
                .runtime
                .stdio_call(
                    "read",
                    json!({ "uri": uri.to_uri(), "start": start, "len": len }),
                )
                .map_err(map_error)?;
            let body = value
                .get("body_b64")
                .and_then(|value| value.as_str())
                .ok_or_else(|| KernelError::Storage("plugin read response has no body".into()))?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(body)
                .map_err(|error| KernelError::Storage(format!("plugin read body: {error}")))?;
            return Ok(Box::new(Cursor::new(bytes)));
        }
        let path = format!(
            "/v1/read?uri={}&start={start}&len={len}",
            http::encode(&uri.to_uri())
        );
        let guard: InflightGuard = self.gate.enter()?;
        let response = self.runtime.request(&path).map_err(map_error)?;
        if response.status != 200 {
            return Err(map_status(response.status));
        }
        Ok(Box::new(GuardedRead::new(response.take(len), guard)))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_gate_short_circuits_before_the_socket() {
        // Building a real runtime needs a child process; the gate is the part
        // that decides whether a call may start at all, so test it in place.
        let gate = Gate::new();
        gate.close();
        assert!(matches!(
            gate.enter().unwrap_err(),
            KernelError::StorageUnavailable(_)
        ));
    }
}
