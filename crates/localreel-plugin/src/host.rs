//! The running plugin set: tokens, permissions, and the network proxy.
//!
//! A sidecar can always open its own sockets — the host cannot stop that without
//! an OS sandbox. What it *can* do is stop being the accomplice: the host proxy
//! is the only sanctioned way out, it requires the `network` permission, and it
//! records every request. A plugin that declares nothing can still reach the
//! internet, but nothing it does through the host will be unattributed.

use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use serde::Serialize;

use crate::audit::AuditLog;
use crate::manifest::{Manifest, PERMISSION_NETWORK};
use crate::runtime::PluginRuntime;

/// Upper bound on a proxied response, so one plugin cannot fill the disk.
pub const MAX_PROXY_BYTES: u64 = 32 * 1024 * 1024;
const PROXY_TIMEOUT: Duration = Duration::from_secs(30);

/// What the admin API lists for one plugin.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginSummary {
    pub id: String,
    pub name: String,
    pub version: String,
    /// Effective permissions, with the storage baseline included.
    pub permissions: Vec<String>,
    /// Whether the host will proxy network requests for it.
    pub network: bool,
    pub running: bool,
}

/// A response fetched on a plugin's behalf.
#[derive(Debug, Clone)]
pub struct ProxyResponse {
    pub status: u16,
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

struct Entry {
    manifest: Manifest,
    runtime: Arc<PluginRuntime>,
}

/// Owns the started plugins for the lifetime of the process.
#[derive(Default)]
pub struct PluginHost {
    entries: Vec<Entry>,
    audit: Option<AuditLog>,
}

impl PluginHost {
    pub fn new(db: Database) -> Self {
        Self {
            entries: Vec::new(),
            audit: Some(AuditLog::new(db)),
        }
    }

    pub fn audit(&self) -> Option<&AuditLog> {
        self.audit.as_ref()
    }

    /// Start tracking a plugin the server has launched.
    pub fn register(&mut self, runtime: Arc<PluginRuntime>) {
        let manifest = runtime.manifest().clone();
        if let Some(audit) = &self.audit {
            if let Err(error) = audit.record(
                &manifest.id,
                None,
                "start",
                &format!("version {}", manifest.version),
            ) {
                tracing::warn!(plugin = %manifest.id, "failed to record plugin start: {error}");
            }
        }
        self.entries.push(Entry { manifest, runtime });
    }

    pub fn summaries(&self) -> Vec<PluginSummary> {
        self.entries
            .iter()
            .map(|entry| PluginSummary {
                id: entry.manifest.id.clone(),
                name: entry.manifest.name.clone(),
                version: entry.manifest.version.clone(),
                permissions: entry.manifest.effective_permissions(),
                network: entry.manifest.has_permission(PERMISSION_NETWORK),
                running: true,
            })
            .collect()
    }

    /// The runtime that presents `token`, if any.
    pub fn authenticate(&self, token: &str) -> Option<&PluginRuntime> {
        self.entries
            .iter()
            .find(|entry| entry.runtime.token() == token)
            .map(|entry| entry.runtime.as_ref())
    }

    /// Fetch `url` on behalf of the plugin that owns `token`.
    ///
    /// Refused — and recorded — when the plugin never declared `network`.
    pub fn proxy(&self, token: &str, url: &str) -> Result<ProxyResponse> {
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.runtime.token() == token)
            .ok_or_else(|| KernelError::PermissionDenied("unknown plugin token".to_string()))?;
        let id = entry.manifest.id.clone();

        if !entry.manifest.has_permission(PERMISSION_NETWORK) {
            if let Some(audit) = &self.audit {
                let _ = audit.record(&id, Some(PERMISSION_NETWORK), "network.denied", url);
            }
            return Err(KernelError::PermissionDenied(format!(
                "plugin '{id}' did not declare the network permission"
            )));
        }

        if let Some(audit) = &self.audit {
            let _ = audit.record(&id, Some(PERMISSION_NETWORK), "network.allowed", url);
        }
        fetch(url)
    }
}

/// Fetch one URL with a timeout and a size cap.
fn fetch(url: &str) -> Result<ProxyResponse> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(KernelError::InvalidInput(
            "only http(s) URLs can be proxied".to_string(),
        ));
    }
    let agent = ureq::AgentBuilder::new().timeout(PROXY_TIMEOUT).build();
    let response = match agent.get(url).call() {
        Ok(response) => response,
        // A 4xx/5xx is still a response: the plugin may want to see it.
        Err(ureq::Error::Status(_, response)) => response,
        Err(error) => {
            return Err(KernelError::Storage(format!(
                "the proxy could not reach {url}: {error}"
            )))
        }
    };
    let status = response.status();
    let content_type = response.header("content-type").map(str::to_string);
    let mut body = Vec::new();
    response
        .into_reader()
        .take(MAX_PROXY_BYTES)
        .read_to_end(&mut body)?;
    Ok(ProxyResponse {
        status,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::manifest::API_VERSION;

    fn manifest(permissions: Vec<&str>) -> Manifest {
        Manifest {
            id: "demo".into(),
            name: "Demo".into(),
            version: "0.1.0".into(),
            api_version: API_VERSION.into(),
            kind: crate::PluginKind::Storage,
            entry: crate::manifest::Entry {
                program: "plugin".into(),
                args: Vec::new(),
                runtime: None,
            },
            permissions: permissions.into_iter().map(str::to_string).collect(),
            description: None,
        }
    }

    #[test]
    fn a_summary_reports_the_effective_permissions() {
        let host = PluginHost::default();
        // A storage plugin always reports the read baseline.
        let summary = PluginSummary {
            id: manifest(vec!["network"]).id,
            name: "Demo".into(),
            version: "0.1.0".into(),
            permissions: manifest(vec!["network"]).effective_permissions(),
            network: true,
            running: true,
        };
        assert_eq!(summary.permissions, vec!["storage:read", "network"]);
        assert!(host.summaries().is_empty());
    }

    #[test]
    fn an_unknown_token_is_refused() {
        let host = PluginHost::default();
        let error = host.proxy("nope", "https://example.com").unwrap_err();
        assert!(matches!(error, KernelError::PermissionDenied(_)));
        assert!(host.authenticate("nope").is_none());
    }
}
