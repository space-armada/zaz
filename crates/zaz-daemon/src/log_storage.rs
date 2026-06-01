//! Log storage abstraction layer.
//!
//! This module defines the `LogStorage` trait that abstracts the underlying
//! storage backend. The current implementation uses in-memory storage with
//! VecDeque ring buffers. Future implementations could use SQLite or other
//! backends while maintaining a consistent interface.
//!
//! # Design Principles
//!
//! - **Backward compatibility**: The simple `get()` method provides backward
//!   compatibility with the existing API. New features use `query()`.
//! - **Pagination support**: All query methods support offset/limit for
//!   efficient handling of large log volumes.
//! - **Future-proof**: The trait is designed to work with both in-memory
//!   and persistent storage backends.

use crate::api::LogLine;
use crate::error::LogStorageError;

/// Query parameters for retrieving logs.
///
/// All fields are optional to maintain backward compatibility.
/// Missing fields use sensible defaults.
#[derive(Debug, Clone, Default)]
pub struct LogQuery {
    /// Process name filter. Use "*" or None for all processes.
    pub process: Option<String>,

    /// Group name filter. If None, all groups are included.
    pub group: Option<String>,

    /// Number of results to skip (for pagination).
    /// Defaults to 0.
    pub offset: Option<usize>,

    /// Maximum number of results to return.
    /// If None, returns all matching logs (up to implementation limit).
    pub limit: Option<usize>,

    /// Text search pattern (case-insensitive substring match).
    /// If None, no text filtering is applied.
    pub search: Option<String>,

    /// Minimum timestamp (inclusive, milliseconds since epoch).
    pub since: Option<u64>,

    /// Maximum timestamp (inclusive, milliseconds since epoch).
    pub until: Option<u64>,
}

impl LogQuery {
    /// Create a query for a specific process.
    pub fn process(name: impl Into<String>) -> Self {
        Self {
            process: Some(name.into()),
            ..Default::default()
        }
    }

    /// Create a query for all processes.
    pub fn all() -> Self {
        Self {
            process: Some("*".to_string()),
            ..Default::default()
        }
    }

    /// Set the offset (builder pattern).
    pub fn with_offset(mut self, offset: usize) -> Self {
        self.offset = Some(offset);
        self
    }

    /// Set the limit (builder pattern).
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    /// Set the search pattern (builder pattern).
    pub fn with_search(mut self, pattern: impl Into<String>) -> Self {
        self.search = Some(pattern.into());
        self
    }

    /// Set the since timestamp filter (builder pattern).
    pub fn with_since(mut self, timestamp: u64) -> Self {
        self.since = Some(timestamp);
        self
    }

    /// Set the until timestamp filter (builder pattern).
    pub fn with_until(mut self, timestamp: u64) -> Self {
        self.until = Some(timestamp);
        self
    }

    /// Set the group filter (builder pattern).
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }

    /// Returns the effective process filter, treating None as "*".
    pub fn effective_process(&self) -> &str {
        self.process.as_deref().unwrap_or("*")
    }

    /// Returns whether this query is for all processes.
    pub fn is_all_processes(&self) -> bool {
        matches!(self.effective_process(), "*")
    }
}

/// Result of a log query including pagination metadata.
#[derive(Debug, Clone)]
pub struct LogQueryResult {
    /// The matching log lines.
    pub logs: Vec<LogLine>,

    /// Total number of matching logs (before pagination).
    /// Useful for UI pagination controls.
    pub total_count: usize,

    /// Whether there are more results after this page.
    pub has_more: bool,

    /// The offset used for this query.
    pub offset: usize,
}

impl LogQueryResult {
    /// Create an empty result.
    pub fn empty() -> Self {
        Self {
            logs: Vec::new(),
            total_count: 0,
            has_more: false,
            offset: 0,
        }
    }
}

/// Statistics about the log storage.
#[derive(Debug, Clone, Default)]
pub struct LogStorageStats {
    /// Total number of log lines stored.
    pub total_lines: usize,

    /// Number of distinct processes with logs.
    pub process_count: usize,

    /// Approximate memory usage in bytes.
    pub memory_bytes: usize,

    /// Oldest log timestamp (if any logs exist).
    pub oldest_timestamp: Option<u64>,

    /// Newest log timestamp (if any logs exist).
    pub newest_timestamp: Option<u64>,

    /// Memory limit in bytes (if configured).
    pub memory_limit: Option<usize>,

    /// Maximum lines per process.
    pub max_lines_per_process: usize,
}

/// Trait for log storage backends.
///
/// This trait defines the interface for storing and querying logs.
/// Implementations can use in-memory storage, SQLite, or other backends.
///
/// # Fallibility
///
/// All read and write methods return `Result<_, LogStorageError>`. The
/// in-memory backend never fails, but a persistent backend can fail on open,
/// schema init, write, query, lock timeout, or retention. Callers must
/// propagate or surface the error rather than silently dropping it.
///
/// # Lifecycle hooks
///
/// - [`push_batch`](LogStorage::push_batch) is the primary write extension.
///   Persistent backends commit the batch inside one transaction so a partial
///   write does not leak.
/// - [`flush`](LogStorage::flush) is the periodic-cadence hook the runtime
///   calls between batches.
/// - [`flush_now`](LogStorage::flush_now) is the shutdown hook. Persistent
///   backends must commit and checkpoint before returning; the memory backend
///   is a no-op.
/// - [`enforce_retention`](LogStorage::enforce_retention) runs an explicit
///   retention sweep. The memory backend enforces its limits inline on push
///   and treats this as a no-op.
///
/// # Thread Safety
///
/// The trait requires `Send` to allow moving between threads. Implementations
/// that need interior mutability should use appropriate synchronization.
///
/// # Example
///
/// ```rust,ignore
/// use crate::log_storage::{LogQuery, LogQueryResult};
///
/// // Simple query (backward compatible)
/// let logs = storage.get("web", Some(10))?;
///
/// // Advanced query with pagination and search
/// let result = storage.query(
///     LogQuery::all()
///         .with_search("error")
///         .with_limit(50)
///         .with_offset(100)
/// )?;
///
/// println!("Showing {}/{} logs", result.logs.len(), result.total_count);
/// ```
pub trait LogStorage: Send {
    /// Push a log line to storage.
    ///
    /// The storage may apply eviction policies if limits are exceeded.
    fn push(&mut self, log: LogLine) -> Result<(), LogStorageError>;

    /// Push a batch of log lines in one operation.
    ///
    /// Persistent backends commit the batch atomically; in-memory backends
    /// may apply per-line eviction inside the loop.
    fn push_batch(&mut self, logs: Vec<LogLine>) -> Result<(), LogStorageError>;

    /// Get logs for a process (backward compatible simple interface).
    ///
    /// This is equivalent to calling `query()` with a `LogQuery` for the
    /// given process and limit, returning the most recent `limit` logs.
    ///
    /// # Arguments
    ///
    /// - `name`: Process name, or "*" for all processes.
    /// - `limit`: Maximum logs to return. If None, returns all logs.
    ///
    /// # Returns
    ///
    /// Logs sorted by timestamp ascending (oldest first), with the most
    /// recent `limit` logs if a limit is specified.
    fn get(&self, name: &str, limit: Option<usize>) -> Result<Vec<LogLine>, LogStorageError>;

    /// Query logs with advanced filtering and pagination.
    ///
    /// This is the primary method for retrieving logs with full control
    /// over filtering, pagination, and sorting.
    fn query(&self, query: LogQuery) -> Result<LogQueryResult, LogStorageError>;

    /// Get statistics about the storage.
    fn stats(&self) -> Result<LogStorageStats, LogStorageError>;

    /// Clear all logs.
    fn clear(&mut self) -> Result<(), LogStorageError>;

    /// Clear logs for a specific process.
    fn clear_process(&mut self, name: &str) -> Result<(), LogStorageError>;

    /// Periodic flush hook called between drain cycles.
    ///
    /// In-memory backends are a no-op. Persistent backends should ensure any
    /// pending writes are durable up to the call site.
    fn flush(&mut self) -> Result<(), LogStorageError>;

    /// Shutdown flush hook. Implementations must guarantee durability before
    /// returning so the daemon can exit without losing buffered history.
    fn flush_now(&mut self) -> Result<(), LogStorageError>;

    /// Run an explicit retention sweep.
    ///
    /// The memory backend enforces retention inline on every push and treats
    /// this call as a no-op. Persistent backends prune outside the write path
    /// to keep batch latency bounded.
    fn enforce_retention(&mut self) -> Result<(), LogStorageError>;

    /// Get the current memory limit in bytes, if set.
    fn memory_limit(&self) -> Option<usize>;

    /// Set the maximum memory budget in bytes.
    ///
    /// Implementations should evict old logs when this limit is approached.
    /// Returns the previous limit, if any.
    fn set_memory_limit(&mut self, bytes: usize) -> Option<usize>;

    /// Get the current max lines per process.
    fn max_lines_per_process(&self) -> usize;

    /// Set the maximum logs per process.
    ///
    /// When a process exceeds this limit, oldest logs are evicted.
    /// Returns the previous limit.
    fn set_max_lines_per_process(&mut self, max: usize) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_query_builder() {
        let query = LogQuery::process("web")
            .with_offset(10)
            .with_limit(50)
            .with_search("error");

        assert_eq!(query.process, Some("web".to_string()));
        assert_eq!(query.offset, Some(10));
        assert_eq!(query.limit, Some(50));
        assert_eq!(query.search, Some("error".to_string()));
    }

    #[test]
    fn test_log_query_all() {
        let query = LogQuery::all();
        assert!(query.is_all_processes());
        assert_eq!(query.effective_process(), "*");
    }

    #[test]
    fn test_log_query_default_is_all() {
        let query = LogQuery::default();
        assert!(query.is_all_processes());
    }

    #[test]
    fn test_log_query_result_empty() {
        let result = LogQueryResult::empty();
        assert!(result.logs.is_empty());
        assert_eq!(result.total_count, 0);
        assert!(!result.has_more);
        assert_eq!(result.offset, 0);
    }
}
