//! Append-only record of package verification.
//!
//! This is not a public transparency log, but it answers the operator question
//! that a signature alone does not: *which* key verified *which* package, and
//! when. Rows are written by the host before a plugin is extracted.

use std::path::Path;
use std::sync::Arc;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension};
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
    /// Hash chain over this row and the previous one; `None` for legacy rows.
    pub chain_hash: Option<String>,
}

#[derive(Clone)]
pub struct VerificationStore {
    db: Database,
    anchor: Option<Arc<SigningKey>>,
}

impl VerificationStore {
    pub fn new(db: Database) -> Self {
        Self { db, anchor: None }
    }

    /// Load (or create) the host audit key and sign every new chain head.
    pub fn with_anchor(db: Database, key_path: &Path) -> Result<Self> {
        let key = Arc::new(load_or_create_key(key_path)?);
        Ok(Self {
            db,
            anchor: Some(key),
        })
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
        let anchor = self.anchor.clone();
        let (id, chain_hash) = self.db.with_write(|connection| {
            let previous: Option<String> = connection
                .query_row(
                    "SELECT chain_hash FROM plugin_package_verifications
                     WHERE chain_hash IS NOT NULL ORDER BY id DESC LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let chain = chain_hash(
                previous.as_deref(),
                &ChainInput {
                    plugin_id,
                    plugin_version,
                    package_sha256,
                    publisher,
                    public_key_fingerprint: fingerprint.as_deref(),
                    signature_verified,
                    at,
                },
            );
            connection.execute(
                "INSERT INTO plugin_package_verifications (
                     plugin_id, plugin_version, package_sha256, publisher,
                     public_key_fingerprint, signature_verified, at, chain_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    plugin_id,
                    plugin_version,
                    package_sha256,
                    publisher,
                    fingerprint,
                    signature_verified as i64,
                    at,
                    chain
                ],
            )?;
            if let Some(key) = &anchor {
                let signature = key.sign(chain.as_bytes());
                let public_key = key.verifying_key().to_bytes();
                connection.execute(
                    "INSERT INTO plugin_verification_anchors
                         (chain_hash, signature, public_key, at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        chain,
                        hex(signature.to_bytes().as_slice()),
                        hex(public_key.as_slice()),
                        at
                    ],
                )?;
            }
            Ok((connection.last_insert_rowid(), chain))
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
            chain_hash: Some(chain_hash),
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
                                public_key_fingerprint, signature_verified, at, chain_hash
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
                                public_key_fingerprint, signature_verified, at, chain_hash
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
        chain_hash: row.get(8)?,
    })
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct VerificationChain {
    pub checked: u64,
    pub valid: bool,
    pub broken_at: Option<i64>,
    pub anchored: bool,
    pub anchor_valid: Option<bool>,
}

impl VerificationStore {
    /// Recompute the chain in insertion order. `chain_hash` is null only on
    /// legacy rows written before the chain existed; those are skipped.
    pub fn verify_chain(&self) -> Result<VerificationChain> {
        self.db.with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, plugin_id, plugin_version, package_sha256, publisher,
                        public_key_fingerprint, signature_verified, at, chain_hash
                 FROM plugin_package_verifications
                 WHERE chain_hash IS NOT NULL ORDER BY id ASC",
            )?;
            let mapped = statement.query_map([], row_to_verification)?;
            let mut previous: Option<String> = None;
            let mut checked = 0u64;
            for row in mapped {
                let row = row?;
                let expected = chain_hash(
                    previous.as_deref(),
                    &ChainInput {
                        plugin_id: &row.plugin_id,
                        plugin_version: &row.plugin_version,
                        package_sha256: &row.package_sha256,
                        publisher: row.publisher.as_deref(),
                        public_key_fingerprint: row.public_key_fingerprint.as_deref(),
                        signature_verified: row.signature_verified,
                        at: row.at,
                    },
                );
                if row.chain_hash.as_deref() != Some(expected.as_str()) {
                    return Ok(VerificationChain {
                        checked,
                        valid: false,
                        broken_at: Some(row.id),
                        anchored: false,
                        anchor_valid: None,
                    });
                }
                previous = Some(expected);
                checked += 1;
            }
            let latest: Option<(String, String, String)> = connection
                .query_row(
                    "SELECT chain_hash, signature, public_key
                     FROM plugin_verification_anchors ORDER BY id DESC LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let head = previous.as_deref().unwrap_or("genesis");
            let (anchored, anchor_valid) = match latest {
                Some((chain, signature, public_key)) => (
                    true,
                    Some(chain == head && verify_anchor(&chain, &signature, &public_key)),
                ),
                None => (false, None),
            };
            Ok(VerificationChain {
                checked,
                valid: true,
                broken_at: None,
                anchored,
                anchor_valid,
            })
        })
    }
}

fn chain_hash(previous: Option<&str>, row: &ChainInput<'_>) -> String {
    let mut hasher = Sha256::new();
    for field in [
        previous.unwrap_or("genesis"),
        row.plugin_id,
        row.plugin_version,
        row.package_sha256,
        row.publisher.unwrap_or(""),
        row.public_key_fingerprint.unwrap_or(""),
    ] {
        hasher.update((field.len() as u64).to_le_bytes());
        hasher.update(field.as_bytes());
    }
    hasher.update([row.signature_verified as u8]);
    hasher.update(row.at.to_le_bytes());
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct ChainInput<'a> {
    plugin_id: &'a str,
    plugin_version: &'a str,
    package_sha256: &'a str,
    publisher: Option<&'a str>,
    public_key_fingerprint: Option<&'a str>,
    signature_verified: bool,
    at: i64,
}

/// Stable identifier for a public key without storing the key itself.
pub fn key_fingerprint(public_key_hex: &str) -> String {
    let digest = Sha256::digest(public_key_hex.trim().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn verify_anchor(chain: &str, signature: &str, public_key: &str) -> bool {
    let Ok(public_key) = decode_hex(public_key) else {
        return false;
    };
    let Ok(signature) = decode_hex(signature) else {
        return false;
    };
    let Ok(public_key) = <[u8; 32]>::try_from(public_key.as_slice()) else {
        return false;
    };
    let Ok(signature) = Signature::from_slice(&signature) else {
        return false;
    };
    VerifyingKey::from_bytes(&public_key)
        .map(|key| key.verify(chain.as_bytes(), &signature).is_ok())
        .unwrap_or(false)
}

fn load_or_create_key(path: &Path) -> Result<SigningKey> {
    if path.is_file() {
        let text = std::fs::read_to_string(path)
            .map_err(|error| KernelError::Storage(format!("audit key: {error}")))?;
        let seed = decode_hex(text.trim())
            .map_err(|_| KernelError::Storage("audit key is not valid hex".into()))?;
        let seed: [u8; 32] = seed
            .try_into()
            .map_err(|_| KernelError::Storage("audit key must be 32 bytes".into()))?;
        return Ok(SigningKey::from_bytes(&seed));
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| KernelError::Storage(format!("audit key dir: {error}")))?;
        }
    }
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed)
        .map_err(|error| KernelError::Storage(format!("audit key entropy: {error}")))?;
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(path)
            .map_err(|error| KernelError::Storage(format!("audit key create: {error}")))?;
        file.write_all(hex(&seed).as_bytes())
            .map_err(|error| KernelError::Storage(format!("audit key write: {error}")))?;
    }
    #[cfg(windows)]
    restrict_key_acl(path);
    Ok(SigningKey::from_bytes(&seed))
}

#[cfg(windows)]
fn restrict_key_acl(path: &Path) {
    let Some(user) = std::env::var_os("USERNAME") else {
        eprintln!("audit key ACL: USERNAME is unavailable");
        return;
    };
    let status = std::process::Command::new("icacls")
        .arg(path)
        .arg("/inheritance:r")
        .arg("/grant:r")
        .arg(format!("{}:F", user.to_string_lossy()))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !status.is_ok_and(|status| status.success()) {
        eprintln!(
            "audit key ACL: failed to restrict {} to the current user",
            path.display()
        );
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn decode_hex(text: &str) -> std::result::Result<Vec<u8>, ()> {
    if !text.len().is_multiple_of(2) {
        return Err(());
    }
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).ok_or(())?;
            let low = (pair[1] as char).to_digit(16).ok_or(())?;
            Ok(((high << 4) | low) as u8)
        })
        .collect()
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

    fn anchored_store() -> (tempfile::TempDir, VerificationStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("verify.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        let store = VerificationStore::with_anchor(db, &dir.path().join("audit.key")).unwrap();
        (dir, store)
    }

    #[cfg(windows)]
    #[test]
    fn audit_key_acl_is_restricted_to_the_current_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.key");
        load_or_create_key(&path).unwrap();
        let output = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&output.stdout);
        let user = std::env::var("USERNAME").unwrap();
        assert!(text.contains(&user), "{text}");
        assert!(!text.contains("BUILTIN\\Users"), "{text}");
    }

    #[test]
    fn anchor_signs_the_chain_head() {
        let (_dir, store) = anchored_store();
        store
            .record("demo", "0.1.0", &"a".repeat(64), Some("acme"), None, true)
            .unwrap();
        store
            .record("demo", "0.2.0", &"b".repeat(64), Some("acme"), None, true)
            .unwrap();
        let chain = store.verify_chain().unwrap();
        assert!(chain.valid);
        assert!(chain.anchored);
        assert_eq!(chain.anchor_valid, Some(true));
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
    fn chain_detects_tampering_and_reordering() {
        let (_dir, store) = store();
        store
            .record("demo", "0.1.0", &"a".repeat(64), Some("acme"), None, true)
            .unwrap();
        store
            .record("demo", "0.2.0", &"b".repeat(64), Some("acme"), None, true)
            .unwrap();
        assert!(store.verify_chain().unwrap().valid);

        store
            .db
            .with_write(|connection| {
                connection.execute(
                    "UPDATE plugin_package_verifications SET publisher = 'mallory'
                     WHERE plugin_version = '0.2.0'",
                    [],
                )?;
                Ok(())
            })
            .unwrap();
        let chain = store.verify_chain().unwrap();
        assert!(!chain.valid);
        assert_eq!(chain.checked, 1);
        assert!(chain.broken_at.is_some());
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
