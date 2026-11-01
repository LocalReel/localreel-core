use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::Connection;

use crate::error::{KernelError, Result};

/// SQLite connection holder: exactly one writer, N read-only readers.
///
/// Reads go through `PRAGMA query_only=ON` connections. Writes are serialized,
/// which is the right trade-off for a local single-user app and keeps progress
/// updates from racing with scans.
#[derive(Clone)]
pub struct Database {
    inner: Arc<Inner>,
}

struct Inner {
    write: Mutex<Connection>,
    reads: Vec<Mutex<Connection>>,
    next_read: AtomicUsize,
    path: String,
}

impl Database {
    pub fn open(path: &str, read_connections: usize) -> Result<Self> {
        let in_memory = path == ":memory:";
        if !in_memory {
            if let Some(parent) = Path::new(path).parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|error| {
                        KernelError::Config(format!(
                            "failed to create database directory {}: {error}",
                            parent.display()
                        ))
                    })?;
                }
            }
        }

        let write = Connection::open(path)?;
        configure_connection(&write, false, in_memory)?;

        let mut reads = Vec::new();
        if !in_memory {
            for _ in 0..read_connections.max(1) {
                let conn = Connection::open(path)?;
                configure_connection(&conn, true, false)?;
                reads.push(Mutex::new(conn));
            }
        }

        Ok(Self {
            inner: Arc::new(Inner {
                write: Mutex::new(write),
                reads,
                next_read: AtomicUsize::new(0),
                path: path.to_string(),
            }),
        })
    }

    /// Run a write closure on the single writer connection.
    pub fn with_write<T>(&self, f: impl FnOnce(&mut Connection) -> Result<T>) -> Result<T> {
        let mut connection = self
            .inner
            .write
            .lock()
            .map_err(|_| KernelError::LockPoisoned("database writer".into()))?;
        f(&mut connection)
    }

    /// Run a read closure on a read-only connection (falls back to the writer
    /// for in-memory databases, where separate connections do not share state).
    pub fn with_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        if self.inner.reads.is_empty() {
            let mut connection = self
                .inner
                .write
                .lock()
                .map_err(|_| KernelError::LockPoisoned("database writer".into()))?;
            return f(&mut connection);
        }
        let index = self.inner.next_read.fetch_add(1, Ordering::Relaxed) % self.inner.reads.len();
        let connection = self.inner.reads[index]
            .lock()
            .map_err(|_| KernelError::LockPoisoned("database reader".into()))?;
        f(&connection)
    }

    /// Cheap liveness check used by `/api/v1/health`.
    pub fn health_check(&self) -> Result<()> {
        self.with_read(|connection| {
            let value: i64 = connection.query_row("SELECT 1", [], |row| row.get(0))?;
            if value != 1 {
                return Err(KernelError::Database(rusqlite::Error::InvalidQuery));
            }
            Ok(())
        })
    }

    pub fn path(&self) -> &str {
        &self.inner.path
    }
}

fn configure_connection(connection: &Connection, read_only: bool, in_memory: bool) -> Result<()> {
    connection.busy_timeout(Duration::from_secs(5))?;
    if in_memory {
        connection.execute_batch(
            "PRAGMA journal_mode = MEMORY;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;",
        )?;
    } else {
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA temp_store = MEMORY;
             PRAGMA foreign_keys = ON;",
        )?;
    }
    if read_only {
        connection.execute_batch("PRAGMA query_only = ON;")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_visible_to_readers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.db");
        let db = Database::open(path.to_str().unwrap(), 2).unwrap();
        db.with_write(|conn| {
            conn.execute_batch("CREATE TABLE t (v INTEGER); INSERT INTO t VALUES (7);")?;
            Ok(())
        })
        .unwrap();
        let value: i64 = db
            .with_read(|conn| Ok(conn.query_row("SELECT v FROM t", [], |row| row.get(0))?))
            .unwrap();
        assert_eq!(value, 7);
        db.health_check().unwrap();
    }
}
