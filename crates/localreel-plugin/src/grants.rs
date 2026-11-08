//! Persisted capability grants.
//!
//! A manifest *requests* a capability; an administrator *grants* it for one
//! plugin version. The grant is the durable object the broker will check in
//! phase 2, and it is what makes "revoke without restarting" possible.
//!
//! The store does not execute anything yet. It exists so that the host can
//! answer "what did we allow, for which version, for how long, and who allowed
//! it" before a broker starts enforcing the answer.

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension, Row};
use serde::Serialize;
use serde_json::Value;

use crate::capability::{self, CapabilityRequest};

/// One active or historical grant.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CapabilityGrant {
    pub id: i64,
    pub plugin_id: String,
    pub plugin_version: String,
    pub capability: String,
    pub scope: Value,
    pub scope_digest: String,
    pub tier: u8,
    pub granted_by: Option<i64>,
    pub granted_at: i64,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
}

impl CapabilityGrant {
    pub fn is_active_at(&self, now: i64) -> bool {
        self.revoked_at.is_none() && self.expires_at.is_none_or(|expires| expires > now)
    }
}

#[derive(Clone)]
pub struct GrantStore {
    db: Database,
}

impl GrantStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Grant (or re-grant) one capability for one plugin version.
    ///
    /// The scope is validated against the host's capability schema and stored
    /// in normalized form, so a later broker can compare a request against the
    /// same shape the administrator approved.
    #[allow(clippy::too_many_arguments)]
    pub fn grant(
        &self,
        plugin_id: &str,
        plugin_version: &str,
        capability: &str,
        scope: Value,
        granted_by: Option<i64>,
        expires_at: Option<i64>,
    ) -> Result<CapabilityGrant> {
        if plugin_id.trim().is_empty() || plugin_version.trim().is_empty() {
            return Err(KernelError::InvalidInput(
                "plugin id and version are required".into(),
            ));
        }
        let validated = capability::validate_request(&CapabilityRequest {
            name: capability.to_string(),
            scope,
        })
        .map_err(|error| KernelError::InvalidInput(error.to_string()))?;
        let scope_json = serde_json::to_string(&validated.scope)
            .map_err(|error| KernelError::InvalidInput(error.to_string()))?;
        let at = now_secs();

        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO plugin_grants (
                     plugin_id, plugin_version, capability, scope_json, scope_digest,
                     tier, granted_by, granted_at, expires_at, revoked_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)
                 ON CONFLICT(plugin_id, plugin_version, capability) DO UPDATE SET
                     scope_json = excluded.scope_json,
                     scope_digest = excluded.scope_digest,
                     tier = excluded.tier,
                     granted_by = excluded.granted_by,
                     granted_at = excluded.granted_at,
                     expires_at = excluded.expires_at,
                     revoked_at = NULL",
                params![
                    plugin_id,
                    plugin_version,
                    capability,
                    scope_json,
                    validated.scope_digest,
                    validated.tier,
                    granted_by,
                    at,
                    expires_at
                ],
            )?;
            connection
                .query_row(
                    "SELECT id, plugin_id, plugin_version, capability, scope_json,
                            scope_digest, tier, granted_by, granted_at, expires_at, revoked_at
                     FROM plugin_grants
                     WHERE plugin_id = ?1 AND plugin_version = ?2 AND capability = ?3",
                    params![plugin_id, plugin_version, capability],
                    row_to_grant,
                )
                .map_err(KernelError::from)
        })
    }

    /// All grants for one plugin across versions, or all plugins.
    pub fn list(&self, plugin_id: Option<&str>) -> Result<Vec<CapabilityGrant>> {
        self.db.with_read(|connection| {
            let mut grants = Vec::new();
            match plugin_id {
                Some(id) => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, plugin_version, capability, scope_json,
                                scope_digest, tier, granted_by, granted_at, expires_at, revoked_at
                         FROM plugin_grants WHERE plugin_id = ?1
                         ORDER BY capability, plugin_version",
                    )?;
                    for row in statement.query_map([id], row_to_grant)? {
                        grants.push(row?);
                    }
                }
                None => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, plugin_version, capability, scope_json,
                                scope_digest, tier, granted_by, granted_at, expires_at, revoked_at
                         FROM plugin_grants ORDER BY plugin_id, capability, plugin_version",
                    )?;
                    for row in statement.query_map([], row_to_grant)? {
                        grants.push(row?);
                    }
                }
            }
            Ok(grants)
        })
    }

    /// Active (not revoked, not expired) grants for one version.
    pub fn active(&self, plugin_id: &str, plugin_version: &str) -> Result<Vec<CapabilityGrant>> {
        let now = now_secs();
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, plugin_id, plugin_version, capability, scope_json,
                        scope_digest, tier, granted_by, granted_at, expires_at, revoked_at
                 FROM plugin_grants
                 WHERE plugin_id = ?1 AND plugin_version = ?2
                   AND revoked_at IS NULL
                   AND (expires_at IS NULL OR expires_at > ?3)
                 ORDER BY capability",
            )?;
            let mut grants = Vec::new();
            for row in statement.query_map(params![plugin_id, plugin_version, now], row_to_grant)? {
                grants.push(row?);
            }
            Ok(grants)
        })
    }

    pub fn is_active(
        &self,
        plugin_id: &str,
        plugin_version: &str,
        capability: &str,
    ) -> Result<bool> {
        let now = now_secs();
        self.db.with_read(|connection| {
            let found: Option<i64> = connection
                .query_row(
                    "SELECT id FROM plugin_grants
                     WHERE plugin_id = ?1 AND plugin_version = ?2 AND capability = ?3
                       AND revoked_at IS NULL
                       AND (expires_at IS NULL OR expires_at > ?4)",
                    params![plugin_id, plugin_version, capability, now],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(found.is_some())
        })
    }

    /// Revoke one grant. Returns whether a row changed.
    pub fn revoke(&self, plugin_id: &str, plugin_version: &str, capability: &str) -> Result<bool> {
        let at = now_secs();
        self.db.with_write(|connection| {
            let changed = connection.execute(
                "UPDATE plugin_grants SET revoked_at = ?1
                 WHERE plugin_id = ?2 AND plugin_version = ?3 AND capability = ?4
                   AND revoked_at IS NULL",
                params![at, plugin_id, plugin_version, capability],
            )?;
            Ok(changed > 0)
        })
    }
}

fn row_to_grant(row: &Row<'_>) -> rusqlite::Result<CapabilityGrant> {
    let scope_json: String = row.get(4)?;
    let scope = serde_json::from_str(&scope_json).unwrap_or(Value::Null);
    Ok(CapabilityGrant {
        id: row.get(0)?,
        plugin_id: row.get(1)?,
        plugin_version: row.get(2)?,
        capability: row.get(3)?,
        scope,
        scope_digest: row.get(5)?,
        tier: row.get::<_, i64>(6)? as u8,
        granted_by: row.get(7)?,
        granted_at: row.get(8)?,
        expires_at: row.get(9)?,
        revoked_at: row.get(10)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn setup() -> (tempfile::TempDir, GrantStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("grants.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        (dir, GrantStore::new(db))
    }

    #[test]
    fn grants_are_version_scoped_and_scope_is_normalized() {
        let (_dir, store) = setup();
        let grant = store
            .grant(
                "webdav",
                "0.1.0",
                "net.fetch",
                json!({"hosts": ["nas.local"]}),
                Some(1),
                None,
            )
            .unwrap();
        assert_eq!(grant.tier, 2);
        assert_eq!(grant.scope["methods"][0], "GET");
        assert_eq!(grant.scope["max_bytes"], 16 * 1024 * 1024);
        assert_eq!(grant.scope_digest.len(), 64);
        assert!(grant.is_active_at(now_secs()));

        assert_eq!(store.active("webdav", "0.1.0").unwrap().len(), 1);
        assert!(store.active("webdav", "0.1.1").unwrap().is_empty());
        assert!(store.is_active("webdav", "0.1.0", "net.fetch").unwrap());
        assert!(!store.is_active("webdav", "0.1.0", "secret.use").unwrap());
    }

    #[test]
    fn granting_again_updates_scope_and_unrevokes() {
        let (_dir, store) = setup();
        store
            .grant(
                "webdav",
                "0.1.0",
                "net.fetch",
                json!({"hosts": ["old.local"]}),
                None,
                None,
            )
            .unwrap();
        assert!(store.revoke("webdav", "0.1.0", "net.fetch").unwrap());
        assert!(!store.is_active("webdav", "0.1.0", "net.fetch").unwrap());

        let grant = store
            .grant(
                "webdav",
                "0.1.0",
                "net.fetch",
                json!({"hosts": ["new.local"]}),
                None,
                None,
            )
            .unwrap();
        assert_eq!(grant.scope["hosts"][0], "new.local");
        assert!(store.is_active("webdav", "0.1.0", "net.fetch").unwrap());
        assert_eq!(store.list(Some("webdav")).unwrap().len(), 1);
    }

    #[test]
    fn expired_grants_are_not_active() {
        let (_dir, store) = setup();
        store
            .grant(
                "webdav",
                "0.1.0",
                "net.fetch",
                json!({"hosts": ["nas.local"]}),
                None,
                Some(now_secs() + 60),
            )
            .unwrap();
        assert!(store.is_active("webdav", "0.1.0", "net.fetch").unwrap());

        store
            .db
            .with_write(|connection| {
                connection.execute(
                    "UPDATE plugin_grants SET expires_at = ?1 WHERE plugin_id = 'webdav'",
                    [now_secs() - 1],
                )?;
                Ok(())
            })
            .unwrap();
        assert!(!store.is_active("webdav", "0.1.0", "net.fetch").unwrap());
        assert!(store.active("webdav", "0.1.0").unwrap().is_empty());
    }

    #[test]
    fn unknown_or_forbidden_capabilities_are_rejected() {
        let (_dir, store) = setup();
        assert!(store
            .grant("webdav", "0.1.0", "nope", json!({}), None, None)
            .is_err());
        assert!(store
            .grant("webdav", "0.1.0", "process.spawn", json!({}), None, None)
            .is_err());
        assert!(store
            .grant(
                "webdav",
                "0.1.0",
                "net.fetch",
                json!({"hosts": []}),
                None,
                None
            )
            .is_err());
    }
}
