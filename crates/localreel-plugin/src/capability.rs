//! Host-owned capability vocabulary for sidecar plugins.
//!
//! A plugin does not get to invent permissions. It requests a capability from
//! the list below with a typed scope; the host decides whether the name exists,
//! whether the scope is valid, and which tier it belongs to. The admin page and
//! the installer render the host's description, never the plugin's own words.
//!
//! Phase 1 makes the vocabulary and the grant records real. The broker that
//! executes these capabilities arrives in phase 2; until then a grant is a
//! disclosed intent, not a privilege that has been exercised.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::manifest::PluginError;

/// One capability a plugin asks for, as written in `manifest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRequest {
    pub name: String,
    #[serde(default)]
    pub scope: Value,
}

/// What the host publishes about a capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CapabilityDescriptor {
    pub name: &'static str,
    /// 0 = implicit, 1 = low, 2 = medium, 3 = high. 4 is reserved for the
    /// forbidden list, which is never grantable.
    pub tier: u8,
    pub title: &'static str,
    pub description: &'static str,
    /// `full` or `summary`; phase 1 records grants, phase 2 uses this to decide
    /// how much of each call to persist.
    pub audit: &'static str,
}

pub const CAPABILITIES: &[CapabilityDescriptor] = &[
    CapabilityDescriptor {
        name: "fs.read",
        tier: 2,
        title: "Read configured source roots",
        description: "Read media under the roots the host assigns to this plugin. The host resolves paths; the plugin cannot name arbitrary files.",
        audit: "summary",
    },
    CapabilityDescriptor {
        name: "net.fetch",
        tier: 2,
        title: "Fetch an allowlisted HTTP endpoint",
        description: "Ask the host to perform an HTTP(S) request against the declared hosts. The host applies redirect, size, timeout, rate and SSRF policy.",
        audit: "full",
    },
    CapabilityDescriptor {
        name: "secret.use",
        tier: 2,
        title: "Use a host-held credential",
        description: "Ask the host to inject a stored credential into a request bound to the declared hosts. The plugin never receives the secret value.",
        audit: "full",
    },
    CapabilityDescriptor {
        name: "schedule.request",
        tier: 1,
        title: "Request a periodic run",
        description: "Ask the host to schedule work at a bounded interval. The host owns the schedule, jitter, dedupe and missed-run policy.",
        audit: "summary",
    },
    CapabilityDescriptor {
        name: "catalog.write",
        tier: 2,
        title: "Write catalog drafts",
        description: "Submit normalized titles for host validation. Drafts need review; publishing is a separate, higher-risk scope.",
        audit: "full",
    },
    CapabilityDescriptor {
        name: "blob.read",
        tier: 1,
        title: "Read plugin-private blobs",
        description: "Read the plugin's own namespaced storage within its quota. Other plugins' namespaces are not reachable.",
        audit: "summary",
    },
    CapabilityDescriptor {
        name: "blob.write",
        tier: 2,
        title: "Write plugin-private blobs",
        description: "Write the plugin's own namespaced storage within its quota. The host enforces the quota and keeps the data isolated.",
        audit: "summary",
    },
];

/// Names the host refuses even if a future protocol learns to parse them.
pub const FORBIDDEN_CAPABILITIES: &[&str] = &[
    "net.listen",
    "net.socket",
    "process.spawn",
    "host.database",
    "host.config",
    "host.filesystem",
    "plugin.install",
    "ui.inject",
    "audit.disable",
];

pub fn descriptor(name: &str) -> Option<&'static CapabilityDescriptor> {
    CAPABILITIES.iter().find(|entry| entry.name == name)
}

pub fn is_forbidden(name: &str) -> bool {
    FORBIDDEN_CAPABILITIES.contains(&name)
}

pub fn forbidden() -> Vec<String> {
    FORBIDDEN_CAPABILITIES
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

/// A request that passed schema validation, with defaults materialized.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidatedCapability {
    pub tier: u8,
    pub scope: Value,
    pub scope_digest: String,
}

impl ValidatedCapability {
    pub fn digest(&self) -> &str {
        &self.scope_digest
    }
}

/// Validate a capability request and return the tier its scope actually needs.
pub fn validate_request(request: &CapabilityRequest) -> Result<ValidatedCapability, PluginError> {
    if is_forbidden(&request.name) {
        return Err(PluginError::Manifest(format!(
            "capability {:?} is forbidden",
            request.name
        )));
    }
    let descriptor = descriptor(&request.name)
        .ok_or_else(|| PluginError::Manifest(format!("unknown capability {:?}", request.name)))?;

    let (tier, scope) = match request.name.as_str() {
        "fs.read" => {
            let scope: FsReadScope = parse_scope(&request.scope)?;
            if !scope.read_only {
                return Err(PluginError::Manifest(
                    "fs.read.scope.read_only must be true; writing is a separate capability".into(),
                ));
            }
            if scope.roots.len() > 32 {
                return Err(PluginError::Manifest(
                    "fs.read.scope.roots may list at most 32 roots".into(),
                ));
            }
            for root in &scope.roots {
                require_text(root, "fs.read.scope.roots[]", 1024)?;
            }
            (descriptor.tier, to_value(scope)?)
        }
        "net.fetch" => {
            let scope: NetFetchScope = parse_scope(&request.scope)?;
            if scope.hosts.is_empty() || scope.hosts.len() > 32 {
                return Err(PluginError::Manifest(
                    "net.fetch.scope.hosts must contain 1..=32 hosts".into(),
                ));
            }
            for host in &scope.hosts {
                validate_host(host)?;
            }
            if scope.methods.is_empty() {
                return Err(PluginError::Manifest(
                    "net.fetch.scope.methods must not be empty".into(),
                ));
            }
            for method in &scope.methods {
                if !matches!(method.as_str(), "GET" | "HEAD") {
                    return Err(PluginError::Manifest(format!(
                        "net.fetch.scope.methods contains unsupported method {method:?}; the broker currently supports GET and HEAD"
                    )));
                }
            }
            if !(1..=64 * 1024 * 1024).contains(&scope.max_bytes) {
                return Err(PluginError::Manifest(
                    "net.fetch.scope.max_bytes must be 1..=64 MiB".into(),
                ));
            }
            if !(1..=120).contains(&scope.timeout_secs) {
                return Err(PluginError::Manifest(
                    "net.fetch.scope.timeout_secs must be 1..=120".into(),
                ));
            }
            if scope.redirects > 3 {
                return Err(PluginError::Manifest(
                    "net.fetch.scope.redirects must be 0..=3".into(),
                ));
            }
            if !(1..=600).contains(&scope.rate_per_min) {
                return Err(PluginError::Manifest(
                    "net.fetch.scope.rate_per_min must be 1..=600".into(),
                ));
            }
            (descriptor.tier, to_value(scope)?)
        }
        "secret.use" => {
            let scope: SecretUseScope = parse_scope(&request.scope)?;
            if scope.handles.is_empty() || scope.handles.len() > 16 {
                return Err(PluginError::Manifest(
                    "secret.use.scope.handles must contain 1..=16 handles".into(),
                ));
            }
            for handle in &scope.handles {
                require_text(handle, "secret.use.scope.handles[]", 64)?;
                if !handle
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
                {
                    return Err(PluginError::Manifest(format!(
                        "secret.use handle {handle:?} must be alphanumeric, '-' or '_'"
                    )));
                }
            }
            if scope.hosts.is_empty() || scope.hosts.len() > 32 {
                return Err(PluginError::Manifest(
                    "secret.use.scope.hosts must contain 1..=32 hosts".into(),
                ));
            }
            for host in &scope.hosts {
                validate_host(host)?;
            }
            if !matches!(scope.inject.as_str(), "header" | "query") {
                return Err(PluginError::Manifest(
                    "secret.use.scope.inject must be 'header' or 'query'".into(),
                ));
            }
            (descriptor.tier, to_value(scope)?)
        }
        "schedule.request" => {
            let scope: ScheduleScope = parse_scope(&request.scope)?;
            if scope.min_interval_secs < 60
                || scope.max_interval_secs < scope.min_interval_secs
                || scope.max_interval_secs > 30 * 24 * 60 * 60
            {
                return Err(PluginError::Manifest(
                    "schedule.request.scope intervals must satisfy 60 <= min <= max <= 30d".into(),
                ));
            }
            (descriptor.tier, to_value(scope)?)
        }
        "catalog.write" => {
            let scope: CatalogWriteScope = parse_scope(&request.scope)?;
            if !(1..=100_000).contains(&scope.max_items_per_run) {
                return Err(PluginError::Manifest(
                    "catalog.write.scope.max_items_per_run must be 1..=100000".into(),
                ));
            }
            let tier = if scope.mode == CatalogWriteMode::Publish {
                3
            } else {
                descriptor.tier
            };
            (tier, to_value(scope)?)
        }
        "blob.read" | "blob.write" => {
            let scope: BlobScope = parse_scope(&request.scope)?;
            if !(1024 * 1024..=10 * 1024 * 1024 * 1024).contains(&scope.quota_bytes) {
                return Err(PluginError::Manifest(
                    "blob scope.quota_bytes must be 1 MiB..=10 GiB".into(),
                ));
            }
            (descriptor.tier, to_value(scope)?)
        }
        _ => {
            // A registered capability with no scope schema accepts an empty
            // object. This branch should not be reachable for the list above.
            if !request.scope.is_null()
                && request
                    .scope
                    .as_object()
                    .is_some_and(|object| !object.is_empty())
            {
                return Err(PluginError::Manifest(format!(
                    "capability {:?} does not accept a scope",
                    request.name
                )));
            }
            (descriptor.tier, request.scope.clone())
        }
    };

    Ok(ValidatedCapability {
        tier,
        scope_digest: scope_digest(&scope),
        scope,
    })
}

/// Stable digest of a scope, used to prove that a call stayed inside its grant.
pub fn scope_digest(scope: &Value) -> String {
    let bytes = serde_json::to_vec(scope).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn parse_scope<T: DeserializeOwned>(value: &Value) -> Result<T, PluginError> {
    serde_json::from_value(value.clone())
        .map_err(|error| PluginError::Manifest(format!("invalid capability scope: {error}")))
}

fn to_value<T: Serialize>(value: T) -> Result<Value, PluginError> {
    serde_json::to_value(value)
        .map_err(|error| PluginError::Manifest(format!("invalid capability scope: {error}")))
}

fn require_text(value: &str, field: &str, max: usize) -> Result<(), PluginError> {
    if value.trim().is_empty() || value.len() > max {
        return Err(PluginError::Manifest(format!(
            "{field} must be 1..={max} characters"
        )));
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<(), PluginError> {
    require_text(host, "host", 255)?;
    if host.contains(['/', ':', '@', ' '])
        || host.starts_with('.')
        || host == "*"
        || !host
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '*'))
    {
        return Err(PluginError::Manifest(format!(
            "host {host:?} must be a bare hostname or *.domain (no scheme, port or path)"
        )));
    }
    Ok(())
}

/// A capability as the admin page presents it: declared intent plus the host's
/// own title/description and the tier its scope requires.
#[derive(Debug, Clone, Serialize)]
pub struct RequestedCapability {
    pub name: String,
    pub scope: Value,
    pub tier: u8,
    /// `manifest` for a declared capability, `legacy` for an old permission
    /// that the host still maps for compatibility.
    pub source: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub audit: &'static str,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FsReadScope {
    pub roots: Vec<String>,
    pub read_only: bool,
}

impl Default for FsReadScope {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            read_only: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct NetFetchScope {
    pub hosts: Vec<String>,
    pub methods: Vec<String>,
    pub max_bytes: u64,
    pub timeout_secs: u64,
    pub redirects: u8,
    pub rate_per_min: u32,
}

impl Default for NetFetchScope {
    fn default() -> Self {
        Self {
            hosts: Vec::new(),
            methods: vec!["GET".into()],
            max_bytes: 16 * 1024 * 1024,
            timeout_secs: 30,
            redirects: 0,
            rate_per_min: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct SecretUseScope {
    pub handles: Vec<String>,
    pub hosts: Vec<String>,
    pub inject: String,
}

impl Default for SecretUseScope {
    fn default() -> Self {
        Self {
            handles: Vec::new(),
            hosts: Vec::new(),
            inject: "header".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ScheduleScope {
    pub min_interval_secs: u64,
    pub max_interval_secs: u64,
}

impl Default for ScheduleScope {
    fn default() -> Self {
        Self {
            min_interval_secs: 3600,
            max_interval_secs: 24 * 60 * 60,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogWriteMode {
    #[default]
    Draft,
    Publish,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CatalogWriteScope {
    pub mode: CatalogWriteMode,
    pub max_items_per_run: u64,
}

impl Default for CatalogWriteScope {
    fn default() -> Self {
        Self {
            mode: CatalogWriteMode::Draft,
            max_items_per_run: 2000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BlobScope {
    pub quota_bytes: u64,
}

impl Default for BlobScope {
    fn default() -> Self {
        Self {
            quota_bytes: 100 * 1024 * 1024,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(name: &str, scope: Value) -> CapabilityRequest {
        CapabilityRequest {
            name: name.into(),
            scope,
        }
    }

    #[test]
    fn every_registered_capability_has_a_tier_and_description() {
        assert!(!CAPABILITIES.is_empty());
        for entry in CAPABILITIES {
            assert!(entry.tier <= 3, "{}", entry.name);
            assert!(!entry.title.is_empty(), "{}", entry.name);
            assert!(!entry.description.is_empty(), "{}", entry.name);
            assert!(matches!(entry.audit, "full" | "summary"), "{}", entry.name);
        }
    }

    #[test]
    fn rejects_unknown_and_forbidden_names() {
        assert!(validate_request(&request("nope", json!({}))).is_err());
        assert!(validate_request(&request("process.spawn", json!({}))).is_err());
        assert!(is_forbidden("process.spawn"));
    }

    #[test]
    fn net_fetch_requires_an_allowlist_and_bounded_scope() {
        let valid = validate_request(&request(
            "net.fetch",
            json!({"hosts": ["api.example.com"], "methods": ["GET"]}),
        ))
        .unwrap();
        assert_eq!(valid.tier, 2);
        assert_eq!(valid.scope["max_bytes"], 16 * 1024 * 1024);

        assert!(validate_request(&request("net.fetch", json!({"hosts": []}))).is_err());
        assert!(validate_request(&request("net.fetch", json!({"hosts": ["*"]}))).is_err());
        assert!(validate_request(&request(
            "net.fetch",
            json!({"hosts": ["api.example.com"], "methods": ["DELETE"]})
        ))
        .is_err());
        assert!(validate_request(&request(
            "net.fetch",
            json!({"hosts": ["api.example.com"], "unknown": true})
        ))
        .is_err());
    }

    #[test]
    fn publishing_is_a_higher_tier_than_drafting() {
        let draft = validate_request(&request(
            "catalog.write",
            json!({"mode": "draft", "max_items_per_run": 10}),
        ))
        .unwrap();
        let publish = validate_request(&request(
            "catalog.write",
            json!({"mode": "publish", "max_items_per_run": 10}),
        ))
        .unwrap();
        assert_eq!(draft.tier, 2);
        assert_eq!(publish.tier, 3);
    }

    #[test]
    fn schedule_bounds_are_enforced() {
        assert!(validate_request(&request(
            "schedule.request",
            json!({"min_interval_secs": 10, "max_interval_secs": 20})
        ))
        .is_err());
        assert!(validate_request(&request(
            "schedule.request",
            json!({"min_interval_secs": 3600, "max_interval_secs": 60})
        ))
        .is_err());
    }

    #[test]
    fn scope_digest_is_stable_and_scope_specific() {
        let one =
            validate_request(&request("net.fetch", json!({"hosts": ["api.example.com"]}))).unwrap();
        let two =
            validate_request(&request("net.fetch", json!({"hosts": ["api.example.com"]}))).unwrap();
        let other =
            validate_request(&request("net.fetch", json!({"hosts": ["cdn.example.com"]}))).unwrap();
        assert_eq!(one.scope_digest, two.scope_digest);
        assert_ne!(one.scope_digest, other.scope_digest);
        assert_eq!(one.scope_digest.len(), 64);
    }

    #[test]
    fn fs_read_is_read_only_and_accepts_empty_roots() {
        let valid = validate_request(&request("fs.read", json!({}))).unwrap();
        assert_eq!(valid.scope["read_only"], true);
        assert!(validate_request(&request("fs.read", json!({"read_only": false}))).is_err());
    }
}
