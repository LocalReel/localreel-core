//! Host-held credentials for `secret.use`.
//!
//! The plugin only ever sees a handle. The value is fetched by the host while
//! applying the `secret.use` grant and injected into the broker request; it is
//! never returned by any API and never written to the audit table.
//!
//! Storage note: values live in the local SQLite database, protected by the
//! operating-system account boundary. Encryption at rest (and a separate host
//! key) is a follow-up, not a claim this module makes.

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;

const MAX_VALUE_BYTES: usize = 8192;
const MAX_HANDLE_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SecretHandle {
    pub handle: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct SecretStore {
    db: Database,
}

impl SecretStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Store or replace one credential for one plugin.
    pub fn put(&self, plugin_id: &str, handle: &str, value: &str) -> Result<SecretHandle> {
        validate_plugin_id(plugin_id)?;
        validate_handle(handle)?;
        if value.trim().is_empty() {
            return Err(KernelError::InvalidInput(
                "secret value must not be empty".into(),
            ));
        }
        if value.len() > MAX_VALUE_BYTES {
            return Err(KernelError::InvalidInput(format!(
                "secret value must be at most {MAX_VALUE_BYTES} bytes"
            )));
        }
        if value.contains('\0') {
            return Err(KernelError::InvalidInput(
                "secret value must not contain NUL".into(),
            ));
        }
        let at = now_secs();
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO plugin_secrets (plugin_id, handle, value, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?4)
                 ON CONFLICT(plugin_id, handle) DO UPDATE SET
                     value = excluded.value,
                     updated_at = excluded.updated_at",
                params![plugin_id, handle, value, at],
            )?;
            Ok(())
        })?;
        self.one(plugin_id, handle)?
            .ok_or_else(|| KernelError::Storage("secret row disappeared after write".into()))
    }

    /// Read one value. The caller must already have an active `secret.use`
    /// grant covering the handle and the destination host.
    pub fn get(&self, plugin_id: &str, handle: &str) -> Result<Option<String>> {
        validate_plugin_id(plugin_id)?;
        validate_handle(handle)?;
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT value FROM plugin_secrets WHERE plugin_id = ?1 AND handle = ?2",
                    params![plugin_id, handle],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    pub fn delete(&self, plugin_id: &str, handle: &str) -> Result<bool> {
        validate_plugin_id(plugin_id)?;
        validate_handle(handle)?;
        self.db.with_write(|connection| {
            let removed = connection.execute(
                "DELETE FROM plugin_secrets WHERE plugin_id = ?1 AND handle = ?2",
                params![plugin_id, handle],
            )?;
            Ok(removed > 0)
        })
    }

    /// Metadata only; values never leave through this method.
    pub fn handles(&self, plugin_id: &str) -> Result<Vec<SecretHandle>> {
        validate_plugin_id(plugin_id)?;
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT handle, created_at, updated_at FROM plugin_secrets
                 WHERE plugin_id = ?1 ORDER BY handle",
            )?;
            let rows = statement.query_map(params![plugin_id], |row| {
                Ok(SecretHandle {
                    handle: row.get(0)?,
                    created_at: row.get(1)?,
                    updated_at: row.get(2)?,
                })
            })?;
            let mut handles = Vec::new();
            for row in rows {
                handles.push(row?);
            }
            Ok(handles)
        })
    }

    fn one(&self, plugin_id: &str, handle: &str) -> Result<Option<SecretHandle>> {
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT handle, created_at, updated_at FROM plugin_secrets
                     WHERE plugin_id = ?1 AND handle = ?2",
                    params![plugin_id, handle],
                    |row| {
                        Ok(SecretHandle {
                            handle: row.get(0)?,
                            created_at: row.get(1)?,
                            updated_at: row.get(2)?,
                        })
                    },
                )
                .optional()?)
        })
    }
}

fn validate_plugin_id(plugin_id: &str) -> Result<()> {
    if plugin_id.trim().is_empty() {
        return Err(KernelError::InvalidInput("plugin id is required".into()));
    }
    Ok(())
}

fn validate_handle(handle: &str) -> Result<()> {
    if handle.is_empty() || handle.len() > MAX_HANDLE_LEN {
        return Err(KernelError::InvalidInput(format!(
            "secret handle must contain 1..={MAX_HANDLE_LEN} characters"
        )));
    }
    if !handle
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return Err(KernelError::InvalidInput(
            "secret handle must be alphanumeric, '-' or '_'".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::migrations;

    fn store() -> (tempfile::TempDir, SecretStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("secrets.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        (dir, SecretStore::new(db))
    }

    #[test]
    fn stores_reads_and_replaces_a_value() {
        let (_dir, store) = store();
        store.put("collector", "api-key", "first").unwrap();
        assert_eq!(
            store.get("collector", "api-key").unwrap().as_deref(),
            Some("first")
        );
        store.put("collector", "api-key", "second").unwrap();
        assert_eq!(
            store.get("collector", "api-key").unwrap().as_deref(),
            Some("second")
        );
        assert_eq!(store.handles("collector").unwrap().len(), 1);
    }

    #[test]
    fn handles_are_isolated_per_plugin() {
        let (_dir, store) = store();
        store.put("a", "token", "secret-a").unwrap();
        store.put("b", "token", "secret-b").unwrap();
        assert_eq!(
            store.get("a", "token").unwrap().as_deref(),
            Some("secret-a")
        );
        assert_eq!(
            store.get("b", "token").unwrap().as_deref(),
            Some("secret-b")
        );
    }

    #[test]
    fn delete_is_idempotent_and_reports_whether_a_row_existed() {
        let (_dir, store) = store();
        store.put("collector", "key", "value").unwrap();
        assert!(store.delete("collector", "key").unwrap());
        assert!(!store.delete("collector", "key").unwrap());
        assert_eq!(store.get("collector", "key").unwrap(), None);
    }

    #[test]
    fn rejects_bad_handles_and_empty_or_oversized_values() {
        let (_dir, store) = store();
        assert!(store.put("collector", "bad handle", "x").is_err());
        assert!(store.put("collector", "key", "").is_err());
        assert!(store.put("collector", "key", &"x".repeat(8193)).is_err());
        assert!(store.put("collector", "key", "a\0b").is_err());
    }
}
