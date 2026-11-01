//! Small, explicit migration runner.
//!
//! `rusqlite_migration` was intentionally not used: the kernel needs two
//! independent migration streams (kernel and media) applied by the server, and
//! a tiny `sys_migrations` table gives that without an extra dependency.
//!
//! Every migration is applied in its own transaction and recorded as
//! `(scope, version)`. Versions are append-only and must increase.

use crate::db::Database;
use crate::error::{KernelError, Result};
use crate::time::now_secs;

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    pub name: &'static str,
    pub sql: &'static str,
}

/// Apply all migrations for `scope` that have not been applied yet.
pub fn apply(db: &Database, scope: &str, migrations: &[Migration]) -> Result<()> {
    db.with_write(|connection| {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS sys_migrations (
                 scope      TEXT NOT NULL,
                 version    INTEGER NOT NULL,
                 name       TEXT NOT NULL,
                 applied_at INTEGER NOT NULL,
                 PRIMARY KEY (scope, version)
             );",
        )?;

        let mut current: i64 = connection.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM sys_migrations WHERE scope = ?1",
            [scope],
            |row| row.get(0),
        )?;

        for migration in migrations {
            if migration.version <= current {
                continue;
            }
            if migration.version != current + 1 {
                return Err(KernelError::Config(format!(
                    "migration gap in scope '{scope}': expected {}, found {} ({})",
                    current + 1,
                    migration.version,
                    migration.name
                )));
            }
            let transaction = connection.transaction()?;
            transaction.execute_batch(migration.sql)?;
            transaction.execute(
                "INSERT INTO sys_migrations (scope, version, name, applied_at)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![scope, migration.version, migration.name, now_secs()],
            )?;
            transaction.commit()?;
            current = migration.version;
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn migrations() -> &'static [Migration] {
        &[Migration {
            version: 1,
            name: "test",
            sql: "CREATE TABLE test_table (id INTEGER PRIMARY KEY);",
        }]
    }

    #[test]
    fn applies_once_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("migrations.db");
        let db = Database::open(path.to_str().unwrap(), 1).unwrap();
        apply(&db, "test", migrations()).unwrap();
        apply(&db, "test", migrations()).unwrap();
        let count: i64 = db
            .with_read(|conn| {
                Ok(conn.query_row("SELECT COUNT(*) FROM sys_migrations", [], |row| row.get(0))?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }
}
