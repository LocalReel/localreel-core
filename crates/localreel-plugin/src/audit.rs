//! Plugin audit trail.
//!
//! Declaring a permission only means something if the host records when it is
//! used. Every start, refusal and proxied request leaves a row here; the admin
//! page shows the recent ones per plugin.

use localreel_kernel::db::Database;
use localreel_kernel::error::Result;
use localreel_kernel::migrations::Migration;
use localreel_kernel::time::now_secs;
use rusqlite::{params, Row};
use serde::{Deserialize, Serialize};

/// One recorded action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub plugin_id: String,
    /// `storage:read`, `network`, or `None` for lifecycle events. Kept for old
    /// rows and old clients; new capability calls use `capability`.
    pub permission: Option<String>,
    /// `start`, `stop`, `network.allowed`, `network.denied`, ...
    pub action: String,
    pub detail: Option<String>,
    pub at: i64,
    /// The capability that was exercised, when this is a capability call.
    pub capability: Option<String>,
    /// Digest of the approved scope, so a call can be matched to a grant.
    pub scope_digest: Option<String>,
    /// `allowed`, `denied`, `error`, `granted`, `revoked`, ...
    pub result: Option<String>,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
    pub duration_ms: Option<i64>,
    pub request_id: Option<String>,
    pub job_id: Option<String>,
    pub user_id: Option<i64>,
}

/// One host-written capability event.
#[derive(Debug)]
pub struct AuditEvent<'a> {
    pub plugin_id: &'a str,
    pub capability: Option<&'a str>,
    pub action: &'a str,
    pub result: &'a str,
    pub detail: Option<&'a str>,
    pub scope_digest: Option<&'a str>,
    pub bytes_in: Option<i64>,
    pub bytes_out: Option<i64>,
    pub duration_ms: Option<i64>,
    pub request_id: Option<&'a str>,
    pub job_id: Option<&'a str>,
    pub user_id: Option<i64>,
}

/// Kernel-style migrations for the plugin domain.
pub fn migrations() -> &'static [Migration] {
    &[
        Migration {
            version: 1,
            name: "plugin_audit",
            sql: include_str!("../migrations/0001_plugin_audit.sql"),
        },
        Migration {
            version: 2,
            name: "plugin_capabilities",
            sql: include_str!("../migrations/0002_plugin_capabilities.sql"),
        },
    ]
}

#[derive(Clone)]
pub struct AuditLog {
    db: Database,
}

impl AuditLog {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub fn record(
        &self,
        plugin_id: &str,
        permission: Option<&str>,
        action: &str,
        detail: &str,
    ) -> Result<()> {
        let at = now_secs();
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO plugin_audit (plugin_id, permission, action, detail, at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![plugin_id, permission, action, detail, at],
            )?;
            Ok(())
        })
    }

    /// Record a capability event with the attribution fields the broker uses.
    pub fn record_event(&self, event: &AuditEvent<'_>) -> Result<()> {
        let at = now_secs();
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO plugin_audit (
                     plugin_id, permission, action, detail, at,
                     capability, scope_digest, result, bytes_in, bytes_out,
                     duration_ms, request_id, job_id, user_id
                 ) VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    event.plugin_id,
                    event.action,
                    event.detail,
                    at,
                    event.capability,
                    event.scope_digest,
                    event.result,
                    event.bytes_in,
                    event.bytes_out,
                    event.duration_ms,
                    event.request_id,
                    event.job_id,
                    event.user_id
                ],
            )?;
            Ok(())
        })
    }

    /// Recent entries, newest first, optionally for one plugin.
    pub fn recent(&self, plugin_id: Option<&str>, limit: i64) -> Result<Vec<AuditEntry>> {
        self.db.with_read(|connection| {
            let limit = limit.max(1);
            let mut entries = Vec::new();
            match plugin_id {
                Some(id) => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, permission, action, detail, at,
                                capability, scope_digest, result, bytes_in, bytes_out,
                                duration_ms, request_id, job_id, user_id
                         FROM plugin_audit WHERE plugin_id = ?1
                         ORDER BY id DESC LIMIT ?2",
                    )?;
                    for row in statement.query_map(params![id, limit], row_to_entry)? {
                        entries.push(row?);
                    }
                }
                None => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, permission, action, detail, at,
                                capability, scope_digest, result, bytes_in, bytes_out,
                                duration_ms, request_id, job_id, user_id
                         FROM plugin_audit ORDER BY id DESC LIMIT ?1",
                    )?;
                    for row in statement.query_map([limit], row_to_entry)? {
                        entries.push(row?);
                    }
                }
            }
            Ok(entries)
        })
    }

    /// Drop entries older than `max_age_secs`, so the table cannot grow forever.
    pub fn prune(&self, max_age_secs: i64) -> Result<usize> {
        let cutoff = now_secs() - max_age_secs;
        self.db.with_write(|connection| {
            Ok(connection.execute("DELETE FROM plugin_audit WHERE at < ?1", [cutoff])?)
        })
    }
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<AuditEntry> {
    Ok(AuditEntry {
        id: row.get(0)?,
        plugin_id: row.get(1)?,
        permission: row.get(2)?,
        action: row.get(3)?,
        detail: row.get(4)?,
        at: row.get(5)?,
        capability: row.get(6)?,
        scope_digest: row.get(7)?,
        result: row.get(8)?,
        bytes_in: row.get(9)?,
        bytes_out: row.get(10)?,
        duration_ms: row.get(11)?,
        request_id: row.get(12)?,
        job_id: row.get(13)?,
        user_id: row.get(14)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::OptionalExtension;

    fn setup() -> (tempfile::TempDir, AuditLog) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("audit.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", migrations()).unwrap();
        (dir, AuditLog::new(db))
    }

    #[test]
    fn records_and_lists_newest_first() {
        let (_dir, log) = setup();
        log.record("webdav", None, "start", "version 0.1.0")
            .unwrap();
        log.record("webdav", Some("network"), "network.allowed", "nas.local")
            .unwrap();
        log.record("strm", Some("network"), "network.denied", "example.com")
            .unwrap();

        let all = log.recent(None, 10).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].plugin_id, "strm", "newest first");
        assert_eq!(all[0].permission.as_deref(), Some("network"));
        assert_eq!(all[2].action, "start");
        assert_eq!(all[2].permission, None);

        let only = log.recent(Some("webdav"), 10).unwrap();
        assert_eq!(only.len(), 2);
        assert!(only.iter().all(|entry| entry.plugin_id == "webdav"));
    }

    #[test]
    fn records_capability_fields_and_keeps_legacy_rows() {
        let (_dir, log) = setup();
        log.record("webdav", Some("network"), "network.allowed", "nas.local")
            .unwrap();
        log.record_event(&AuditEvent {
            plugin_id: "webdav",
            capability: Some("net.fetch"),
            action: "capability.call",
            result: "allowed",
            detail: Some("GET api.example.com"),
            scope_digest: Some("abc"),
            bytes_in: Some(0),
            bytes_out: Some(42),
            duration_ms: Some(7),
            request_id: Some("req-1"),
            job_id: Some("job-1"),
            user_id: Some(3),
        })
        .unwrap();

        let entries = log.recent(Some("webdav"), 10).unwrap();
        assert_eq!(entries.len(), 2);
        let newest = &entries[0];
        assert_eq!(newest.capability.as_deref(), Some("net.fetch"));
        assert_eq!(newest.result.as_deref(), Some("allowed"));
        assert_eq!(newest.bytes_out, Some(42));
        assert_eq!(newest.request_id.as_deref(), Some("req-1"));
        assert_eq!(newest.user_id, Some(3));
        assert_eq!(newest.permission, None);

        let legacy = &entries[1];
        assert_eq!(legacy.permission.as_deref(), Some("network"));
        assert_eq!(legacy.capability, None);
    }

    #[test]
    fn the_migration_creates_the_grants_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("m.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", migrations()).unwrap();
        let exists: Option<i64> = db
            .with_read(|connection| {
                Ok(connection
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'plugin_grants'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?)
            })
            .unwrap();
        assert_eq!(exists, Some(1));
    }

    #[test]
    fn pruning_drops_only_old_rows() {
        let (_dir, log) = setup();
        log.record("webdav", None, "start", "old").unwrap();
        // Age the row instead of waiting.
        log.db
            .with_write(|connection| {
                connection.execute("UPDATE plugin_audit SET at = 0", [])?;
                Ok(())
            })
            .unwrap();
        log.record("webdav", None, "start", "new").unwrap();

        assert_eq!(log.prune(60).unwrap(), 1);
        let left = log.recent(None, 10).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].detail.as_deref(), Some("new"));
    }

    #[test]
    fn the_migration_creates_the_table() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("m.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", migrations()).unwrap();
        let exists: Option<i64> = db
            .with_read(|connection| {
                Ok(connection
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'plugin_audit'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?)
            })
            .unwrap();
        assert_eq!(exists, Some(1));
    }
}
