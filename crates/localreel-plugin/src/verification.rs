//! Append-only record of package verification.
//!
//! This is not a public transparency log, but it answers the operator question
//! that a signature alone does not: *which* key verified *which* package, and
//! when. Rows are written by the host before a plugin is extracted.

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::params;
use serde::Serialize;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageVerification {
    pub id: i64,
    pub plugin_id: String,
    pub plugin_version: String,
    pub package_sha256: String,
    pub publisher: Option<String>,
    pub public_key_fingerprint: Option<String>,
    pub signature_verified: bool,
    pub at: i64,
}

#[derive(Clone)]
pub struct VerificationStore {
    db: Database,
}

impl VerificationStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn record(
        &self,
        plugin_id: &str,
        plugin_version: &str,
        package_sha256: &str,
        publisher: Option<&str>,
        public_key: Option<&str>,
        signature_verified: bool,
    ) -> Result<PackageVerification> {
        if plugin_id.trim().is_empty()
            || plugin_version.trim().is_empty()
            || package_sha256.trim().is_empty()
        {
            return Err(KernelError::InvalidInput(
                "plugin id, version and package digest are required".into(),
            ));
        }
        let fingerprint = public_key.map(key_fingerprint);
        let at = now_secs();
        let id = self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO plugin_package_verifications (
                     plugin_id, plugin_version, package_sha256, publisher,
                     public_key_fingerprint, signature_verified, at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    plugin_id,
                    plugin_version,
                    package_sha256,
                    publisher,
                    fingerprint,
                    signature_verified as i64,
                    at
                ],
            )?;
            Ok(connection.last_insert_rowid())
        })?;
        Ok(PackageVerification {
            id,
            plugin_id: plugin_id.to_string(),
            plugin_version: plugin_version.to_string(),
            package_sha256: package_sha256.to_string(),
            publisher: publisher.map(str::to_string),
            public_key_fingerprint: fingerprint,
            signature_verified,
            at,
        })
    }

    pub fn recent(
        &self,
        plugin_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PackageVerification>> {
        self.db.with_read(|connection| {
            let limit = limit.clamp(1, 500) as i64;
            let mut rows = Vec::new();
            match plugin_id {
                Some(plugin_id) => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, plugin_version, package_sha256, publisher,
                                public_key_fingerprint, signature_verified, at
                         FROM plugin_package_verifications WHERE plugin_id = ?1
                         ORDER BY id DESC LIMIT ?2",
                    )?;
                    let mapped =
                        statement.query_map(params![plugin_id, limit], row_to_verification)?;
                    for row in mapped {
                        rows.push(row?);
                    }
                }
                None => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, plugin_version, package_sha256, publisher,
                                public_key_fingerprint, signature_verified, at
                         FROM plugin_package_verifications ORDER BY id DESC LIMIT ?1",
                    )?;
                    let mapped = statement.query_map(params![limit], row_to_verification)?;
                    for row in mapped {
                        rows.push(row?);
                    }
                }
            }
            Ok(rows)
        })
    }
}

fn row_to_verification(row: &rusqlite::Row<'_>) -> rusqlite::Result<PackageVerification> {
    let verified: i64 = row.get(6)?;
    Ok(PackageVerification {
        id: row.get(0)?,
        plugin_id: row.get(1)?,
        plugin_version: row.get(2)?,
        package_sha256: row.get(3)?,
        publisher: row.get(4)?,
        public_key_fingerprint: row.get(5)?,
        signature_verified: verified != 0,
        at: row.get(7)?,
    })
}

/// Stable identifier for a public key without storing the key itself.
pub fn key_fingerprint(public_key_hex: &str) -> String {
    let digest = Sha256::digest(public_key_hex.trim().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::migrations;

    fn store() -> (tempfile::TempDir, VerificationStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("verify.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        (dir, VerificationStore::new(db))
    }

    #[test]
    fn records_and_lists_verifications_newest_first() {
        let (_dir, store) = store();
        store
            .record(
                "demo",
                "0.1.0",
                &"a".repeat(64),
                Some("acme"),
                Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
                true,
            )
            .unwrap();
        store
            .record("demo", "0.1.0", &"b".repeat(64), None, None, false)
            .unwrap();
        let rows = store.recent(Some("demo"), 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].package_sha256, "b".repeat(64));
        assert!(!rows[0].signature_verified);
        assert_eq!(rows[1].publisher.as_deref(), Some("acme"));
        assert!(rows[1].signature_verified);
        assert!(rows[1].public_key_fingerprint.is_some());
    }

    #[test]
    fn fingerprints_are_stable_and_key_specific() {
        let first = key_fingerprint("aaaa");
        assert_eq!(first, key_fingerprint("aaaa"));
        assert_ne!(first, key_fingerprint("bbbb"));
        assert_eq!(first.len(), 64);
    }

    #[test]
    fn rejects_missing_identity() {
        let (_dir, store) = store();
        assert!(store
            .record("", "0.1.0", "digest", None, None, false)
            .is_err());
        assert!(store
            .record("demo", "", "digest", None, None, false)
            .is_err());
    }
}
