//! Manifest and package rules for sidecar plugins.
//!
//! A plugin is a zip package whose root contains `manifest.json`. The manifest
//! is read and validated **before** any package content is extracted or any
//! process is started: the id, API version, entry point and permissions are
//! known up front, which is the property the vtx plugin system lacked.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Protocol version a host understands. A plugin with any other value is
/// rejected, never best-effort interpreted.
pub const API_VERSION: &str = "localreel.plugin/v1";

pub const MAX_MANIFEST_BYTES: usize = 64 * 1024;
pub const MAX_PACKAGE_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_PACKAGE_ENTRIES: usize = 4096;

/// Permission a storage plugin needs to reach a remote server. Without it the
/// host proxy refuses to fetch anything on the plugin's behalf.
pub const PERMISSION_NETWORK: &str = "network";

/// Permission to read the media under the plugin's declared source roots.
///
/// Every storage plugin needs it; declaring it is what makes the installation
/// prompt meaningful — the host can say what a plugin will be able to do before
/// it runs.
pub const PERMISSION_STORAGE_READ: &str = "storage:read";

pub const ALLOWED_PERMISSIONS: &[&str] = &[PERMISSION_STORAGE_READ, PERMISSION_NETWORK];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub api_version: String,
    pub kind: PluginKind,
    pub entry: Entry,
    #[serde(default)]
    pub permissions: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginKind {
    /// Adds a storage provider (`Storage` implementation).
    Storage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Program inside the package, relative to its root.
    pub program: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Optional interpreter used to run the program (for example `node` or
    /// `python3`). When absent the program itself is executed.
    #[serde(default)]
    pub runtime: Option<String>,
}

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("invalid manifest: {0}")]
    Manifest(String),
    #[error("unsupported plugin api version {0} (expected {API_VERSION})")]
    ApiVersion(String),
    #[error("plugin manifest is too large (max {MAX_MANIFEST_BYTES} bytes)")]
    ManifestTooLarge,
    #[error("plugin package is too large (max {MAX_PACKAGE_BYTES} bytes)")]
    PackageTooLarge,
    #[error("plugin package has too many entries")]
    TooManyEntries,
    #[error("plugin package expands beyond the size limit")]
    ExtractedTooLarge,
    #[error("unsafe package entry: {0}")]
    UnsafeEntry(String),
    #[error("plugin package is missing {0}")]
    Missing(String),
    #[error("plugin io error: {0}")]
    Io(String),
    #[error("plugin package error: {0}")]
    Archive(String),
    #[error("plugin did not become ready: {0}")]
    NotReady(String),
    #[error("plugin request failed: {0}")]
    Request(String),
}

impl Manifest {
    pub fn parse(bytes: &[u8]) -> Result<Self, PluginError> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PluginError::ManifestTooLarge);
        }
        let manifest: Manifest = serde_json::from_slice(bytes)
            .map_err(|error| PluginError::Manifest(error.to_string()))?;
        manifest.validate()?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), PluginError> {
        if self.api_version != API_VERSION {
            return Err(PluginError::ApiVersion(self.api_version.clone()));
        }
        validate_id(&self.id)?;
        if self.name.trim().is_empty() || self.name.chars().count() > 80 {
            return Err(PluginError::Manifest(
                "name must be 1..=80 characters".into(),
            ));
        }
        if self.version.trim().is_empty()
            || self.version.chars().count() > 32
            || self.version.chars().any(|ch| ch.is_control())
        {
            return Err(PluginError::Manifest("version is invalid".into()));
        }
        if self
            .description
            .as_ref()
            .is_some_and(|text| text.len() > 2000)
        {
            return Err(PluginError::Manifest(
                "description is too long (max 2000 bytes)".into(),
            ));
        }
        validate_relative_program(&self.entry.program)?;
        if let Some(runtime) = &self.entry.runtime {
            let valid = !runtime.is_empty()
                && runtime.len() <= 64
                && runtime
                    .chars()
                    .next()
                    .is_some_and(|ch| ch.is_ascii_alphanumeric())
                && runtime
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'));
            if !valid {
                return Err(PluginError::Manifest(
                    "entry runtime must be a bare command name (no path separators)".into(),
                ));
            }
        }
        if self.entry.args.len() > 32 || self.entry.args.iter().any(|arg| arg.contains('\0')) {
            return Err(PluginError::Manifest(
                "entry args are invalid (max 32, no NUL)".into(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for permission in &self.permissions {
            if !ALLOWED_PERMISSIONS.contains(&permission.as_str()) {
                return Err(PluginError::Manifest(format!(
                    "unknown permission {permission:?}"
                )));
            }
            if !seen.insert(permission) {
                return Err(PluginError::Manifest(format!(
                    "duplicate permission {permission:?}"
                )));
            }
        }
        // A storage plugin that cannot read its roots cannot do its job, but
        // the permission is implied by the kind: a package written before the
        // permission existed keeps loading. `network` is never implied.
        Ok(())
    }

    /// The permissions that actually apply to this plugin.
    ///
    /// A storage plugin always has read access to its declared roots — that is
    /// what the permission means — so the host reports it even when an older
    /// manifest only listed `network`. The installer prompt and the admin page
    /// show this list, not the raw one.
    pub fn effective_permissions(&self) -> Vec<String> {
        let mut permissions: Vec<String> = Vec::new();
        if self.kind == PluginKind::Storage {
            permissions.push(PERMISSION_STORAGE_READ.to_string());
        }
        for permission in &self.permissions {
            if !permissions.contains(permission) {
                permissions.push(permission.clone());
            }
        }
        permissions
    }

    pub fn has_permission(&self, permission: &str) -> bool {
        self.permissions.iter().any(|value| value == permission)
    }
}

fn validate_id(id: &str) -> Result<(), PluginError> {
    let valid = !id.is_empty()
        && id.len() <= 32
        && id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
        && id
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_alphanumeric());
    if !valid {
        return Err(PluginError::Manifest(
            "id must be 1..=32 lowercase letters, digits or '-'".into(),
        ));
    }
    Ok(())
}

pub fn validate_relative_program(program: &str) -> Result<(), PluginError> {
    if program.trim().is_empty() || program.len() > 255 || program.contains('\0') {
        return Err(PluginError::Manifest("entry program is invalid".into()));
    }
    // ZIP entries use '/'. A package must mean the same thing on every host, so
    // a backslash separator or a Windows drive prefix is rejected even on Unix,
    // where `C:/x` would otherwise be a relative file name.
    if program.contains('\\') {
        return Err(PluginError::Manifest(
            "entry program must use '/' separators".into(),
        ));
    }
    let bytes = program.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(PluginError::Manifest(
            "entry program must not contain a drive prefix".into(),
        ));
    }
    if program.starts_with('/') {
        return Err(PluginError::Manifest(
            "entry program must be relative to the package".into(),
        ));
    }
    let path = Path::new(program);
    let mut components = path.components();
    let mut count = 0usize;
    for component in components.by_ref() {
        match component {
            Component::Normal(_) => count += 1,
            _ => {
                return Err(PluginError::Manifest(
                    "entry program must not contain '.', '..' or a drive prefix".into(),
                ))
            }
        }
    }
    if count == 0 {
        return Err(PluginError::Manifest("entry program is empty".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            id: "webdav".into(),
            name: "WebDAV storage".into(),
            version: "0.1.0".into(),
            api_version: API_VERSION.into(),
            kind: PluginKind::Storage,
            entry: Entry {
                program: "bin/webdav-plugin".into(),
                args: vec!["--serve".into()],
                runtime: None,
            },
            permissions: vec![PERMISSION_NETWORK.into()],
            description: None,
        }
    }

    #[test]
    fn accepts_a_valid_manifest() {
        let parsed = Manifest::parse(&serde_json::to_vec(&manifest()).unwrap()).unwrap();
        assert_eq!(parsed.id, "webdav");
        assert!(parsed.has_permission(PERMISSION_NETWORK));
        // The kind implies read access; the raw list stays as declared so the
        // manifest round-trips.
        assert!(!parsed.has_permission(PERMISSION_STORAGE_READ));
        assert_eq!(
            parsed.effective_permissions(),
            vec![
                PERMISSION_STORAGE_READ.to_string(),
                PERMISSION_NETWORK.to_string()
            ]
        );
    }

    #[test]
    fn effective_permissions_do_not_duplicate_a_declared_one() {
        let mut value = manifest();
        value.permissions = vec![PERMISSION_STORAGE_READ.into(), PERMISSION_NETWORK.into()];
        let parsed = Manifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            parsed.effective_permissions(),
            vec![
                PERMISSION_STORAGE_READ.to_string(),
                PERMISSION_NETWORK.to_string()
            ]
        );
        // Declaring only the baseline is fine too.
        value.permissions = vec![PERMISSION_STORAGE_READ.into()];
        let parsed = Manifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(
            parsed.effective_permissions(),
            vec![PERMISSION_STORAGE_READ.to_string()]
        );
        assert!(!parsed.has_permission(PERMISSION_NETWORK));
    }

    #[test]
    fn rejects_duplicate_permissions() {
        let mut value = manifest();
        value.permissions = vec![PERMISSION_NETWORK.into(), PERMISSION_NETWORK.into()];
        assert!(matches!(
            Manifest::parse(&serde_json::to_vec(&value).unwrap()),
            Err(PluginError::Manifest(_))
        ));
    }

    #[test]
    fn rejects_unknown_api_versions_and_permissions() {
        let mut newer = manifest();
        newer.api_version = "localreel.plugin/v2".into();
        assert!(matches!(
            Manifest::parse(&serde_json::to_vec(&newer).unwrap()),
            Err(PluginError::ApiVersion(_))
        ));

        let mut scary = manifest();
        scary.permissions.push("filesystem:all".into());
        assert!(matches!(
            Manifest::parse(&serde_json::to_vec(&scary).unwrap()),
            Err(PluginError::Manifest(_))
        ));
    }

    #[test]
    fn rejects_unsafe_entry_programs() {
        for program in ["../escape", "/abs", "C:/windows/x", "./x", ""] {
            let mut bad = manifest();
            bad.entry.program = program.into();
            assert!(
                Manifest::parse(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "{program} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_bad_ids() {
        for id in ["", "-lead", "Upper", "with space", "x".repeat(33).as_str()] {
            let mut bad = manifest();
            bad.id = id.into();
            assert!(
                Manifest::parse(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "{id} should be rejected"
            );
        }
    }

    #[test]
    fn accepts_an_interpreter_runtime() {
        let mut value = manifest();
        value.entry.program = "plugin.js".into();
        value.entry.args.clear();
        value.entry.runtime = Some("node".into());
        let parsed = Manifest::parse(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(parsed.entry.runtime.as_deref(), Some("node"));
    }

    #[test]
    fn rejects_runtime_paths() {
        for runtime in ["/usr/bin/node", "node\\plugin", "..", "node script"] {
            let mut value = manifest();
            value.entry.runtime = Some(runtime.into());
            assert!(value.validate().is_err(), "{runtime} should be rejected");
        }
    }
}
