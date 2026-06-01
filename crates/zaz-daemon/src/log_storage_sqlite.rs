//! SQLite-backed log storage skeleton.
//!
//! This module owns the persistent backend's open / pragma / migrate
//! lifecycle. Write, query, retention, and trait wiring land in the
//! follow-up milestones; this file's job is to make sure the database
//! is created in WAL mode with `synchronous = NORMAL`, dispatches
//! `PRAGMA user_version` migrations forward, and refuses to touch a
//! database produced by a newer zaz.

// The constructor and helpers are reachable only from the inline tests
// until the daemon startup wiring lands; the suppression peels off then.
#![allow(dead_code)]

use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::error::LogStorageError;

/// Latest schema version this build understands.
pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// Busy-timeout applied to every connection. Tolerates short read
/// contention from the API path while keeping shutdown responsive.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// SQLite-backed log storage.
///
/// The trait implementation and the read/write methods come with
/// milestones 27.2 and 27.3; this skeleton only owns the lifecycle.
pub struct SqliteLogStorage {
    conn: Connection,
}

impl SqliteLogStorage {
    /// Open or create a SQLite log database at `path`.
    ///
    /// Creates the parent directory if it does not yet exist, applies
    /// the WAL / `synchronous = NORMAL` / busy-timeout pragmas, and
    /// dispatches forward `user_version` migrations.
    pub fn open(path: &Path) -> Result<Self, LogStorageError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    LogStorageError::Open(format!(
                        "create parent {}: {e}",
                        parent.display()
                    ))
                })?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|e| LogStorageError::Open(format!("open {}: {e}", path.display())))?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory SQLite database with the same lifecycle as
    /// [`open`](Self::open). Useful for tests; WAL does not stick on
    /// `:memory:` so the journal-mode check tolerates `memory` here.
    pub fn open_in_memory() -> Result<Self, LogStorageError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| LogStorageError::Open(format!("open in-memory: {e}")))?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self { conn })
    }
}

/// Apply the daemon-standard SQLite pragmas to `conn`.
fn apply_pragmas(conn: &Connection) -> Result<(), LogStorageError> {
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| LogStorageError::Open(format!("pragma journal_mode: {e}")))?;
    let mode: String = conn
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .map_err(|e| LogStorageError::Open(format!("read journal_mode: {e}")))?;
    let normalized = mode.to_ascii_lowercase();
    if normalized != "wal" && normalized != "memory" {
        return Err(LogStorageError::Open(format!(
            "journal_mode did not switch to wal, got {mode}"
        )));
    }

    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| LogStorageError::Open(format!("pragma synchronous: {e}")))?;

    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| LogStorageError::Open(format!("busy_timeout: {e}")))?;

    Ok(())
}

/// Dispatch forward migrations against `PRAGMA user_version`.
///
/// A version above the build's `CURRENT_SCHEMA_VERSION` is rejected so a
/// downgrade does not silently truncate or rewrite history.
fn migrate(conn: &Connection) -> Result<(), LogStorageError> {
    let current: i32 = conn
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|e| LogStorageError::SchemaInit(format!("read user_version: {e}")))?;

    if current > CURRENT_SCHEMA_VERSION {
        return Err(LogStorageError::SchemaInit(format!(
            "database created by a newer zaz (schema version {current}, this build understands up to {CURRENT_SCHEMA_VERSION})"
        )));
    }

    for next in (current + 1)..=CURRENT_SCHEMA_VERSION {
        match next {
            1 => migrate_to_v1(conn)?,
            other => {
                return Err(LogStorageError::SchemaInit(format!(
                    "missing migration for schema version {other}"
                )));
            }
        }
    }

    Ok(())
}

/// Initial schema. Creates `log_entries` plus the three indexes the
/// query patterns named in ZAZ-012 will rely on, then stamps
/// `user_version = 1`.
fn migrate_to_v1(conn: &Connection) -> Result<(), LogStorageError> {
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE log_entries (
             id INTEGER PRIMARY KEY AUTOINCREMENT,
             timestamp_ms INTEGER NOT NULL,
             process TEXT NOT NULL,
             group_name TEXT,
             source TEXT NOT NULL,
             output_kind TEXT NOT NULL,
             content TEXT NOT NULL
         );
         CREATE INDEX log_entries_process_id_idx ON log_entries(process, id);
         CREATE INDEX log_entries_time_id_idx ON log_entries(timestamp_ms, id);
         CREATE INDEX log_entries_group_id_idx ON log_entries(group_name, id);
         PRAGMA user_version = 1;
         COMMIT;",
    )
    .map_err(|e| LogStorageError::SchemaInit(format!("apply v1 schema: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn user_version(conn: &Connection) -> i32 {
        conn.pragma_query_value(None, "user_version", |row| row.get(0))
            .expect("read user_version")
    }

    fn journal_mode(conn: &Connection) -> String {
        conn.pragma_query_value(None, "journal_mode", |row| row.get(0))
            .expect("read journal_mode")
    }

    fn synchronous(conn: &Connection) -> i32 {
        conn.pragma_query_value(None, "synchronous", |row| row.get(0))
            .expect("read synchronous")
    }

    fn count_schema_objects(conn: &Connection, kind: &str, name: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
            (kind, name),
            |row| row.get(0),
        )
        .expect("query sqlite_master")
    }

    #[test]
    fn open_initializes_schema_at_current_version() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("logs.sqlite3");

        let storage = SqliteLogStorage::open(&path).expect("open");

        assert_eq!(user_version(&storage.conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(journal_mode(&storage.conn).to_ascii_lowercase(), "wal");
        // synchronous = NORMAL is the integer 1 in SQLite.
        assert_eq!(synchronous(&storage.conn), 1);
        assert_eq!(count_schema_objects(&storage.conn, "table", "log_entries"), 1);
        assert_eq!(
            count_schema_objects(&storage.conn, "index", "log_entries_process_id_idx"),
            1
        );
        assert_eq!(
            count_schema_objects(&storage.conn, "index", "log_entries_time_id_idx"),
            1
        );
        assert_eq!(
            count_schema_objects(&storage.conn, "index", "log_entries_group_id_idx"),
            1
        );
    }

    #[test]
    fn open_is_idempotent() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("logs.sqlite3");

        let first = SqliteLogStorage::open(&path).expect("open first");
        // Seed a row so we can confirm re-open does not drop or rewrite.
        first
            .conn
            .execute(
                "INSERT INTO log_entries
                    (timestamp_ms, process, group_name, source, output_kind, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (1_000_i64, "task1", Option::<&str>::None, "process", "combined", "hello"),
            )
            .expect("insert seed row");
        drop(first);

        let second = SqliteLogStorage::open(&path).expect("open second");
        assert_eq!(user_version(&second.conn), CURRENT_SCHEMA_VERSION);
        let row_count: i64 = second
            .conn
            .query_row("SELECT COUNT(*) FROM log_entries", (), |row| row.get(0))
            .expect("count rows");
        assert_eq!(row_count, 1);
    }

    #[test]
    fn migrate_rejects_future_user_version() {
        let conn = Connection::open_in_memory().expect("in-memory conn");
        conn.pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 5)
            .expect("seed future version");

        let err = migrate(&conn).expect_err("expected future-version rejection");
        match err {
            LogStorageError::SchemaInit(msg) => {
                assert!(msg.contains("newer zaz"), "message was: {msg}");
            }
            other => panic!("expected SchemaInit, got {other:?}"),
        }
    }

    #[test]
    fn migrate_runs_from_version_zero() {
        let conn = Connection::open_in_memory().expect("in-memory conn");
        assert_eq!(user_version(&conn), 0);

        migrate(&conn).expect("migrate");

        assert_eq!(user_version(&conn), CURRENT_SCHEMA_VERSION);
        assert_eq!(count_schema_objects(&conn, "table", "log_entries"), 1);
    }

    #[test]
    fn open_in_memory_succeeds_without_wal() {
        let storage = SqliteLogStorage::open_in_memory().expect("open in-memory");
        // :memory: silently keeps journal_mode = memory; the soft-accept
        // path in apply_pragmas should not have errored out.
        assert_eq!(journal_mode(&storage.conn).to_ascii_lowercase(), "memory");
        assert_eq!(user_version(&storage.conn), CURRENT_SCHEMA_VERSION);
    }
}
