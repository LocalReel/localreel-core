//! Host-managed blob storage for `blob.read` / `blob.write`.
//!
//! A plugin gets a namespace keyed by its id, a per-blob size cap and a total
//! quota from its grant. It never chooses a filesystem path. Blobs are stored
//! in SQLite in phase 2; moving them to the data directory is an implementation
//! detail that must keep the same quota/namespace checks.

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub const MAX_BLOB_BYTES: u64 = 16 * 1024 * 1024;
const MAX_NAME_BYTES: usize = 128;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlobMeta {
    pub name: String,
    pub size: u64,
    pub digest: String,
    pub updated_at: i64,
}

#[derive(Clone)]
pub struct BlobStore {
    db: Database,
}

impl BlobStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Store or replace one blob, enforcing the per-blob cap and the plugin's
    /// total quota. Replacing a blob counts only the difference.
    pub fn put(
        &self,
        plugin_id: &str,
        name: &str,
        bytes: &[u8],
        quota_bytes: u64,
    ) -> Result<BlobMeta> {
        validate_plugin(plugin_id)?;
        validate_name(name)?;
        if bytes.len() as u64 > MAX_BLOB_BYTES {
            return Err(KernelError::InvalidInput(format!(
                "blob must be at most {MAX_BLOB_BYTES} bytes"
            )));
        }
        let existing = self.size_of(plugin_id, name)?.unwrap_or(0);
        let usage = self.usage(plugin_id)?;
        let next = usage
            .saturating_sub(existing)
            .saturating_add(bytes.len() as u64);
        if next > quota_bytes {
            return Err(KernelError::InvalidInput(
                "blob quota exceeded for this plugin".to_string(),
            ));
        }
        let at = now_secs();
        let digest = digest_hex(bytes);
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO plugin_blobs (
                     plugin_id, name, data, size, digest, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                 ON CONFLICT(plugin_id, name) DO UPDATE SET
                     data = excluded.data,
                     size = excluded.size,
                     digest = excluded.digest,
                     updated_at = excluded.updated_at",
                params![plugin_id, name, bytes, bytes.len() as i64, digest, at],
            )?;
            Ok(())
        })?;
        Ok(BlobMeta {
            name: name.to_string(),
            size: bytes.len() as u64,
            digest,
            updated_at: at,
        })
    }

    pub fn get(&self, plugin_id: &str, name: &str) -> Result<Option<Vec<u8>>> {
        validate_plugin(plugin_id)?;
        validate_name(name)?;
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT data FROM plugin_blobs WHERE plugin_id = ?1 AND name = ?2",
                    params![plugin_id, name],
                    |row| row.get(0),
                )
                .optional()?)
        })
    }

    pub fn list(&self, plugin_id: &str) -> Result<Vec<BlobMeta>> {
        validate_plugin(plugin_id)?;
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT name, size, digest, updated_at FROM plugin_blobs
                 WHERE plugin_id = ?1 ORDER BY name",
            )?;
            let rows = statement.query_map(params![plugin_id], |row| {
                let size: i64 = row.get(1)?;
                Ok(BlobMeta {
                    name: row.get(0)?,
                    size: size.max(0) as u64,
                    digest: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            })?;
            let mut blobs = Vec::new();
            for row in rows {
                blobs.push(row?);
            }
            Ok(blobs)
        })
    }

    pub fn delete(&self, plugin_id: &str, name: &str) -> Result<bool> {
        validate_plugin(plugin_id)?;
        validate_name(name)?;
        self.db.with_write(|connection| {
            let removed = connection.execute(
                "DELETE FROM plugin_blobs WHERE plugin_id = ?1 AND name = ?2",
                params![plugin_id, name],
            )?;
            Ok(removed > 0)
        })
    }

    pub fn usage(&self, plugin_id: &str) -> Result<u64> {
        validate_plugin(plugin_id)?;
        self.db.with_read(|connection| {
            let total: i64 = connection.query_row(
                "SELECT COALESCE(SUM(size), 0) FROM plugin_blobs WHERE plugin_id = ?1",
                params![plugin_id],
                |row| row.get(0),
            )?;
            Ok(total.max(0) as u64)
        })
    }

    fn size_of(&self, plugin_id: &str, name: &str) -> Result<Option<u64>> {
        self.db.with_read(|connection| {
            let size: Option<i64> = connection
                .query_row(
                    "SELECT size FROM plugin_blobs WHERE plugin_id = ?1 AND name = ?2",
                    params![plugin_id, name],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(size.map(|size| size.max(0) as u64))
        })
    }
}

fn validate_plugin(plugin_id: &str) -> Result<()> {
    if plugin_id.trim().is_empty() {
        return Err(KernelError::InvalidInput("plugin id is required".into()));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name.len() > MAX_NAME_BYTES {
        return Err(KernelError::InvalidInput(format!(
            "blob name must contain 1..={MAX_NAME_BYTES} bytes"
        )));
    }
    if name.contains(['/', '\\']) || name == "." || name == ".." || name.contains("..") {
        return Err(KernelError::InvalidInput(
            "blob name must not contain a path or '..'".into(),
        ));
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        return Err(KernelError::InvalidInput(
            "blob name must be alphanumeric, '-', '_' or '.'".into(),
        ));
    }
    Ok(())
}

fn digest_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("blob.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        (dir, BlobStore::new(db))
    }

    #[test]
    fn stores_reads_and_replaces_a_blob() {
        let (_dir, store) = store();
        let meta = store.put("collector", "page-1.json", b"one", 1024).unwrap();
        assert_eq!(meta.size, 3);
        assert_eq!(
            store.get("collector", "page-1.json").unwrap().as_deref(),
            Some(b"one".as_slice())
        );
        store.put("collector", "page-1.json", b"two", 1024).unwrap();
        assert_eq!(
            store.get("collector", "page-1.json").unwrap().as_deref(),
            Some(b"two".as_slice())
        );
        assert_eq!(store.usage("collector").unwrap(), 3);
    }

    #[test]
    fn namespaces_are_isolated_per_plugin() {
        let (_dir, store) = store();
        store.put("a", "x", b"a", 1024).unwrap();
        store.put("b", "x", b"b", 1024).unwrap();
        assert_eq!(
            store.get("a", "x").unwrap().as_deref(),
            Some(b"a".as_slice())
        );
        assert_eq!(
            store.get("b", "x").unwrap().as_deref(),
            Some(b"b".as_slice())
        );
        assert_eq!(store.list("a").unwrap().len(), 1);
    }

    #[test]
    fn quota_is_enforced_across_blobs() {
        let (_dir, store) = store();
        store.put("collector", "a", &[0u8; 600], 1000).unwrap();
        assert!(store.put("collector", "b", &[0u8; 600], 1000).is_err());
        assert_eq!(store.usage("collector").unwrap(), 600);
        // Replacing the same blob only counts the delta.
        store.put("collector", "a", &[0u8; 900], 1000).unwrap();
        assert_eq!(store.usage("collector").unwrap(), 900);
    }

    #[test]
    fn rejects_path_like_names_and_oversized_blobs() {
        let (_dir, store) = store();
        for name in ["../x", "a/b", "a\\b", "..", ""] {
            assert!(store.put("collector", name, b"x", 1024).is_err(), "{name}");
        }
        assert!(store
            .put(
                "collector",
                "big",
                &vec![0u8; (MAX_BLOB_BYTES + 1) as usize],
                MAX_BLOB_BYTES + 1
            )
            .is_err());
    }

    #[test]
    fn delete_reports_whether_a_blob_existed() {
        let (_dir, store) = store();
        store.put("collector", "x", b"1", 1024).unwrap();
        assert!(store.delete("collector", "x").unwrap());
        assert!(!store.delete("collector", "x").unwrap());
        assert_eq!(store.get("collector", "x").unwrap(), None);
    }
}
