//! SQLite-backed log storage.
//!
//! This module owns the persistent backend's open / pragma / migrate
//! lifecycle plus the data-plane operations the daemon's API surface
//! already speaks: batched inserts as one SQLite transaction, paged
//! queries that mirror the in-memory store's filter and pagination
//! semantics, aggregate stats, clear / clear-process, and a `flush_now`
//! that issues `PRAGMA wal_checkpoint(TRUNCATE)` so cold open stays
//! fast and the `.sqlite3` file is copy-safe after a clean shutdown.
//!
//! The hybrid hot-buffer / SQLite runtime composes this backend with
//! the in-memory [`LogStore`]; see [`db_path_for_config`] for the
//! XDG-state path the daemon resolves at startup.
//!
//! [`LogStorage`]: crate::log_storage::LogStorage
//! [`LogStore`]: crate::log_store::LogStore

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::api::{LogLine, LogSource, OutputKind};
use crate::error::LogStorageError;
use crate::log_storage::{LogQuery, LogQueryResult, LogStorage, LogStorageStats};

/// Latest schema version this build understands.
pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// Busy-timeout applied to every connection. Tolerates short read
/// contention from the API path while keeping shutdown responsive.
const BUSY_TIMEOUT: Duration = Duration::from_millis(5_000);

/// Maximum rows deleted per pass when pruning for the size budget.
/// The size sweep loops until the logical data size is under the
/// configured limit; chunking keeps any single statement's lock
/// window bounded so concurrent reads stay responsive.
const RETENTION_CHUNK: i64 = 1_000;

/// Persistent retention limits applied by `enforce_retention`.
///
/// Both limits are enforced together: the per-process sweep deletes
/// oldest rows for any process whose persisted row count exceeds
/// `max_lines_per_process`, then the size sweep deletes oldest rows
/// globally until the logical data size (excluding freelist pages)
/// is under `max_size_bytes`. The sweep never issues `VACUUM` or
/// `wal_checkpoint`, so the on-disk file settles near the budget
/// while subsequent inserts reuse freed pages.
#[derive(Debug, Clone, Copy)]
pub struct RetentionPolicy {
    /// Maximum logical data bytes the persisted log set may occupy.
    /// Computed as `(page_count - freelist_count) * page_size`.
    pub max_size_bytes: usize,
    /// Maximum rows retained per process.
    pub max_lines_per_process: usize,
}

/// SQLite-backed log storage.
///
/// Owns the open / migrate lifecycle, the data-plane methods that mirror the
/// in-memory store's filter and pagination semantics, and the shutdown
/// checkpoint. The hybrid runtime in [`crate::log_store::LogStore`] composes
/// this backend with the in-memory hot buffer.
pub struct SqliteLogStorage {
    conn: Connection,
    policy: Option<RetentionPolicy>,
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
                    LogStorageError::Open(format!("create parent {}: {e}", parent.display()))
                })?;
            }
        }
        let conn = Connection::open(path)
            .map_err(|e| LogStorageError::Open(format!("open {}: {e}", path.display())))?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            policy: None,
        })
    }

    /// Open an in-memory SQLite database with the same lifecycle as
    /// [`open`](Self::open). Useful for tests; WAL does not stick on
    /// `:memory:` so the journal-mode check tolerates `memory` here.
    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self, LogStorageError> {
        let conn = Connection::open_in_memory()
            .map_err(|e| LogStorageError::Open(format!("open in-memory: {e}")))?;
        apply_pragmas(&conn)?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            policy: None,
        })
    }

    /// Attach a retention policy. Without a policy, `enforce_retention`
    /// is a no-op so the SQLite backend keeps the original "seam wired,
    /// no behavior" contract from phase 27.
    pub fn with_retention(mut self, policy: RetentionPolicy) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Insert a batch of log lines inside a single SQLite transaction.
    ///
    /// Empty batches return without touching the database. Any error
    /// during prepare, execute, or commit rolls the transaction back
    /// atomically; partial writes never leak.
    pub fn push_batch(&mut self, logs: Vec<LogLine>) -> Result<(), LogStorageError> {
        if logs.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction()
            .map_err(|e| LogStorageError::Write(format!("begin transaction: {e}")))?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT INTO log_entries
                        (timestamp_ms, process, group_name, source, output_kind, content)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                )
                .map_err(|e| LogStorageError::Write(format!("prepare insert: {e}")))?;
            for log in &logs {
                stmt.execute(rusqlite::params![
                    log.timestamp as i64,
                    log.process,
                    log.group,
                    source_to_str(log.source),
                    output_kind_to_str(log.output_kind),
                    log.content,
                ])
                .map_err(|e| LogStorageError::Write(format!("insert log: {e}")))?;
            }
        }
        tx.commit()
            .map_err(|e| LogStorageError::Write(format!("commit transaction: {e}")))?;
        Ok(())
    }

    /// Run a query against the persisted log set.
    ///
    /// Filter ordering matches the in-memory store: process, then
    /// group, then since / until, then case-insensitive substring
    /// search on `content`. Pagination is offset / limit with the same
    /// `has_more = offset + page.len() < total_count` contract.
    pub fn query(&self, query: LogQuery) -> Result<LogQueryResult, LogStorageError> {
        let (where_clause, params) = build_where(&query);

        let count_sql = format!("SELECT COUNT(*) FROM log_entries {where_clause}");
        let total_count: i64 = self
            .conn
            .query_row(
                &count_sql,
                rusqlite::params_from_iter(params.iter()),
                |row| row.get(0),
            )
            .map_err(|e| LogStorageError::Query(format!("count rows: {e}")))?;
        let total_count = total_count.max(0) as usize;

        let offset = query.offset.unwrap_or(0);
        let limit_value: i64 = match query.limit {
            Some(n) => n as i64,
            None => -1,
        };

        let select_sql = format!(
            "SELECT timestamp_ms, process, group_name, source, output_kind, content
             FROM log_entries {where_clause}
             ORDER BY timestamp_ms ASC, id ASC
             LIMIT ?{limit_idx} OFFSET ?{offset_idx}",
            limit_idx = params.len() + 1,
            offset_idx = params.len() + 2,
        );

        let mut all_params: Vec<Value> = params;
        all_params.push(Value::Integer(limit_value));
        all_params.push(Value::Integer(offset as i64));

        let mut stmt = self
            .conn
            .prepare(&select_sql)
            .map_err(|e| LogStorageError::Query(format!("prepare select: {e}")))?;
        let rows = stmt
            .query_map(
                rusqlite::params_from_iter(all_params.iter()),
                row_to_log_line,
            )
            .map_err(|e| LogStorageError::Query(format!("execute select: {e}")))?;
        let mut logs = Vec::new();
        for row in rows {
            logs.push(row.map_err(|e| LogStorageError::Query(format!("decode row: {e}")))?);
        }

        let has_more = offset + logs.len() < total_count;
        Ok(LogQueryResult {
            logs,
            total_count,
            has_more,
            offset,
        })
    }

    /// Aggregate stats over the persisted log set.
    ///
    /// Hot-buffer concepts (`memory_bytes`, `memory_limit`,
    /// `max_lines_per_process`) are returned as their zero / `None`
    /// defaults here; the hybrid wrapper that combines this backend
    /// with the in-memory hot buffer is the right place to populate
    /// them.
    pub fn stats(&self) -> Result<LogStorageStats, LogStorageError> {
        let (total_lines, process_count, oldest, newest): (i64, i64, Option<i64>, Option<i64>) =
            self.conn
                .query_row(
                    "SELECT COUNT(*),
                        COUNT(DISTINCT process),
                        MIN(timestamp_ms),
                        MAX(timestamp_ms)
                 FROM log_entries",
                    (),
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .map_err(|e| LogStorageError::Query(format!("aggregate stats: {e}")))?;

        Ok(LogStorageStats {
            total_lines: total_lines.max(0) as usize,
            process_count: process_count.max(0) as usize,
            memory_bytes: 0,
            oldest_timestamp: oldest.map(|t| t as u64),
            newest_timestamp: newest.map(|t| t as u64),
            memory_limit: None,
            max_lines_per_process: 0,
        })
    }

    /// Remove every row from `log_entries`.
    pub fn clear(&mut self) -> Result<(), LogStorageError> {
        self.conn
            .execute("DELETE FROM log_entries", ())
            .map_err(|e| LogStorageError::Write(format!("clear log_entries: {e}")))?;
        Ok(())
    }

    /// Remove every row for `name` from `log_entries`.
    pub fn clear_process(&mut self, name: &str) -> Result<(), LogStorageError> {
        self.conn
            .execute("DELETE FROM log_entries WHERE process = ?1", (name,))
            .map_err(|e| LogStorageError::Write(format!("clear process {name}: {e}")))?;
        Ok(())
    }

    /// Shutdown hook. Issues `PRAGMA wal_checkpoint(TRUNCATE)` so the
    /// WAL sibling shrinks to zero before exit, keeping cold open fast
    /// and the `.sqlite3` file copy-safe without `-wal` / `-shm`
    /// siblings dangling.
    ///
    /// In-memory databases keep `journal_mode = memory`, where
    /// checkpointing is meaningless; this call is a no-op there.
    pub fn flush_now(&mut self) -> Result<(), LogStorageError> {
        let mode: String = self
            .conn
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(|e| LogStorageError::Write(format!("read journal_mode: {e}")))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Ok(());
        }
        self.conn
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", (), |_| Ok(()))
            .map_err(|e| LogStorageError::Write(format!("wal_checkpoint: {e}")))?;
        Ok(())
    }

    /// Run the persistent retention sweep.
    ///
    /// With no policy attached, this returns `Ok(())` without touching
    /// the database. With a policy attached, two passes run:
    ///
    /// 1. **Per-process pass.** For each process whose row count exceeds
    ///    `max_lines_per_process`, the oldest `count - max` rows are
    ///    deleted by ascending `id` (which equals insertion order).
    /// 2. **Size pass.** While the logical data size
    ///    `(page_count - freelist_count) * page_size` exceeds
    ///    `max_size_bytes`, the oldest [`RETENTION_CHUNK`] rows are
    ///    deleted in a loop. The loop bails out when no rows remain
    ///    or when a pass deletes nothing, so a degenerate budget can
    ///    never spin forever.
    ///
    /// No `VACUUM` or `wal_checkpoint` runs here; freed pages stay on
    /// the freelist and get reused by subsequent inserts.
    pub fn enforce_retention(&mut self) -> Result<(), LogStorageError> {
        let Some(policy) = self.policy else {
            return Ok(());
        };

        self.prune_per_process(policy.max_lines_per_process)?;
        self.prune_to_size(policy.max_size_bytes)?;
        Ok(())
    }

    fn prune_per_process(&self, max: usize) -> Result<(), LogStorageError> {
        let max_i64 = max as i64;
        let over: Vec<(String, i64)> = {
            let mut stmt = self
                .conn
                .prepare(
                    "SELECT process, COUNT(*) AS row_count
                     FROM log_entries
                     GROUP BY process
                     HAVING row_count > ?1",
                )
                .map_err(|e| {
                    LogStorageError::Retention(format!("prepare per-process scan: {e}"))
                })?;
            let rows = stmt
                .query_map(rusqlite::params![max_i64], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })
                .map_err(|e| {
                    LogStorageError::Retention(format!("execute per-process scan: {e}"))
                })?;
            let mut out = Vec::new();
            for row in rows {
                out.push(
                    row.map_err(|e| {
                        LogStorageError::Retention(format!("decode per-process row: {e}"))
                    })?,
                );
            }
            out
        };

        for (process, count) in over {
            let excess = count - max_i64;
            if excess <= 0 {
                continue;
            }
            self.conn
                .execute(
                    "DELETE FROM log_entries
                     WHERE id IN (
                         SELECT id FROM log_entries
                         WHERE process = ?1
                         ORDER BY id ASC
                         LIMIT ?2
                     )",
                    rusqlite::params![process, excess],
                )
                .map_err(|e| {
                    LogStorageError::Retention(format!("delete excess for {process}: {e}"))
                })?;
        }
        Ok(())
    }

    fn prune_to_size(&self, max_bytes: usize) -> Result<(), LogStorageError> {
        let page_size = self.read_pragma_int("page_size")?;
        if page_size <= 0 {
            return Ok(());
        }

        loop {
            let logical = self.logical_data_bytes(page_size)?;
            if logical <= max_bytes as i64 {
                return Ok(());
            }

            let deleted = self
                .conn
                .execute(
                    "DELETE FROM log_entries
                     WHERE id IN (
                         SELECT id FROM log_entries
                         ORDER BY id ASC
                         LIMIT ?1
                     )",
                    rusqlite::params![RETENTION_CHUNK],
                )
                .map_err(|e| LogStorageError::Retention(format!("delete oldest chunk: {e}")))?;

            if deleted == 0 {
                return Ok(());
            }
        }
    }

    fn logical_data_bytes(&self, page_size: i64) -> Result<i64, LogStorageError> {
        let page_count = self.read_pragma_int("page_count")?;
        let freelist = self.read_pragma_int("freelist_count")?;
        let used_pages = (page_count - freelist).max(0);
        Ok(used_pages.saturating_mul(page_size))
    }

    fn read_pragma_int(&self, name: &str) -> Result<i64, LogStorageError> {
        self.conn
            .pragma_query_value(None, name, |row| row.get::<_, i64>(0))
            .map_err(|e| LogStorageError::Retention(format!("read pragma {name}: {e}")))
    }

    /// Read up to `limit` rows for `name`, ordered ascending by
    /// `(timestamp_ms, id)`. `name == "*"` returns rows across every
    /// process. Matches the in-memory store's "most recent `limit`
    /// rows, in ascending order" contract — implemented as a DESC
    /// inner query that takes the last N rows, then re-ordered ASC.
    fn get_internal(
        &self,
        name: &str,
        limit: Option<usize>,
    ) -> Result<Vec<LogLine>, LogStorageError> {
        let limit_value: i64 = match limit {
            Some(n) => n as i64,
            None => -1,
        };

        let collected = if name == "*" {
            let sql = "SELECT timestamp_ms, process, group_name, source, output_kind, content
                       FROM (
                           SELECT id, timestamp_ms, process, group_name, source, output_kind, content
                           FROM log_entries
                           ORDER BY timestamp_ms DESC, id DESC
                           LIMIT ?1
                       )
                       ORDER BY timestamp_ms ASC, id ASC";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| LogStorageError::Query(format!("prepare get: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![limit_value], row_to_log_line)
                .map_err(|e| LogStorageError::Query(format!("execute get: {e}")))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| LogStorageError::Query(format!("decode row: {e}")))?);
            }
            out
        } else {
            let sql = "SELECT timestamp_ms, process, group_name, source, output_kind, content
                       FROM (
                           SELECT id, timestamp_ms, process, group_name, source, output_kind, content
                           FROM log_entries
                           WHERE process = ?1
                           ORDER BY timestamp_ms DESC, id DESC
                           LIMIT ?2
                       )
                       ORDER BY timestamp_ms ASC, id ASC";
            let mut stmt = self
                .conn
                .prepare(sql)
                .map_err(|e| LogStorageError::Query(format!("prepare get: {e}")))?;
            let rows = stmt
                .query_map(rusqlite::params![name, limit_value], row_to_log_line)
                .map_err(|e| LogStorageError::Query(format!("execute get: {e}")))?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.map_err(|e| LogStorageError::Query(format!("decode row: {e}")))?);
            }
            out
        };

        Ok(collected)
    }
}

impl LogStorage for SqliteLogStorage {
    fn push(&mut self, log: LogLine) -> Result<(), LogStorageError> {
        self.push_batch(vec![log])
    }

    fn push_batch(&mut self, logs: Vec<LogLine>) -> Result<(), LogStorageError> {
        SqliteLogStorage::push_batch(self, logs)
    }

    fn get(&self, name: &str, limit: Option<usize>) -> Result<Vec<LogLine>, LogStorageError> {
        self.get_internal(name, limit)
    }

    fn query(&self, query: LogQuery) -> Result<LogQueryResult, LogStorageError> {
        SqliteLogStorage::query(self, query)
    }

    fn stats(&self) -> Result<LogStorageStats, LogStorageError> {
        SqliteLogStorage::stats(self)
    }

    fn clear(&mut self) -> Result<(), LogStorageError> {
        SqliteLogStorage::clear(self)
    }

    fn clear_process(&mut self, name: &str) -> Result<(), LogStorageError> {
        SqliteLogStorage::clear_process(self, name)
    }

    fn flush(&mut self) -> Result<(), LogStorageError> {
        // The inherent `flush_now` is the meaningful durability boundary;
        // between-batch flush is a no-op against WAL-mode SQLite.
        Ok(())
    }

    fn flush_now(&mut self) -> Result<(), LogStorageError> {
        SqliteLogStorage::flush_now(self)
    }

    fn enforce_retention(&mut self) -> Result<(), LogStorageError> {
        SqliteLogStorage::enforce_retention(self)
    }

    fn memory_limit(&self) -> Option<usize> {
        // Hot-buffer concept; the hybrid runtime answers it from the
        // in-memory side. SQLite has no equivalent.
        None
    }

    fn set_memory_limit(&mut self, _bytes: usize) -> Option<usize> {
        None
    }

    fn max_lines_per_process(&self) -> usize {
        0
    }

    fn set_max_lines_per_process(&mut self, _max: usize) -> usize {
        0
    }
}

/// Resolve the persistent log database path for `config_path`.
///
/// Always lands under `$XDG_STATE_HOME/zaz/logs/<hash>.sqlite3`, falling
/// back to `~/.local/state/zaz/logs/<hash>.sqlite3`. The proposal pins
/// this layout against the existing daemon debug-log precedent and
/// explicitly does *not* fork into `<project>/.zaz/` even when the
/// socket does, so SQLite WAL / SHM siblings cannot leak into project
/// trees or network mounts.
///
/// Identity is keyed by the canonicalized config path so unrelated
/// projects do not share log history. The directory tree is created
/// with `0o700` permissions on first use.
pub fn db_path_for_config(config_path: &Path) -> PathBuf {
    db_path_in_base(&state_dir_base(), config_path)
}

#[cfg(test)]
impl SqliteLogStorage {
    /// Test-only access to the underlying connection so sibling-module
    /// tests can install poison triggers or read pragmas directly.
    pub(crate) fn conn_for_test(&self) -> &Connection {
        &self.conn
    }
}

fn state_dir_base() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".local/state");
    }
    PathBuf::from(".")
}

fn db_path_in_base(base: &Path, config_path: &Path) -> PathBuf {
    let canonical = config_path
        .canonicalize()
        .unwrap_or_else(|_| config_path.to_path_buf());
    let hash = {
        let mut hasher = DefaultHasher::new();
        canonical.hash(&mut hasher);
        hasher.finish()
    };
    let dir = base.join("zaz").join("logs");
    if !dir.exists() {
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    dir.join(format!("{:016x}.sqlite3", hash))
}

fn source_to_str(s: LogSource) -> &'static str {
    match s {
        LogSource::Process => "process",
        LogSource::Daemon => "daemon",
    }
}

fn output_kind_to_str(k: OutputKind) -> &'static str {
    match k {
        OutputKind::Stdout => "stdout",
        OutputKind::Stderr => "stderr",
        OutputKind::Combined => "combined",
    }
}

fn parse_source(s: &str) -> Result<LogSource, String> {
    match s {
        "process" => Ok(LogSource::Process),
        "daemon" => Ok(LogSource::Daemon),
        other => Err(format!("unknown log source: {other}")),
    }
}

fn parse_output_kind(s: &str) -> Result<OutputKind, String> {
    match s {
        "stdout" => Ok(OutputKind::Stdout),
        "stderr" => Ok(OutputKind::Stderr),
        "combined" => Ok(OutputKind::Combined),
        other => Err(format!("unknown output kind: {other}")),
    }
}

fn row_to_log_line(row: &rusqlite::Row<'_>) -> rusqlite::Result<LogLine> {
    let timestamp_ms: i64 = row.get(0)?;
    let process: String = row.get(1)?;
    let group: Option<String> = row.get(2)?;
    let source_str: String = row.get(3)?;
    let output_kind_str: String = row.get(4)?;
    let content: String = row.get(5)?;

    let source = parse_source(&source_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::<dyn std::error::Error + Send + Sync>::from(e),
        )
    })?;
    let output_kind = parse_output_kind(&output_kind_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::<dyn std::error::Error + Send + Sync>::from(e),
        )
    })?;

    Ok(LogLine {
        timestamp: timestamp_ms as u64,
        process,
        group,
        content,
        source,
        output_kind,
    })
}

/// Build the shared WHERE clause and positional parameter list used by
/// both the COUNT and the paginated SELECT.
fn build_where(query: &LogQuery) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut params: Vec<Value> = Vec::new();

    if !query.is_all_processes() {
        if let Some(name) = &query.process {
            clauses.push(format!("process = ?{}", params.len() + 1));
            params.push(Value::Text(name.clone()));
        }
    }
    if let Some(group) = &query.group {
        clauses.push(format!("group_name = ?{}", params.len() + 1));
        params.push(Value::Text(group.clone()));
    }
    if let Some(since) = query.since {
        clauses.push(format!("timestamp_ms >= ?{}", params.len() + 1));
        params.push(Value::Integer(since as i64));
    }
    if let Some(until) = query.until {
        clauses.push(format!("timestamp_ms <= ?{}", params.len() + 1));
        params.push(Value::Integer(until as i64));
    }
    if let Some(search) = &query.search {
        clauses.push(format!(
            "instr(LOWER(content), LOWER(?{})) > 0",
            params.len() + 1
        ));
        params.push(Value::Text(search.clone()));
    }

    let where_clause = if clauses.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", clauses.join(" AND "))
    };
    (where_clause, params)
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
        assert_eq!(
            count_schema_objects(&storage.conn, "table", "log_entries"),
            1
        );
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
                (
                    1_000_i64,
                    "task1",
                    Option::<&str>::None,
                    "process",
                    "combined",
                    "hello",
                ),
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

    fn line(timestamp: u64, process: &str, group: Option<&str>, content: &str) -> LogLine {
        LogLine {
            timestamp,
            process: process.to_string(),
            group: group.map(|g| g.to_string()),
            content: content.to_string(),
            source: LogSource::Process,
            output_kind: OutputKind::Combined,
        }
    }

    fn row_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM log_entries", (), |row| row.get(0))
            .expect("count rows")
    }

    /// Install a BEFORE INSERT trigger that aborts when content matches
    /// the sentinel, so the atomicity test can force a mid-batch failure
    /// without monkey-patching the schema.
    fn install_poison_trigger(conn: &Connection, sentinel: &str) {
        conn.execute(
            &format!(
                "CREATE TRIGGER abort_on_poison
                 BEFORE INSERT ON log_entries
                 FOR EACH ROW WHEN NEW.content = '{sentinel}'
                 BEGIN
                     SELECT RAISE(ABORT, 'poison row rejected');
                 END",
            ),
            (),
        )
        .expect("install trigger");
    }

    #[test]
    fn push_batch_inserts_rows_and_round_trips_columns() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        let batch = vec![
            LogLine {
                timestamp: 100,
                process: "web".into(),
                group: Some("backend".into()),
                content: "first".into(),
                source: LogSource::Process,
                output_kind: OutputKind::Stdout,
            },
            LogLine {
                timestamp: 200,
                process: "web".into(),
                group: None,
                content: "second".into(),
                source: LogSource::Daemon,
                output_kind: OutputKind::Combined,
            },
            LogLine {
                timestamp: 300,
                process: "worker".into(),
                group: Some("queue".into()),
                content: "third".into(),
                source: LogSource::Process,
                output_kind: OutputKind::Stderr,
            },
        ];
        storage.push_batch(batch).expect("push_batch");

        assert_eq!(row_count(&storage.conn), 3);

        let result = storage.query(LogQuery::all()).expect("query");
        assert_eq!(result.logs.len(), 3);
        assert_eq!(result.total_count, 3);
        assert!(!result.has_more);
        assert_eq!(result.offset, 0);

        // Ordering by (timestamp ASC, id ASC) places insertion order.
        assert_eq!(result.logs[0].timestamp, 100);
        assert_eq!(result.logs[0].process, "web");
        assert_eq!(result.logs[0].group.as_deref(), Some("backend"));
        assert_eq!(result.logs[0].source, LogSource::Process);
        assert_eq!(result.logs[0].output_kind, OutputKind::Stdout);
        assert_eq!(result.logs[0].content, "first");

        assert_eq!(result.logs[1].source, LogSource::Daemon);
        assert_eq!(result.logs[1].group, None);

        assert_eq!(result.logs[2].process, "worker");
        assert_eq!(result.logs[2].output_kind, OutputKind::Stderr);
    }

    #[test]
    fn push_batch_empty_is_noop() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage.push_batch(Vec::new()).expect("empty batch");
        assert_eq!(row_count(&storage.conn), 0);
    }

    #[test]
    fn push_batch_rolls_back_on_failure() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        // Seed a row directly so we can confirm pre-existing state survives.
        storage
            .conn
            .execute(
                "INSERT INTO log_entries
                    (timestamp_ms, process, group_name, source, output_kind, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    50_i64,
                    "seed",
                    Option::<&str>::None,
                    "process",
                    "combined",
                    "pre-existing",
                ),
            )
            .expect("seed row");
        install_poison_trigger(&storage.conn, "__POISON__");

        let batch = vec![
            line(100, "web", None, "ok-1"),
            line(200, "web", None, "__POISON__"),
            line(300, "web", None, "ok-3"),
        ];
        let err = storage
            .push_batch(batch)
            .expect_err("expected write failure");
        match err {
            LogStorageError::Write(msg) => {
                assert!(msg.contains("poison"), "unexpected message: {msg}");
            }
            other => panic!("expected Write, got {other:?}"),
        }

        // The seed row survives; none of the three batch rows landed.
        assert_eq!(row_count(&storage.conn), 1);
        let content: String = storage
            .conn
            .query_row("SELECT content FROM log_entries", (), |row| row.get(0))
            .expect("read seed content");
        assert_eq!(content, "pre-existing");
    }

    #[test]
    fn query_pagination_matches_memory_semantics() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        let batch: Vec<LogLine> = (0..20)
            .map(|i| line(i as u64 * 100, "task1", None, &format!("line {i}")))
            .collect();
        storage.push_batch(batch).expect("push_batch");

        let all = storage.query(LogQuery::process("task1")).expect("query");
        assert_eq!(all.logs.len(), 20);
        assert_eq!(all.total_count, 20);
        assert!(!all.has_more);
        assert_eq!(all.offset, 0);

        let first_page = storage
            .query(LogQuery::process("task1").with_limit(5))
            .expect("query");
        assert_eq!(first_page.logs.len(), 5);
        assert_eq!(first_page.total_count, 20);
        assert!(first_page.has_more);
        assert_eq!(first_page.logs[0].content, "line 0");
        assert_eq!(first_page.logs[4].content, "line 4");

        let middle_page = storage
            .query(LogQuery::process("task1").with_offset(10).with_limit(5))
            .expect("query");
        assert_eq!(middle_page.logs.len(), 5);
        assert_eq!(middle_page.total_count, 20);
        assert!(middle_page.has_more);
        assert_eq!(middle_page.offset, 10);
        assert_eq!(middle_page.logs[0].content, "line 10");

        let tail_page = storage
            .query(LogQuery::process("task1").with_offset(15).with_limit(10))
            .expect("query");
        assert_eq!(tail_page.logs.len(), 5);
        assert_eq!(tail_page.total_count, 20);
        assert!(!tail_page.has_more);
        assert_eq!(tail_page.offset, 15);
        assert_eq!(tail_page.logs[0].content, "line 15");
        assert_eq!(tail_page.logs[4].content, "line 19");
    }

    #[test]
    fn query_filters_process_and_group() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage
            .push_batch(vec![
                line(100, "web", Some("backend"), "a"),
                line(200, "web", None, "b"),
                line(300, "worker", Some("queue"), "c"),
                line(400, "worker", Some("backend"), "d"),
            ])
            .expect("push_batch");

        let web = storage.query(LogQuery::process("web")).expect("query");
        assert_eq!(web.total_count, 2);
        assert!(web.logs.iter().all(|l| l.process == "web"));

        let backend = storage
            .query(LogQuery::all().with_group("backend"))
            .expect("query");
        assert_eq!(backend.total_count, 2);
        assert!(backend
            .logs
            .iter()
            .all(|l| l.group.as_deref() == Some("backend")));

        // Group filter with no match returns an empty page (NULL groups
        // are excluded, matching the in-memory `Option::as_ref()` semantics).
        let missing = storage
            .query(LogQuery::all().with_group("nope"))
            .expect("query");
        assert_eq!(missing.total_count, 0);
        assert!(missing.logs.is_empty());
    }

    #[test]
    fn query_search_is_case_insensitive() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage
            .push_batch(vec![
                line(100, "web", None, "INFO: started"),
                line(200, "web", None, "ERROR: something failed"),
                line(300, "web", None, "INFO: processing"),
                line(400, "web", None, "ERROR: another Failure"),
                line(500, "web", None, "INFO: done"),
            ])
            .expect("push_batch");

        for needle in ["error", "ERROR", "Error"] {
            let result = storage
                .query(LogQuery::all().with_search(needle))
                .expect("query");
            assert_eq!(result.total_count, 2, "needle {needle}");
            assert!(result
                .logs
                .iter()
                .all(|l| l.content.to_lowercase().contains("error")));
        }

        let info = storage
            .query(LogQuery::all().with_search("INFO").with_limit(2))
            .expect("query");
        assert_eq!(info.total_count, 3);
        assert_eq!(info.logs.len(), 2);
        assert!(info.has_more);

        let empty = storage
            .query(LogQuery::all().with_search("not-present"))
            .expect("query");
        assert_eq!(empty.total_count, 0);
        assert!(empty.logs.is_empty());
    }

    #[test]
    fn query_since_until_are_inclusive() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage
            .push_batch(vec![
                line(100, "web", None, "a"),
                line(200, "web", None, "b"),
                line(300, "web", None, "c"),
                line(400, "web", None, "d"),
                line(500, "web", None, "e"),
            ])
            .expect("push_batch");

        let since = storage
            .query(LogQuery::all().with_since(200))
            .expect("query");
        assert_eq!(since.total_count, 4);
        assert_eq!(since.logs.first().map(|l| l.timestamp), Some(200));

        let until = storage
            .query(LogQuery::all().with_until(300))
            .expect("query");
        assert_eq!(until.total_count, 3);
        assert_eq!(until.logs.last().map(|l| l.timestamp), Some(300));

        let window = storage
            .query(LogQuery::all().with_since(200).with_until(400))
            .expect("query");
        assert_eq!(window.total_count, 3);
        assert_eq!(window.logs.first().map(|l| l.timestamp), Some(200));
        assert_eq!(window.logs.last().map(|l| l.timestamp), Some(400));
    }

    #[test]
    fn stats_reports_persistent_aggregates() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");

        let empty = storage.stats().expect("stats");
        assert_eq!(empty.total_lines, 0);
        assert_eq!(empty.process_count, 0);
        assert_eq!(empty.oldest_timestamp, None);
        assert_eq!(empty.newest_timestamp, None);
        assert_eq!(empty.memory_bytes, 0);
        assert_eq!(empty.memory_limit, None);
        assert_eq!(empty.max_lines_per_process, 0);

        storage
            .push_batch(vec![
                line(100, "web", None, "a"),
                line(250, "web", None, "b"),
                line(50, "worker", None, "c"),
                line(900, "worker", None, "d"),
            ])
            .expect("push_batch");

        let stats = storage.stats().expect("stats");
        assert_eq!(stats.total_lines, 4);
        assert_eq!(stats.process_count, 2);
        assert_eq!(stats.oldest_timestamp, Some(50));
        assert_eq!(stats.newest_timestamp, Some(900));
    }

    #[test]
    fn clear_removes_all_rows() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage
            .push_batch(vec![
                line(100, "web", None, "a"),
                line(200, "worker", None, "b"),
            ])
            .expect("push_batch");
        assert_eq!(row_count(&storage.conn), 2);

        storage.clear().expect("clear");
        assert_eq!(row_count(&storage.conn), 0);
    }

    #[test]
    fn clear_process_removes_only_named_rows() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage
            .push_batch(vec![
                line(100, "web", None, "a"),
                line(200, "worker", None, "b"),
                line(300, "web", None, "c"),
            ])
            .expect("push_batch");

        storage.clear_process("web").expect("clear_process");

        let remaining = storage.query(LogQuery::all()).expect("query");
        assert_eq!(remaining.total_count, 1);
        assert_eq!(remaining.logs[0].process, "worker");
        assert_eq!(remaining.logs[0].content, "b");
    }

    #[test]
    fn flush_now_truncates_wal_and_preserves_rows() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("logs.sqlite3");
        let wal_path = dir.path().join("logs.sqlite3-wal");

        let mut storage = SqliteLogStorage::open(&path).expect("open");
        let batch: Vec<LogLine> = (0..200)
            .map(|i| line(i as u64, "web", None, &format!("payload {i}")))
            .collect();
        storage.push_batch(batch).expect("push_batch");

        let wal_size_before = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert!(
            wal_size_before > 0,
            "WAL sibling should exist and be non-empty"
        );

        storage.flush_now().expect("flush_now");

        let wal_size_after = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
        assert_eq!(
            wal_size_after, 0,
            "wal_checkpoint(TRUNCATE) should reduce the WAL file to zero",
        );

        drop(storage);
        let reopened = SqliteLogStorage::open(&path).expect("reopen");
        let result = reopened.query(LogQuery::all()).expect("query");
        assert_eq!(result.total_count, 200);
    }

    #[test]
    fn flush_now_is_noop_on_in_memory() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage
            .push_batch(vec![line(100, "web", None, "a")])
            .expect("push_batch");
        storage.flush_now().expect("flush_now no-op");
    }

    // ---------- LogStorage trait impl ----------

    #[test]
    fn trait_push_round_trips_single_line() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        let log = line(100, "web", Some("backend"), "single");
        LogStorage::push(&mut storage, log).expect("push");

        let logs = LogStorage::get(&storage, "web", None).expect("get");
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].content, "single");
        assert_eq!(logs[0].group.as_deref(), Some("backend"));
    }

    #[test]
    fn trait_push_batch_round_trips() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        let batch = vec![
            line(100, "web", None, "one"),
            line(200, "web", None, "two"),
            line(300, "web", None, "three"),
        ];
        LogStorage::push_batch(&mut storage, batch).expect("push_batch");

        let logs = LogStorage::get(&storage, "web", None).expect("get");
        assert_eq!(logs.len(), 3);
        assert_eq!(logs[0].content, "one");
        assert_eq!(logs[2].content, "three");
    }

    #[test]
    fn trait_get_returns_most_recent_when_limited() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        for i in 0..10 {
            LogStorage::push(&mut storage, line(i as u64 * 100, "web", None, &format!("l{i}")))
                .expect("push");
        }
        let last_three = LogStorage::get(&storage, "web", Some(3)).expect("get");
        assert_eq!(last_three.len(), 3);
        assert_eq!(last_three[0].content, "l7");
        assert_eq!(last_three[2].content, "l9");
    }

    #[test]
    fn trait_get_all_processes() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        LogStorage::push(&mut storage, line(100, "web", None, "a")).expect("push");
        LogStorage::push(&mut storage, line(200, "worker", None, "b")).expect("push");
        LogStorage::push(&mut storage, line(300, "web", None, "c")).expect("push");

        let all = LogStorage::get(&storage, "*", None).expect("get");
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].timestamp, 100);
        assert_eq!(all[2].timestamp, 300);
    }

    #[test]
    fn trait_clear_removes_all() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        LogStorage::push(&mut storage, line(100, "web", None, "a")).expect("push");
        LogStorage::clear(&mut storage).expect("clear");
        assert!(LogStorage::get(&storage, "*", None).expect("get").is_empty());
    }

    #[test]
    fn trait_lifecycle_hooks_are_noop() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        // flush / enforce_retention always succeed against the SQLite backend
        // even when no rows exist.
        LogStorage::flush(&mut storage).expect("flush");
        LogStorage::enforce_retention(&mut storage).expect("retention");
        // memory_limit / max_lines_per_process are hot-buffer concepts; the
        // hybrid wrapper answers them, so the SQLite-side trait impl returns
        // neutral values.
        assert!(LogStorage::memory_limit(&storage).is_none());
        assert_eq!(LogStorage::max_lines_per_process(&storage), 0);
        assert!(storage.set_memory_limit(123).is_none());
        assert_eq!(storage.set_max_lines_per_process(456), 0);
    }

    // ---------- enforce_retention ----------

    fn process_row_count(conn: &Connection, process: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM log_entries WHERE process = ?1",
            (process,),
            |row| row.get(0),
        )
        .expect("count rows for process")
    }

    fn ids_for_process(conn: &Connection, process: &str) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT id FROM log_entries WHERE process = ?1 ORDER BY id ASC")
            .expect("prepare id select");
        let rows = stmt
            .query_map((process,), |row| row.get::<_, i64>(0))
            .expect("query ids");
        rows.map(|r| r.expect("decode id")).collect()
    }

    #[test]
    fn enforce_retention_noop_without_policy() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        storage
            .push_batch(vec![
                line(100, "web", None, "a"),
                line(200, "web", None, "b"),
            ])
            .expect("push_batch");

        storage.enforce_retention().expect("retention");

        assert_eq!(row_count(&storage.conn), 2);
    }

    #[test]
    fn enforce_retention_noop_when_under_limits() {
        let mut storage = SqliteLogStorage::open_in_memory()
            .expect("open")
            .with_retention(RetentionPolicy {
                max_size_bytes: 100 * 1024 * 1024,
                max_lines_per_process: 1_000,
            });
        let batch: Vec<LogLine> = (0..50)
            .map(|i| line(i as u64 * 10, "web", None, &format!("line {i}")))
            .collect();
        storage.push_batch(batch).expect("push_batch");

        storage.enforce_retention().expect("retention");

        assert_eq!(row_count(&storage.conn), 50);
    }

    #[test]
    fn enforce_retention_trims_per_process_to_limit() {
        let mut storage = SqliteLogStorage::open_in_memory()
            .expect("open")
            .with_retention(RetentionPolicy {
                max_size_bytes: 100 * 1024 * 1024,
                max_lines_per_process: 10,
            });
        let batch: Vec<LogLine> = (0..30)
            .map(|i| line(i as u64 * 10, "web", None, &format!("line {i}")))
            .collect();
        storage.push_batch(batch).expect("push_batch");
        assert_eq!(process_row_count(&storage.conn, "web"), 30);

        storage.enforce_retention().expect("retention");

        assert_eq!(process_row_count(&storage.conn, "web"), 10);
        let logs = storage.get_internal("web", None).expect("get");
        // The ten most recent rows survive; ascending-order contract holds.
        assert_eq!(logs.len(), 10);
        assert_eq!(logs[0].content, "line 20");
        assert_eq!(logs[9].content, "line 29");
    }

    #[test]
    fn enforce_retention_per_process_independent() {
        let mut storage = SqliteLogStorage::open_in_memory()
            .expect("open")
            .with_retention(RetentionPolicy {
                max_size_bytes: 100 * 1024 * 1024,
                max_lines_per_process: 5,
            });
        let mut batch = Vec::new();
        for i in 0..20 {
            batch.push(line(i as u64 * 10, "web", None, &format!("w{i}")));
        }
        for i in 0..3 {
            batch.push(line(2_000 + i as u64 * 10, "worker", None, &format!("k{i}")));
        }
        storage.push_batch(batch).expect("push_batch");

        storage.enforce_retention().expect("retention");

        assert_eq!(process_row_count(&storage.conn, "web"), 5);
        // Worker stayed under the limit and is untouched.
        assert_eq!(process_row_count(&storage.conn, "worker"), 3);
    }

    #[test]
    fn enforce_retention_handles_stale_process() {
        let mut storage = SqliteLogStorage::open_in_memory()
            .expect("open")
            .with_retention(RetentionPolicy {
                max_size_bytes: 100 * 1024 * 1024,
                max_lines_per_process: 4,
            });
        // The stale process emitted in the past and has not written
        // since; the per-process pass should still trim it.
        let batch: Vec<LogLine> = (0..20)
            .map(|i| line(i as u64 * 10, "stale", None, &format!("s{i}")))
            .collect();
        storage.push_batch(batch).expect("push_batch");

        storage.enforce_retention().expect("retention");

        assert_eq!(process_row_count(&storage.conn, "stale"), 4);
    }

    #[test]
    fn enforce_retention_trims_oldest_for_size_budget() {
        let mut storage = SqliteLogStorage::open_in_memory().expect("open");
        // Push enough rows that the size sweep must iterate the chunked
        // delete loop at least twice. A single chunk deletes
        // `RETENTION_CHUNK` rows, so a smaller dataset would either be
        // wiped in one pass or not need pruning at all.
        let payload = "x".repeat(1_024);
        let total_rows = (RETENTION_CHUNK as usize) * 5;
        let batch: Vec<LogLine> = (0..total_rows)
            .map(|i| line(i as u64, "web", None, &payload))
            .collect();
        storage.push_batch(batch).expect("push_batch");

        let page_size = storage.read_pragma_int("page_size").expect("page_size");
        let before = storage.logical_data_bytes(page_size).expect("logical");
        assert!(before > 1_000_000, "expected sizeable DB, got {before}");

        // Half-of-original budget should leave the loop with rows remaining
        // because each chunked pass removes ~1/5 of the data.
        let budget = (before as usize) / 2;
        storage = storage.with_retention(RetentionPolicy {
            max_size_bytes: budget,
            max_lines_per_process: 1_000_000,
        });
        storage.enforce_retention().expect("retention");

        let after = storage.logical_data_bytes(page_size).expect("logical");
        assert!(
            after <= budget as i64,
            "logical size {after} should be at or under budget {budget}",
        );

        let remaining = row_count(&storage.conn);
        assert!(
            remaining > 0 && (remaining as usize) < total_rows,
            "expected partial trim, got {remaining} of {total_rows}",
        );

        // Surviving rows are the most recent ones: their ids are larger
        // than any deleted row, and ascending-order id is the insert order.
        let ids = ids_for_process(&storage.conn, "web");
        let first_id = *ids.first().expect("at least one surviving row");
        assert!(
            first_id > 1,
            "oldest surviving id {first_id} should be past the deleted prefix",
        );
    }

    #[test]
    fn enforce_retention_size_sweep_bails_when_empty() {
        let mut storage = SqliteLogStorage::open_in_memory()
            .expect("open")
            .with_retention(RetentionPolicy {
                max_size_bytes: 1,
                max_lines_per_process: 1_000,
            });
        // No rows: nothing to delete, so the loop must terminate cleanly
        // even though the empty DB can be larger than the absurd budget.
        storage.enforce_retention().expect("retention");
        assert_eq!(row_count(&storage.conn), 0);
    }

    // ---------- db_path_for_config ----------

    #[test]
    fn db_path_in_base_keys_off_canonical_config_path() {
        let base = tempdir().expect("tempdir");
        // Use a path that does not exist on disk so canonicalize falls
        // through to the literal value; deterministic regardless of CWD.
        let cfg = PathBuf::from("/tmp/zaz-test-config-a/zaz.toml");
        let p1 = db_path_in_base(base.path(), &cfg);
        let p2 = db_path_in_base(base.path(), &cfg);
        assert_eq!(p1, p2, "same config path should resolve to same DB path");

        let cfg2 = PathBuf::from("/tmp/zaz-test-config-b/zaz.toml");
        let p3 = db_path_in_base(base.path(), &cfg2);
        assert_ne!(
            p1, p3,
            "different config paths should resolve to different DB paths"
        );

        // Layout: <base>/zaz/logs/<16-hex>.sqlite3
        assert!(p1.starts_with(base.path().join("zaz").join("logs")));
        assert_eq!(p1.extension().and_then(|e| e.to_str()), Some("sqlite3"));
        let stem = p1.file_stem().and_then(|s| s.to_str()).expect("stem");
        assert_eq!(stem.len(), 16);
        assert!(stem.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn db_path_in_base_creates_logs_directory_with_0o700() {
        let base = tempdir().expect("tempdir");
        let cfg = PathBuf::from("/tmp/zaz-test-dir-perm/zaz.toml");
        let _ = db_path_in_base(base.path(), &cfg);

        let logs_dir = base.path().join("zaz").join("logs");
        assert!(logs_dir.is_dir());
        let meta = std::fs::metadata(&logs_dir).expect("metadata");
        let mode = meta.permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "expected 0o700, got {mode:o}");
    }
}
