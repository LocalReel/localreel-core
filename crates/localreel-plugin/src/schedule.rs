//! Host-owned schedules for `schedule.request`.
//!
//! A plugin asks the host for a periodic callback; it does not run its own
//! timer and cannot choose outside the granted interval scope. The host stores
//! one schedule per plugin version, wakes it with `GET /v1/schedule?payload=...`
//! and records every run in the audit trail.

use localreel_kernel::db::Database;
use localreel_kernel::error::{KernelError, Result};
use localreel_kernel::time::now_secs;
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use serde_json::Value;

const MAX_PAYLOAD_BYTES: usize = 4096;
const MIN_INTERVAL_SECS: u64 = 60;
const MAX_INTERVAL_SECS: u64 = 30 * 24 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PluginSchedule {
    pub id: i64,
    pub plugin_id: String,
    pub plugin_version: String,
    pub interval_secs: u64,
    pub payload: String,
    pub next_run_at: i64,
    pub last_run_at: Option<i64>,
    pub last_error: Option<String>,
    pub enabled: bool,
}

#[derive(Clone)]
pub struct ScheduleStore {
    db: Database,
}

impl ScheduleStore {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Ask for a periodic callback. A second request for the same plugin
    /// version replaces the interval/payload and resets the next run.
    pub fn request(
        &self,
        plugin_id: &str,
        plugin_version: &str,
        interval_secs: u64,
        payload: &Value,
    ) -> Result<PluginSchedule> {
        if plugin_id.trim().is_empty() || plugin_version.trim().is_empty() {
            return Err(KernelError::InvalidInput(
                "plugin id and version are required".into(),
            ));
        }
        if !(MIN_INTERVAL_SECS..=MAX_INTERVAL_SECS).contains(&interval_secs) {
            return Err(KernelError::InvalidInput(format!(
                "schedule interval must be {MIN_INTERVAL_SECS}..={MAX_INTERVAL_SECS} seconds"
            )));
        }
        let payload = serde_json::to_string(payload)
            .map_err(|error| KernelError::InvalidInput(error.to_string()))?;
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(KernelError::InvalidInput(format!(
                "schedule payload must be at most {MAX_PAYLOAD_BYTES} bytes"
            )));
        }
        let at = now_secs();
        let next_run_at = at.saturating_add(interval_secs as i64);
        self.db.with_write(|connection| {
            connection.execute(
                "INSERT INTO plugin_schedules (
                     plugin_id, plugin_version, interval_secs, payload,
                     next_run_at, last_run_at, last_error, enabled, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, 1, ?6, ?6)
                 ON CONFLICT(plugin_id, plugin_version) DO UPDATE SET
                     interval_secs = excluded.interval_secs,
                     payload = excluded.payload,
                     next_run_at = excluded.next_run_at,
                     last_error = NULL,
                     enabled = 1,
                     updated_at = excluded.updated_at",
                params![
                    plugin_id,
                    plugin_version,
                    interval_secs as i64,
                    payload,
                    next_run_at,
                    at
                ],
            )?;
            Ok(())
        })?;
        self.one(plugin_id, plugin_version)?
            .ok_or_else(|| KernelError::Storage("schedule disappeared after write".into()))
    }

    /// Due schedules, oldest first. The caller runs them and then calls
    /// [`Self::mark`] so a failing plugin still advances its interval.
    pub fn due(&self, now: i64, limit: usize) -> Result<Vec<PluginSchedule>> {
        self.db.with_read(|connection| {
            let limit = limit.clamp(1, 64) as i64;
            let mut statement = connection.prepare(
                "SELECT id, plugin_id, plugin_version, interval_secs, payload,
                        next_run_at, last_run_at, last_error, enabled
                 FROM plugin_schedules
                 WHERE enabled = 1 AND next_run_at <= ?1
                 ORDER BY next_run_at ASC LIMIT ?2",
            )?;
            let rows = statement.query_map(params![now, limit], schedule_row)?;
            let mut schedules = Vec::new();
            for row in rows {
                schedules.push(row?);
            }
            Ok(schedules)
        })
    }

    pub fn list(&self, plugin_id: Option<&str>) -> Result<Vec<PluginSchedule>> {
        self.db.with_read(|connection| {
            let mut schedules = Vec::new();
            match plugin_id {
                Some(plugin_id) => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, plugin_version, interval_secs, payload,
                                next_run_at, last_run_at, last_error, enabled
                         FROM plugin_schedules WHERE plugin_id = ?1 ORDER BY id",
                    )?;
                    let rows = statement.query_map(params![plugin_id], schedule_row)?;
                    for row in rows {
                        schedules.push(row?);
                    }
                }
                None => {
                    let mut statement = connection.prepare(
                        "SELECT id, plugin_id, plugin_version, interval_secs, payload,
                                next_run_at, last_run_at, last_error, enabled
                         FROM plugin_schedules ORDER BY id",
                    )?;
                    let rows = statement.query_map([], schedule_row)?;
                    for row in rows {
                        schedules.push(row?);
                    }
                }
            }
            Ok(schedules)
        })
    }

    /// Record one callback attempt. `next_run_at` is always advanced, so a
    /// broken plugin cannot create a hot retry loop.
    pub fn mark(&self, id: i64, next_run_at: i64, error: Option<&str>) -> Result<()> {
        let at = now_secs();
        self.db.with_write(|connection| {
            let updated = connection.execute(
                "UPDATE plugin_schedules
                 SET last_run_at = ?2, next_run_at = ?3, last_error = ?4, updated_at = ?2
                 WHERE id = ?1",
                params![id, at, next_run_at, error],
            )?;
            if updated == 0 {
                return Err(KernelError::NotFound(format!("schedule {id}")));
            }
            Ok(())
        })
    }

    pub fn delete(&self, plugin_id: &str, plugin_version: &str) -> Result<bool> {
        self.db.with_write(|connection| {
            let removed = connection.execute(
                "DELETE FROM plugin_schedules WHERE plugin_id = ?1 AND plugin_version = ?2",
                params![plugin_id, plugin_version],
            )?;
            Ok(removed > 0)
        })
    }

    fn one(&self, plugin_id: &str, plugin_version: &str) -> Result<Option<PluginSchedule>> {
        self.db.with_read(|connection| {
            Ok(connection
                .query_row(
                    "SELECT id, plugin_id, plugin_version, interval_secs, payload,
                            next_run_at, last_run_at, last_error, enabled
                     FROM plugin_schedules
                     WHERE plugin_id = ?1 AND plugin_version = ?2",
                    params![plugin_id, plugin_version],
                    schedule_row,
                )
                .optional()?)
        })
    }
}

fn schedule_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PluginSchedule> {
    let interval_secs: i64 = row.get(3)?;
    let enabled: i64 = row.get(8)?;
    Ok(PluginSchedule {
        id: row.get(0)?,
        plugin_id: row.get(1)?,
        plugin_version: row.get(2)?,
        interval_secs: interval_secs.max(0) as u64,
        payload: row.get(4)?,
        next_run_at: row.get(5)?,
        last_run_at: row.get(6)?,
        last_error: row.get(7)?,
        enabled: enabled != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, ScheduleStore) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path().join("schedule.db").to_str().unwrap(), 1).unwrap();
        localreel_kernel::migrations::apply(&db, "plugin", crate::audit::migrations()).unwrap();
        (dir, ScheduleStore::new(db))
    }

    #[test]
    fn requests_a_schedule_and_replaces_it_on_re_request() {
        let (_dir, store) = store();
        let first = store
            .request("collector", "0.1.0", 3600, &json!({"page": 1}))
            .unwrap();
        assert_eq!(first.interval_secs, 3600);
        assert!(first.next_run_at > now_secs());
        let second = store
            .request("collector", "0.1.0", 600, &json!({"page": 2}))
            .unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.interval_secs, 600);
        assert_eq!(second.payload, r#"{"page":2}"#);
        assert_eq!(store.list(Some("collector")).unwrap().len(), 1);
    }

    #[test]
    fn intervals_and_payloads_are_bounded() {
        let (_dir, store) = store();
        assert!(store.request("collector", "0.1.0", 30, &json!({})).is_err());
        assert!(store
            .request("collector", "0.1.0", 31 * 24 * 60 * 60, &json!({}))
            .is_err());
        assert!(store
            .request(
                "collector",
                "0.1.0",
                MIN_INTERVAL_SECS,
                &json!({"data": "x".repeat(5000)})
            )
            .is_err());
    }

    #[test]
    fn due_only_returns_enabled_rows_that_have_arrived() {
        let (_dir, store) = store();
        let schedule = store
            .request("collector", "0.1.0", MIN_INTERVAL_SECS, &json!({}))
            .unwrap();
        assert!(store.due(now_secs(), 10).unwrap().is_empty());
        assert_eq!(
            store.due(schedule.next_run_at, 10).unwrap()[0].id,
            schedule.id
        );
        store
            .mark(schedule.id, schedule.next_run_at + 60, Some("boom"))
            .unwrap();
        let entries = store.list(Some("collector")).unwrap();
        assert_eq!(entries[0].last_error.as_deref(), Some("boom"));
        assert!(entries[0].last_run_at.is_some());
    }
}
