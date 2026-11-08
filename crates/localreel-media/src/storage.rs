//! Maps a source kind to its storage provider.
//!
//! `local` is built in. Every other kind is registered by the server after a
//! sidecar plugin has started, so the media layer can resolve a source without
//! knowing whether it is a directory, a WebDAV share or something else.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::storage::{LocalStorage, MediaUri, Storage};

use crate::model::Source;

#[derive(Default)]
pub struct StorageRegistry {
    plugins: RwLock<HashMap<String, Arc<dyn Storage>>>,
    /// Kinds the configuration expects a provider for, whether or not one is
    /// registered right now. Lets a stopped plugin answer `503` instead of the
    /// generic "no provider" error.
    declared: RwLock<HashSet<String>>,
}

impl StorageRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a provider for a source kind (usually a plugin id).
    pub fn register(&self, kind: impl Into<String>, provider: Arc<dyn Storage>) {
        self.plugins
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(kind.into(), provider);
    }

    pub fn has(&self, kind: &str) -> bool {
        kind == "local"
            || self
                .plugins
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains_key(kind)
    }

    /// Remove the provider registered for `kind`.
    ///
    /// Returns whether anything was removed. Registering a provider and later
    /// taking it away is what lets a plugin stop: sources keep their rows, but
    /// resolving them fails with [`KernelError::StorageUnavailable`] until a new
    /// generation registers again.
    pub fn unregister(&self, kind: &str) -> bool {
        self.plugins
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(kind)
            .is_some()
    }

    /// Record that the configuration expects a provider for `kind`.
    ///
    /// Called for every configured plugin, running or not, so a source whose
    /// plugin is disabled or failed says "unavailable" instead of "unknown".
    pub fn declare(&self, kind: impl Into<String>) {
        self.declared
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(kind.into());
    }

    pub fn is_declared(&self, kind: &str) -> bool {
        self.declared
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(kind)
    }

    /// The kinds currently served by a sidecar plugin, sorted.
    pub fn kinds(&self) -> Vec<String> {
        let mut kinds: Vec<String> = self
            .plugins
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .keys()
            .cloned()
            .collect();
        kinds.sort();
        kinds
    }

    /// Resolve the provider for a source row.
    pub fn for_source(&self, source: &Source) -> Result<Arc<dyn Storage>> {
        if source.kind == "local" {
            let root = match MediaUri::parse(&source.uri)? {
                MediaUri::Local(path) => path,
                _ => {
                    return Err(KernelError::Storage(format!(
                        "source {} is marked local but its uri is not a file path",
                        source.id
                    )))
                }
            };
            return Ok(Arc::new(LocalStorage::new([root])?));
        }
        let provider = self
            .plugins
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&source.kind)
            .cloned();
        if let Some(provider) = provider {
            return Ok(provider);
        }
        // A kind the configuration knows about but nothing serves right now is
        // a temporary condition (disabled, starting, failed), not a bad source.
        if self.is_declared(&source.kind) {
            return Err(KernelError::StorageUnavailable(format!(
                "the '{}' storage plugin is not running",
                source.kind
            )));
        }
        Err(KernelError::Storage(format!(
            "no storage provider registered for source kind '{}'",
            source.kind
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::storage::{FileStat, StorageEntry};
    use std::io::Read;

    struct Mock;

    impl Storage for Mock {
        fn stat(&self, _uri: &MediaUri) -> Result<FileStat> {
            Ok(FileStat {
                size: 7,
                modified: Some(1),
                is_dir: false,
            })
        }
        fn list(&self, _uri: &MediaUri) -> Result<Vec<StorageEntry>> {
            Ok(Vec::new())
        }
        fn open_range(
            &self,
            _uri: &MediaUri,
            _start: u64,
            _len: u64,
        ) -> Result<Box<dyn Read + Send>> {
            Ok(Box::new(std::io::empty()))
        }
        fn walk(
            &self,
            _root: &MediaUri,
        ) -> Result<Box<dyn Iterator<Item = Result<StorageEntry>> + Send>> {
            Ok(Box::new(std::iter::empty()))
        }
    }

    fn source(kind: &str, uri: &str) -> Source {
        Source {
            id: 1,
            uri: uri.to_string(),
            kind: kind.to_string(),
            enabled: true,
            created_at: 0,
            updated_at: 0,
            archived_at: None,
        }
    }

    #[test]
    fn resolves_local_sources_without_registration() {
        let dir = tempfile::tempdir().unwrap();
        let registry = StorageRegistry::new();
        let provider = registry
            .for_source(&source("local", dir.path().to_str().unwrap()))
            .unwrap();
        assert!(provider
            .stat(&MediaUri::Local(dir.path().join("missing.mp4")))
            .is_err());
    }

    #[test]
    fn resolves_registered_plugin_kinds_and_rejects_unknown_ones() {
        let registry = StorageRegistry::new();
        registry.register("demo", Arc::new(Mock));
        assert!(registry.has("demo"));
        let provider = registry
            .for_source(&source("demo", "demo://library"))
            .unwrap();
        assert_eq!(
            provider
                .stat(&MediaUri::Http("http://x".into()))
                .unwrap()
                .size,
            7
        );
        assert!(registry
            .for_source(&source("nope", "nope://library"))
            .is_err());
    }

    #[test]
    fn unregistering_a_provider_takes_it_away() {
        let registry = StorageRegistry::new();
        registry.register("demo", Arc::new(Mock));
        assert_eq!(registry.kinds(), vec!["demo".to_string()]);
        assert!(registry.unregister("demo"));
        assert!(!registry.unregister("demo"));
        assert!(registry.kinds().is_empty());
        assert!(!registry.has("demo"));
    }

    #[test]
    fn a_declared_but_stopped_plugin_reports_unavailable() {
        let registry = StorageRegistry::new();
        registry.declare("demo");
        let error = registry
            .for_source(&source("demo", "demo://library"))
            .err()
            .expect("a declared but stopped plugin must not resolve");
        assert!(
            matches!(error, KernelError::StorageUnavailable(_)),
            "a configured plugin that is not running must be unavailable, not unknown: {error:?}"
        );
        // A kind nobody ever declared keeps the old "unknown" answer.
        let error = registry
            .for_source(&source("nope", "nope://library"))
            .err()
            .expect("an unknown kind must not resolve");
        assert!(matches!(error, KernelError::Storage(_)));
    }
}
