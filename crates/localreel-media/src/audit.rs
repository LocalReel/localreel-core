//! Append-only record of administrator actions.
//!
//! Deliberately small: an action, who did it, what it was done to, and a short
//! note. It is not an event log and it never stores request bodies, so a
//! password reset records *that* it happened, not the password.

use localreel_kernel::error::Result;
use localreel_kernel::time::now_secs;
use rusqlite::{params, Row};
use serde::Serialize;

use crate::service::MediaService;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminAuditEntry {
    pub id: i64,
    pub at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

fn row_to_entry(row: &Row<'_>) -> rusqlite::Result<AdminAuditEntry> {
    Ok(AdminAuditEntry {
        id: row.get(0)?,
        at: row.get(1)?,
        user_id: row.get(2)?,
        username: row.get(3)?,
        action: row.get(4)?,
        target: row.get(5)?,
        detail: row.get(6)?,
    })
}

impl MediaService {
    /// Record one action. Callers pass the actor, never a request body.
    pub fn record_admin(
        &self,
        user_id: Option<i64>,
        username: Option<&str>,
        action: &str,
        target: Option<&str>,
        detail: Option<&str>,
    ) -> Result<()> {
        let at = now_secs();
        self.database().with_write(|connection| {
            connection.execute(
                "INSERT INTO admin_audit (at, user_id, username, action, target, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![at, user_id, username, action, target, detail],
            )?;
            Ok(())
        })
    }

    /// Newest first, capped by the caller.
    pub fn admin_audit(&self, limit: i64) -> Result<Vec<AdminAuditEntry>> {
        let limit = limit.clamp(1, 200);
        self.database().with_read(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, at, user_id, username, action, target, detail
                 FROM admin_audit ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = statement
                .query_map([limit], row_to_entry)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localreel_kernel::db::Database;
    use localreel_kernel::migrations;

    fn setup() -> (tempfile::TempDir, MediaService) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("audit.db").to_str().unwrap(), 1).unwrap();
        migrations::apply(&db, "kernel", localreel_kernel::jobs::migrations()).unwrap();
        migrations::apply(&db, "media", crate::migrations::list()).unwrap();
        (dir, MediaService::new(db))
    }

    #[test]
    fn records_and_reads_back_newest_first() {
        let (_dir, service) = setup();
        service
            .record_admin(Some(1), Some("owner"), "source.delete", Some("4"), None)
            .unwrap();
        service
            .record_admin(
                Some(1),
                Some("owner"),
                "server.shutdown",
                None,
                Some("from the console"),
            )
            .unwrap();
        let entries = service.admin_audit(10).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].action, "server.shutdown");
        assert_eq!(entries[0].detail.as_deref(), Some("from the console"));
        assert_eq!(entries[1].action, "source.delete");
        assert_eq!(entries[1].target.as_deref(), Some("4"));
        assert_eq!(entries[1].username.as_deref(), Some("owner"));
    }

    #[test]
    fn the_limit_is_clamped() {
        let (_dir, service) = setup();
        for index in 0..5 {
            service
                .record_admin(None, None, "test", Some(&index.to_string()), None)
                .unwrap();
        }
        assert_eq!(service.admin_audit(0).unwrap().len(), 1);
        assert_eq!(service.admin_audit(3).unwrap().len(), 3);
        assert_eq!(service.admin_audit(500).unwrap().len(), 5);
    }
}
