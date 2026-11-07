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
    /// `storage:read`, `network`, or `None` for lifecycle events.
    pub permission: Option<String>,
    /// `start`, `stop`, `network.allowed`, `network.denied`, ...
    pub action: String,
    pub detail: Option<String>,
    pub at: i64,
}

/// Kernel-style migrations for the plugin domain.
pub fn migrations() -> &'static [Migration] {
    &[Migration {
        version: 1,
        name: "plugin_audit",
        sql: include_str!("../migrations/0001_plugin_audit.sql"),
    }]
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

    /// Recent entries, newest first, optionally for one plugin.
    pub fn recent(&self, plugin_id: Option<&str>, limit: i64) -> Result<Vec<AuditEntry>> {
        self.db.with_read(|connection| {
            let limit = limit.max(1);
            let mut entries = Vec::new();
            match plugin_id {
                Some(id) => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, permission, action, detail, at
                         FROM plugin_audit WHERE plugin_id = ?1
                         ORDER BY id DESC LIMIT ?2",
                    )?;
                    for row in statement.query_map(params![id, limit], row_to_entry)? {
                        entries.push(row?);
                    }
                }
                None => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, permission, action, detail, at
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
