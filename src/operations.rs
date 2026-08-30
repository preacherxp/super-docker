//! Persistent audit trail for user-triggered Docker operations.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};

const DEFAULT_HISTORY_LIMIT: usize = 500;

static DATABASE: OnceLock<Database> = OnceLock::new();

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationRow {
    pub id: i64,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub action: String,
    pub resource: String,
    pub target: String,
    pub target_id: String,
    pub status: String,
    pub error: String,
}

struct Database {
    connection: Mutex<Connection>,
    path: PathBuf,
}

#[derive(Debug)]
pub struct Error(String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// Open the persistent operation database once for this process.
pub fn init() -> Result<PathBuf, Error> {
    if let Some(db) = DATABASE.get() {
        return Ok(db.path.clone());
    }
    let path = database_path();
    let db = Database::open(&path)?;
    DATABASE
        .set(db)
        .map_err(|_| Error("operation database was initialized concurrently".into()))?;
    Ok(path)
}

/// Location can be overridden for scripts, tests, and portable installs.
pub fn database_path() -> PathBuf {
    if let Some(path) = std::env::var_os("SUPER_DOCKER_DB").filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    if let Some(base) = std::env::var_os("XDG_STATE_HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(base).join("super-docker/operations.sqlite3");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|p| !p.is_empty()) {
        return PathBuf::from(home).join(".local/state/super-docker/operations.sqlite3");
    }
    PathBuf::from(".super-docker-operations.sqlite3")
}

impl Database {
    fn open(path: &Path) -> Result<Self, Error> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error(format!("create {}: {e}", parent.display())))?;
        }
        let connection =
            Connection::open(path).map_err(|e| Error(format!("open {}: {e}", path.display())))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA busy_timeout = 5000;
                 CREATE TABLE IF NOT EXISTS operations (
                     id          INTEGER PRIMARY KEY AUTOINCREMENT,
                     started_at  INTEGER NOT NULL,
                     finished_at INTEGER,
                     action      TEXT NOT NULL,
                     resource    TEXT NOT NULL,
                     target      TEXT NOT NULL,
                     target_id   TEXT NOT NULL DEFAULT '',
                     status      TEXT NOT NULL CHECK(status IN ('running', 'succeeded', 'failed', 'interrupted')),
                     error       TEXT NOT NULL DEFAULT ''
                 );
                 CREATE INDEX IF NOT EXISTS operations_started_at
                     ON operations(started_at DESC, id DESC);
                 UPDATE operations
                    SET status = 'interrupted',
                        finished_at = unixepoch(),
                        error = CASE WHEN error = '' THEN 'application exited before completion' ELSE error END
                  WHERE status = 'running';",
            )
            .map_err(|e| Error(format!("initialize {}: {e}", path.display())))?;
        Ok(Self {
            connection: Mutex::new(connection),
            path: path.to_path_buf(),
        })
    }

    fn begin(
        &self,
        action: &str,
        resource: &str,
        target: &str,
        target_id: &str,
    ) -> rusqlite::Result<i64> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "INSERT INTO operations
                (started_at, action, resource, target, target_id, status)
             VALUES (?1, ?2, ?3, ?4, ?5, 'running')",
            params![unix_now(), action, resource, target, target_id],
        )?;
        Ok(connection.last_insert_rowid())
    }

    fn finish(&self, id: i64, error: Option<&str>) -> rusqlite::Result<()> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        connection.execute(
            "UPDATE operations
                SET finished_at = ?1, status = ?2, error = ?3
              WHERE id = ?4",
            params![
                unix_now(),
                if error.is_some() {
                    "failed"
                } else {
                    "succeeded"
                },
                error.unwrap_or_default(),
                id
            ],
        )?;
        Ok(())
    }

    fn recent(&self, limit: usize) -> rusqlite::Result<Vec<OperationRow>> {
        let connection = self.connection.lock().unwrap_or_else(|e| e.into_inner());
        let mut statement = connection.prepare(
            "SELECT id, started_at, finished_at, action, resource, target,
                    target_id, status, error
               FROM operations
              ORDER BY id DESC
              LIMIT ?1",
        )?;
        let rows = statement.query_map([limit.min(i64::MAX as usize) as i64], |row| {
            Ok(OperationRow {
                id: row.get(0)?,
                started_at: row.get(1)?,
                finished_at: row.get(2)?,
                action: row.get(3)?,
                resource: row.get(4)?,
                target: row.get(5)?,
                target_id: row.get(6)?,
                status: row.get(7)?,
                error: row.get(8)?,
            })
        })?;
        rows.collect()
    }
}

/// Record one operation and return a handle that marks its final outcome.
/// Logging failures never prevent the requested Docker action from running.
pub fn begin(action: &str, resource: &str, target: &str, target_id: &str) -> PendingOperation {
    // Action callers run on worker threads.  Lazily opening the database here
    // keeps SQLite directory creation, migration, and recovery off the UI
    // thread while still recording the very first operation.
    if DATABASE.get().is_none() {
        let _ = init();
    }
    let id = DATABASE
        .get()
        .and_then(|db| db.begin(action, resource, target, target_id).ok());
    PendingOperation { id }
}

pub struct PendingOperation {
    id: Option<i64>,
}

impl PendingOperation {
    pub fn finish<T, E: fmt::Display>(&self, result: &Result<T, E>) {
        let Some(id) = self.id else { return };
        let error = result.as_ref().err().map(ToString::to_string);
        if let Some(db) = DATABASE.get() {
            let _ = db.finish(id, error.as_deref());
        }
    }
}

pub fn recent(limit: usize) -> Vec<OperationRow> {
    DATABASE
        .get()
        .and_then(|db| db.recent(limit).ok())
        .unwrap_or_default()
}

pub fn print_history() {
    let rows = recent(DEFAULT_HISTORY_LIMIT);
    println!("database: {}", database_path().display());
    println!(
        "{:<8}  {:<11}  {:<10}  {:<12}  target",
        "started", "status", "resource", "operation"
    );
    for row in rows {
        let detail = if row.error.is_empty() {
            String::new()
        } else {
            format!(" — {}", row.error.replace('\n', " "))
        };
        println!(
            "{:<8}  {:<11}  {:<10}  {:<12}  {}{}",
            crate::app::ago(row.started_at),
            row.status,
            row.resource,
            row.action,
            row.target,
            detail
        );
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(label: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        std::env::temp_dir().join(format!(
            "super-docker-{label}-{}-{}.sqlite3",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ))
    }

    fn cleanup(path: &Path) {
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(path.with_extension("sqlite3-wal"));
        let _ = std::fs::remove_file(path.with_extension("sqlite3-shm"));
    }

    #[test]
    fn persists_successes_and_failures() {
        let path = test_path("operations");
        let db = Database::open(&path).unwrap();
        let ok = db.begin("start", "container", "web", "abc").unwrap();
        db.finish(ok, None).unwrap();
        let failed = db.begin("remove", "volume", "data", "").unwrap();
        db.finish(failed, Some("volume is in use")).unwrap();

        drop(db);
        let db = Database::open(&path).unwrap();
        let rows = db.recent(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[0].error, "volume is in use");
        assert_eq!(rows[1].status, "succeeded");
        assert!(rows[1].finished_at.is_some());

        drop(db);
        cleanup(&path);
    }

    #[test]
    fn reopening_marks_unfinished_rows_interrupted() {
        let path = test_path("interrupted");
        let db = Database::open(&path).unwrap();
        db.begin("restart", "container", "api", "abc").unwrap();
        drop(db);

        let db = Database::open(&path).unwrap();
        let row = db.recent(1).unwrap().pop().unwrap();
        assert_eq!(row.status, "interrupted");
        assert_eq!(row.error, "application exited before completion");
        assert!(row.finished_at.is_some());
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn recent_is_newest_first_and_respects_zero_and_finite_limits() {
        let path = test_path("limits");
        let db = Database::open(&path).unwrap();
        for target in ["one", "two", "three"] {
            db.begin("start", "container", target, target).unwrap();
        }
        assert!(db.recent(0).unwrap().is_empty());
        let rows = db.recent(2).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].target, "three");
        assert_eq!(rows[1].target, "two");
        drop(db);
        cleanup(&path);
    }

    #[test]
    fn corrupt_database_returns_contextual_error() {
        let path = test_path("corrupt");
        std::fs::write(&path, b"not sqlite").unwrap();
        let error = Database::open(&path).err().unwrap();
        assert!(error.to_string().contains("initialize"));
        cleanup(&path);
    }
}
