//! Log storage with async channel support.
//!
//! This module provides a testable log storage abstraction that:
//! - Receives logs from spawned tasks via an async channel
//! - Stores logs in per-process ring buffers
//! - Returns logs sorted by timestamp
//! - Supports pagination via the `LogStorage` trait
//!
//! The key invariant is: logs sent to the channel are available
//! via `get()` after calling `drain()`.

use crate::api::{LogLine, LogSource};
use crate::error::LogStorageError;
use crate::log_storage::{LogQuery, LogQueryResult, LogStorage, LogStorageStats};
use crate::log_storage_sqlite::SqliteLogStorage;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};

/// Default maximum log lines to keep per process.
const DEFAULT_MAX_LINES_PER_PROCESS: usize = 100_000;

/// Default memory limit (100 MB).
const DEFAULT_MEMORY_LIMIT: usize = 100 * 1024 * 1024;

/// Log storage with channel-based ingestion.
///
/// Spawned tasks send logs via the channel sender. The main loop
/// must call `drain()` to move logs from the channel to the buffer
/// before they become visible via `get()`.
pub struct LogStore {
    /// Per-process log ring buffers.
    buffers: HashMap<String, VecDeque<LogLine>>,

    /// Channel receiver for logs from spawned tasks.
    rx: mpsc::Receiver<LogLine>,

    /// Channel sender (cloned for spawned tasks).
    tx: mpsc::Sender<LogLine>,

    /// Broadcast channel for real-time log streaming to subscribers.
    broadcast_tx: broadcast::Sender<LogLine>,

    /// Maximum lines per process.
    max_lines_per_process: usize,

    /// Memory limit in bytes (for global eviction).
    memory_limit: Option<usize>,

    /// Estimated current memory usage in bytes.
    estimated_memory: usize,

    /// Callback for verbose output (optional).
    #[allow(clippy::type_complexity)]
    verbose_callback: Option<Box<dyn Fn(&LogLine) + Send>>,

    /// Optional persistent SQLite backend. When attached, drained
    /// batches are also written through to disk and historical queries
    /// route to SQLite; the hot buffer continues to back broadcast and
    /// the simple `get(name, limit)` recent-tail path so live UI stays
    /// responsive even when the disk backend is slow or degraded.
    sqlite: Option<Box<SqliteLogStorage>>,

    /// Wall-clock instant the persistent retention sweep last ran.
    /// `push_batch` and the periodic tick share this gate so the tick
    /// short-circuits when an after-batch sweep already covered the
    /// window.
    last_retention_at: Option<Instant>,
}

impl LogStore {
    /// Create a new log store with default settings.
    pub fn new() -> Self {
        Self::with_capacity(1024)
    }

    /// Create a new log store with specified channel capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let (tx, rx) = mpsc::channel(capacity);
        let (broadcast_tx, _) = broadcast::channel(256);

        Self {
            buffers: HashMap::new(),
            rx,
            tx,
            broadcast_tx,
            max_lines_per_process: DEFAULT_MAX_LINES_PER_PROCESS,
            memory_limit: Some(DEFAULT_MEMORY_LIMIT),
            estimated_memory: 0,
            verbose_callback: None,
            sqlite: None,
            last_retention_at: None,
        }
    }

    /// Attach a SQLite backend. When attached, drained batches are
    /// also written through to disk, historical queries route to
    /// SQLite, and `clear` / `clear_process` / `flush_now` /
    /// `enforce_retention` propagate to both backends.
    pub fn with_sqlite(mut self, storage: SqliteLogStorage) -> Self {
        self.sqlite = Some(Box::new(storage));
        self
    }

    /// Set a callback for verbose output.
    pub fn with_verbose_callback<F>(mut self, callback: F) -> Self
    where
        F: Fn(&LogLine) + Send + 'static,
    {
        self.verbose_callback = Some(Box::new(callback));
        self
    }

    /// Set maximum lines per process (builder pattern).
    pub fn with_max_lines_per_process(mut self, max: usize) -> Self {
        self.max_lines_per_process = max;
        self
    }

    /// Set memory limit in bytes (builder pattern).
    pub fn with_memory_limit(mut self, bytes: usize) -> Self {
        self.memory_limit = Some(bytes);
        self
    }

    /// Get a sender for spawned tasks to submit logs.
    pub fn sender(&self) -> mpsc::Sender<LogLine> {
        self.tx.clone()
    }

    /// Subscribe to real-time log broadcasts.
    pub fn subscribe(&self) -> broadcast::Receiver<LogLine> {
        self.broadcast_tx.subscribe()
    }

    /// Drain logs from the channel into the buffer.
    ///
    /// This MUST be called before `get()` to ensure logs from
    /// spawned tasks are visible. Call this:
    /// - Before handling any API request
    /// - Periodically in the main loop
    ///
    /// The drained lines are written through `push_batch` so persistent
    /// backends can commit the residual set inside a single transaction.
    pub fn drain(&mut self) -> Result<(), LogStorageError> {
        let mut batch = Vec::new();
        while let Ok(log) = self.rx.try_recv() {
            batch.push(log);
        }
        if batch.is_empty() {
            return Ok(());
        }
        self.push_batch(batch)
    }

    /// Drain the channel and run the shutdown-time flush hook.
    ///
    /// This is the contract invoked from `Engine::shutdown` so persistent
    /// backends can commit and checkpoint before exit. The memory backend's
    /// `flush_now` is a no-op.
    pub fn drain_and_flush_now(&mut self) -> Result<(), LogStorageError> {
        self.drain()?;
        self.flush_now()
    }

    /// Run the persistent retention sweep if `cadence` has elapsed since
    /// the last run.
    ///
    /// The after-batch path inside [`push_batch`] also bumps the gate,
    /// so an active daemon under steady writes never re-runs retention
    /// here. The tick exists to keep the budget honest during quiet
    /// periods, and is a no-op when no SQLite backend is attached.
    pub fn maybe_enforce_retention_tick(
        &mut self,
        cadence: Duration,
    ) -> Result<(), LogStorageError> {
        if self.sqlite.is_none() {
            return Ok(());
        }
        let now = Instant::now();
        let due = match self.last_retention_at {
            Some(last) => now.duration_since(last) >= cadence,
            None => true,
        };
        if !due {
            return Ok(());
        }
        self.enforce_retention()?;
        self.last_retention_at = Some(now);
        Ok(())
    }

    /// Internal push that handles storage, trimming, and broadcast.
    fn push_internal(&mut self, log: LogLine) {
        // Verbose callback
        if let Some(ref callback) = self.verbose_callback {
            if log.source == LogSource::Process {
                callback(&log);
            }
        }

        // Track memory before adding
        let log_size = Self::estimate_log_size(&log);
        self.estimated_memory += log_size;

        // Store in per-process buffer
        let buffer = self.buffers.entry(log.process.clone()).or_default();
        buffer.push_back(log.clone());

        // Per-process eviction: trim to max size
        while buffer.len() > self.max_lines_per_process {
            if let Some(evicted) = buffer.pop_front() {
                self.estimated_memory = self
                    .estimated_memory
                    .saturating_sub(Self::estimate_log_size(&evicted));
            }
        }

        // Global memory eviction
        self.maybe_evict_for_memory();

        // Broadcast to subscribers (ignore errors if no subscribers)
        let _ = self.broadcast_tx.send(log);
    }

    /// Estimate the memory size of a log line in bytes.
    fn estimate_log_size(log: &LogLine) -> usize {
        std::mem::size_of::<LogLine>()
            + log.process.len()
            + log.content.len()
            + log.group.as_ref().map(|s| s.len()).unwrap_or(0)
    }

    /// Evict logs if memory limit is exceeded.
    ///
    /// Uses a fair eviction strategy: removes oldest logs from processes
    /// with the most logs until memory is under the target threshold.
    fn maybe_evict_for_memory(&mut self) {
        let Some(limit) = self.memory_limit else {
            return;
        };

        // Eviction threshold: 90% of limit
        let threshold = (limit as f64 * 0.9) as usize;
        // Target: 80% of limit
        let target = (limit as f64 * 0.8) as usize;

        if self.estimated_memory <= threshold {
            return;
        }

        // Evict from processes with most logs until under target
        while self.estimated_memory > target {
            // Find process with most logs
            let max_process = self
                .buffers
                .iter()
                .filter(|(_, buf)| !buf.is_empty())
                .max_by_key(|(_, buf)| buf.len())
                .map(|(name, _)| name.clone());

            let Some(process_name) = max_process else {
                break; // All buffers empty
            };

            // Evict oldest log from that process
            if let Some(buffer) = self.buffers.get_mut(&process_name) {
                if let Some(evicted) = buffer.pop_front() {
                    self.estimated_memory = self
                        .estimated_memory
                        .saturating_sub(Self::estimate_log_size(&evicted));
                } else {
                    break;
                }
            } else {
                break;
            }
        }
    }

    /// Get all logs sorted by timestamp (internal helper).
    fn get_all_internal(&self, limit: Option<usize>) -> Vec<LogLine> {
        let mut all: Vec<LogLine> = self
            .buffers
            .values()
            .flat_map(|buf| buf.iter().cloned())
            .collect();

        all.sort_by_key(|l| l.timestamp);

        match limit {
            Some(n) => all
                .into_iter()
                .rev()
                .take(n)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect(),
            None => all,
        }
    }

    /// Get logs for a specific process (internal helper).
    fn get_process_internal(&self, name: &str, limit: Option<usize>) -> Vec<LogLine> {
        self.buffers
            .get(name)
            .map(|buf| {
                let iter = buf.iter().cloned();
                match limit {
                    Some(n) => iter
                        .rev()
                        .take(n)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect(),
                    None => iter.collect(),
                }
            })
            .unwrap_or_default()
    }

    /// Query logs with filtering support (internal helper).
    fn query_internal(&self, query: &LogQuery) -> (Vec<LogLine>, usize) {
        // Get base logs
        let mut logs = if query.is_all_processes() {
            self.get_all_internal(None)
        } else {
            self.get_process_internal(query.effective_process(), None)
        };

        // Apply group filter
        if let Some(ref group) = query.group {
            logs.retain(|l| l.group.as_ref() == Some(group));
        }

        // Apply timestamp filters
        if let Some(since) = query.since {
            logs.retain(|l| l.timestamp >= since);
        }
        if let Some(until) = query.until {
            logs.retain(|l| l.timestamp <= until);
        }

        // Apply search filter (case-insensitive)
        if let Some(ref search) = query.search {
            let search_lower = search.to_lowercase();
            logs.retain(|l| l.content.to_lowercase().contains(&search_lower));
        }

        let len = logs.len();
        (logs, len)
    }
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LogStorage for LogStore {
    fn push(&mut self, log: LogLine) -> Result<(), LogStorageError> {
        // Mirror the batched path so the SQLite side gets the same
        // single-insert transaction shape even when callers push one
        // line at a time.
        self.push_batch(vec![log])
    }

    fn push_batch(&mut self, logs: Vec<LogLine>) -> Result<(), LogStorageError> {
        // Hot buffer is authoritative for broadcast and recent-tail,
        // so it always lands first. SQLite write failures surface to
        // the caller without dropping what the live UI already saw.
        let persisted = logs.clone();
        for log in logs {
            self.push_internal(log);
        }
        if let Some(sqlite) = self.sqlite.as_mut() {
            sqlite.push_batch(persisted)?;
            // Retention sweeps directly after the batch so the persisted
            // row count and on-disk size stay near the configured budget
            // without waiting for the periodic tick. The sweep is cheap
            // when nothing needs trimming.
            sqlite.enforce_retention()?;
            self.last_retention_at = Some(Instant::now());
        }
        Ok(())
    }

    fn get(&self, name: &str, limit: Option<usize>) -> Result<Vec<LogLine>, LogStorageError> {
        // Hot buffer is the source for the simple recent-tail path; this
        // keeps the broadcast snapshot and the immediate post-restart
        // view aligned with what subscribers just saw. Historical reads
        // beyond the hot buffer's window go through `query`.
        let logs = if name == "*" {
            self.get_all_internal(limit)
        } else {
            self.get_process_internal(name, limit)
        };
        Ok(logs)
    }

    fn query(&self, query: LogQuery) -> Result<LogQueryResult, LogStorageError> {
        if let Some(sqlite) = self.sqlite.as_ref() {
            return sqlite.query(query);
        }

        let (logs, _) = self.query_internal(&query);
        let total_count = logs.len();

        // Apply pagination
        let offset = query.offset.unwrap_or(0);
        let paginated: Vec<LogLine> = match query.limit {
            Some(limit) => logs.into_iter().skip(offset).take(limit).collect(),
            None => logs.into_iter().skip(offset).collect(),
        };

        let has_more = offset + paginated.len() < total_count;

        Ok(LogQueryResult {
            logs: paginated,
            total_count,
            has_more,
            offset,
        })
    }

    fn stats(&self) -> Result<LogStorageStats, LogStorageError> {
        if let Some(sqlite) = self.sqlite.as_ref() {
            // Persistent aggregates win for total_lines / process_count /
            // oldest / newest because the hot buffer only holds a recent
            // slice. Memory pressure fields stay sourced from the hot
            // buffer since the operator tunes them there.
            let persisted = sqlite.stats()?;
            return Ok(LogStorageStats {
                total_lines: persisted.total_lines,
                process_count: persisted.process_count,
                memory_bytes: self.estimated_memory,
                oldest_timestamp: persisted.oldest_timestamp,
                newest_timestamp: persisted.newest_timestamp,
                memory_limit: self.memory_limit,
                max_lines_per_process: self.max_lines_per_process,
            });
        }

        let total_lines: usize = self.buffers.values().map(|b| b.len()).sum();
        let process_count = self.buffers.len();

        let (oldest, newest): (Option<u64>, Option<u64>) = self
            .buffers
            .values()
            .flat_map(|b| b.iter())
            .fold((None, None), |(oldest, newest), log| {
                let oldest = match oldest {
                    None => Some(log.timestamp),
                    Some(t) => Some(t.min(log.timestamp)),
                };
                let newest = match newest {
                    None => Some(log.timestamp),
                    Some(t) => Some(t.max(log.timestamp)),
                };
                (oldest, newest)
            });

        Ok(LogStorageStats {
            total_lines,
            process_count,
            memory_bytes: self.estimated_memory,
            oldest_timestamp: oldest,
            newest_timestamp: newest,
            memory_limit: self.memory_limit,
            max_lines_per_process: self.max_lines_per_process,
        })
    }

    fn clear(&mut self) -> Result<(), LogStorageError> {
        self.buffers.clear();
        self.estimated_memory = 0;
        if let Some(sqlite) = self.sqlite.as_mut() {
            sqlite.clear()?;
        }
        Ok(())
    }

    fn clear_process(&mut self, name: &str) -> Result<(), LogStorageError> {
        if let Some(buffer) = self.buffers.remove(name) {
            let memory_freed: usize = buffer.iter().map(Self::estimate_log_size).sum();
            self.estimated_memory = self.estimated_memory.saturating_sub(memory_freed);
        }
        if let Some(sqlite) = self.sqlite.as_mut() {
            sqlite.clear_process(name)?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), LogStorageError> {
        Ok(())
    }

    fn flush_now(&mut self) -> Result<(), LogStorageError> {
        if let Some(sqlite) = self.sqlite.as_mut() {
            sqlite.flush_now()?;
        }
        Ok(())
    }

    fn enforce_retention(&mut self) -> Result<(), LogStorageError> {
        if let Some(sqlite) = self.sqlite.as_mut() {
            sqlite.enforce_retention()?;
        }
        Ok(())
    }

    fn memory_limit(&self) -> Option<usize> {
        self.memory_limit
    }

    fn set_memory_limit(&mut self, bytes: usize) -> Option<usize> {
        let old = self.memory_limit;
        self.memory_limit = Some(bytes);
        // Trigger eviction if new limit is lower
        self.maybe_evict_for_memory();
        old
    }

    fn max_lines_per_process(&self) -> usize {
        self.max_lines_per_process
    }

    fn set_max_lines_per_process(&mut self, max: usize) -> usize {
        let old = self.max_lines_per_process;
        self.max_lines_per_process = max;
        // Trim existing buffers if needed
        for buffer in self.buffers.values_mut() {
            while buffer.len() > max {
                if let Some(evicted) = buffer.pop_front() {
                    self.estimated_memory = self
                        .estimated_memory
                        .saturating_sub(Self::estimate_log_size(&evicted));
                }
            }
        }
        old
    }
}

/// Test-only methods for LogStore.
#[cfg(test)]
impl LogStore {
    /// Set maximum lines to keep per process (test helper, same as with_max_lines_per_process).
    pub fn with_max_lines(mut self, max_lines: usize) -> Self {
        self.max_lines_per_process = max_lines;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log_storage::LogQuery;

    fn make_log(process: &str, content: &str, timestamp: u64) -> LogLine {
        LogLine {
            process: process.to_string(),
            group: None,
            content: content.to_string(),
            timestamp,
            source: LogSource::Process,
            output_kind: crate::api::OutputKind::Combined,
        }
    }

    #[test]
    fn test_push_and_get() {
        let mut store = LogStore::new();

        store.push(make_log("task1", "line 1", 100)).unwrap();
        store.push(make_log("task1", "line 2", 200)).unwrap();

        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].content, "line 1");
        assert_eq!(logs[1].content, "line 2");
    }

    #[test]
    fn test_get_nonexistent_process() {
        let store = LogStore::new();
        let logs = store.get("nonexistent", None).unwrap();
        assert!(logs.is_empty());
    }

    #[test]
    fn test_get_with_limit() {
        let mut store = LogStore::new();

        for i in 0..10 {
            store
                .push(make_log("task1", &format!("line {}", i), i as u64 * 100))
                .unwrap();
        }

        let logs = store.get("task1", Some(3)).unwrap();
        assert_eq!(logs.len(), 3);
        // Should get the LAST 3 lines
        assert_eq!(logs[0].content, "line 7");
        assert_eq!(logs[1].content, "line 8");
        assert_eq!(logs[2].content, "line 9");
    }

    #[test]
    fn test_get_all_sorted_by_timestamp() {
        let mut store = LogStore::new();

        // Push logs out of timestamp order
        store.push(make_log("task1", "task1 line 1", 300)).unwrap();
        store.push(make_log("task2", "task2 line 1", 100)).unwrap();
        store.push(make_log("task1", "task1 line 2", 400)).unwrap();
        store.push(make_log("task2", "task2 line 2", 200)).unwrap();

        let logs = store.get("*", None).unwrap();
        assert_eq!(logs.len(), 4);

        // Should be sorted by timestamp
        assert_eq!(logs[0].timestamp, 100);
        assert_eq!(logs[0].content, "task2 line 1");

        assert_eq!(logs[1].timestamp, 200);
        assert_eq!(logs[1].content, "task2 line 2");

        assert_eq!(logs[2].timestamp, 300);
        assert_eq!(logs[2].content, "task1 line 1");

        assert_eq!(logs[3].timestamp, 400);
        assert_eq!(logs[3].content, "task1 line 2");
    }

    #[test]
    fn test_max_lines_enforced() {
        let mut store = LogStore::new().with_max_lines(5);

        for i in 0..10 {
            store
                .push(make_log("task1", &format!("line {}", i), i as u64 * 100))
                .unwrap();
        }

        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 5);
        // Should keep the most recent 5
        assert_eq!(logs[0].content, "line 5");
        assert_eq!(logs[4].content, "line 9");
    }

    #[tokio::test]
    async fn test_drain_moves_channel_logs_to_buffer() {
        let mut store = LogStore::new();
        let sender = store.sender();

        // Send logs via channel (simulating spawned task)
        sender
            .send(make_log("task1", "async line 1", 100))
            .await
            .unwrap();
        sender
            .send(make_log("task1", "async line 2", 200))
            .await
            .unwrap();

        // Logs should NOT be visible yet
        assert!(store.get("task1", None).unwrap().is_empty());

        // Drain the channel
        store.drain().unwrap();

        // Now logs should be visible
        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].content, "async line 1");
        assert_eq!(logs[1].content, "async line 2");
    }

    #[tokio::test]
    async fn test_drain_before_get_ensures_fresh_logs() {
        let mut store = LogStore::new();
        let sender = store.sender();

        // Simulate task sending logs
        sender.send(make_log("task1", "line 1", 100)).await.unwrap();

        // Without drain, logs are not visible
        assert!(store.get("task1", None).unwrap().is_empty());

        // Drain and get
        store.drain().unwrap();
        assert_eq!(store.get("task1", None).unwrap().len(), 1);

        // Send more logs
        sender.send(make_log("task1", "line 2", 200)).await.unwrap();

        // Still only 1 visible (haven't drained)
        assert_eq!(store.get("task1", None).unwrap().len(), 1);

        // Drain again
        store.drain().unwrap();
        assert_eq!(store.get("task1", None).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn test_interleaved_push_and_channel() {
        let mut store = LogStore::new();
        let sender = store.sender();

        // Direct push
        store.push(make_log("task1", "direct 1", 100)).unwrap();

        // Channel send
        sender
            .send(make_log("task1", "channel 1", 200))
            .await
            .unwrap();

        // Direct push again
        store.push(make_log("task1", "direct 2", 300)).unwrap();

        // Only direct pushes visible
        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 2);

        // Drain
        store.drain().unwrap();

        // Now all 3 visible
        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 3);
    }

    #[tokio::test]
    async fn test_multiple_processes_via_channel() {
        let mut store = LogStore::new();
        let sender = store.sender();

        // Simulate multiple tasks running concurrently
        sender
            .send(make_log("task1", "t1 line", 100))
            .await
            .unwrap();
        sender
            .send(make_log("task2", "t2 line", 150))
            .await
            .unwrap();
        sender
            .send(make_log("task3", "t3 line", 120))
            .await
            .unwrap();

        store.drain().unwrap();

        // Each process has its logs
        assert_eq!(store.get("task1", None).unwrap().len(), 1);
        assert_eq!(store.get("task2", None).unwrap().len(), 1);
        assert_eq!(store.get("task3", None).unwrap().len(), 1);

        // All logs sorted by timestamp
        let all = store.get("*", None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].timestamp, 100); // task1
        assert_eq!(all[1].timestamp, 120); // task3
        assert_eq!(all[2].timestamp, 150); // task2
    }

    #[tokio::test]
    async fn test_broadcast_subscriber_receives_logs() {
        let mut store = LogStore::new();
        let mut subscriber = store.subscribe();

        // Push a log
        store
            .push(make_log("task1", "broadcast test", 100))
            .unwrap();

        // Subscriber should receive it
        let received = subscriber.try_recv().unwrap();
        assert_eq!(received.content, "broadcast test");
    }

    #[tokio::test]
    async fn test_channel_logs_broadcast_on_drain() {
        let mut store = LogStore::new();
        let sender = store.sender();
        let mut subscriber = store.subscribe();

        // Send via channel
        sender
            .send(make_log("task1", "channel broadcast", 100))
            .await
            .unwrap();

        // Not broadcast yet (still in channel)
        assert!(subscriber.try_recv().is_err());

        // Drain moves to buffer AND broadcasts
        store.drain().unwrap();

        // Now subscriber should have it
        let received = subscriber.try_recv().unwrap();
        assert_eq!(received.content, "channel broadcast");
    }

    #[test]
    fn test_query_pagination() {
        let mut store = LogStore::new();

        // Add 20 logs
        for i in 0..20 {
            store
                .push(make_log("task1", &format!("line {}", i), i as u64 * 100))
                .unwrap();
        }

        // Query without pagination
        let result = store.query(LogQuery::process("task1")).unwrap();
        assert_eq!(result.logs.len(), 20);
        assert_eq!(result.total_count, 20);
        assert!(!result.has_more);
        assert_eq!(result.offset, 0);

        // Query with limit
        let result = store
            .query(LogQuery::process("task1").with_limit(5))
            .unwrap();
        assert_eq!(result.logs.len(), 5);
        assert_eq!(result.total_count, 20);
        assert!(result.has_more);
        assert_eq!(result.offset, 0);
        assert_eq!(result.logs[0].content, "line 0");

        // Query with offset and limit
        let result = store
            .query(LogQuery::process("task1").with_offset(10).with_limit(5))
            .unwrap();
        assert_eq!(result.logs.len(), 5);
        assert_eq!(result.total_count, 20);
        assert!(result.has_more);
        assert_eq!(result.offset, 10);
        assert_eq!(result.logs[0].content, "line 10");

        // Query at the end
        let result = store
            .query(LogQuery::process("task1").with_offset(15).with_limit(10))
            .unwrap();
        assert_eq!(result.logs.len(), 5); // Only 5 remaining
        assert_eq!(result.total_count, 20);
        assert!(!result.has_more);
        assert_eq!(result.offset, 15);
    }

    #[test]
    fn test_query_search() {
        let mut store = LogStore::new();

        store.push(make_log("task1", "INFO: started", 100)).unwrap();
        store
            .push(make_log("task1", "ERROR: something failed", 200))
            .unwrap();
        store
            .push(make_log("task1", "INFO: processing", 300))
            .unwrap();
        store
            .push(make_log("task1", "ERROR: another failure", 400))
            .unwrap();
        store.push(make_log("task1", "INFO: done", 500)).unwrap();

        // Search for ERROR (case-insensitive)
        let result = store
            .query(LogQuery::process("task1").with_search("error"))
            .unwrap();
        assert_eq!(result.logs.len(), 2);
        assert_eq!(result.total_count, 2);
        assert!(result.logs[0].content.contains("ERROR"));
        assert!(result.logs[1].content.contains("ERROR"));

        // Search with pagination
        let result = store
            .query(LogQuery::process("task1").with_search("INFO").with_limit(2))
            .unwrap();
        assert_eq!(result.logs.len(), 2);
        assert_eq!(result.total_count, 3);
        assert!(result.has_more);
    }

    #[test]
    fn test_query_all_processes() {
        let mut store = LogStore::new();

        store.push(make_log("task1", "task1 line", 100)).unwrap();
        store.push(make_log("task2", "task2 line", 200)).unwrap();
        store.push(make_log("task3", "task3 line", 150)).unwrap();

        // Query all processes
        let result = store.query(LogQuery::all()).unwrap();
        assert_eq!(result.logs.len(), 3);

        // Should be sorted by timestamp
        assert_eq!(result.logs[0].timestamp, 100);
        assert_eq!(result.logs[1].timestamp, 150);
        assert_eq!(result.logs[2].timestamp, 200);

        // Pagination on all
        let result = store.query(LogQuery::all().with_limit(2)).unwrap();
        assert_eq!(result.logs.len(), 2);
        assert!(result.has_more);
    }

    #[test]
    fn test_memory_eviction() {
        // Create store with tiny memory limit
        let mut store = LogStore::new().with_memory_limit(1000); // ~1KB limit

        // Add logs until eviction kicks in
        for i in 0..100 {
            store
                .push(make_log(
                    "task1",
                    &format!("a longer line to take up more memory {}", i),
                    i as u64 * 100,
                ))
                .unwrap();
        }

        // Should have evicted some logs due to memory pressure
        let stats = store.stats().unwrap();
        assert!(stats.memory_bytes < 1000); // Under the target (80% of limit = 800)

        // Logs should still be retrievable
        let result = store.query(LogQuery::process("task1")).unwrap();
        assert!(result.total_count < 100); // Some were evicted
        assert!(result.total_count > 0); // But not all
    }

    #[test]
    fn test_max_lines_per_process_enforcement() {
        let mut store = LogStore::new();
        store.set_max_lines_per_process(10);

        // Add 20 logs
        for i in 0..20 {
            store
                .push(make_log("task1", &format!("line {}", i), i as u64 * 100))
                .unwrap();
        }

        // Should only have 10
        let result = store.query(LogQuery::process("task1")).unwrap();
        assert_eq!(result.total_count, 10);

        // Should have the most recent 10 (lines 10-19)
        assert_eq!(result.logs[0].content, "line 10");
        assert_eq!(result.logs[9].content, "line 19");
    }

    #[test]
    fn test_push_batch_makes_lines_visible() {
        let mut store = LogStore::new();
        let batch = vec![
            make_log("task1", "batch 1", 100),
            make_log("task1", "batch 2", 200),
            make_log("task2", "other", 150),
        ];

        store.push_batch(batch).unwrap();

        let result = store.query(LogQuery::all()).unwrap();
        assert_eq!(result.total_count, 3);
        assert_eq!(result.logs[0].timestamp, 100);
        assert_eq!(result.logs[1].timestamp, 150);
        assert_eq!(result.logs[2].timestamp, 200);
    }

    #[test]
    fn test_flush_is_noop_for_memory_backend() {
        let mut store = LogStore::new();
        store.push(make_log("task1", "before flush", 100)).unwrap();

        store.flush().unwrap();

        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].content, "before flush");
    }

    #[test]
    fn test_flush_now_is_noop_for_memory_backend() {
        let mut store = LogStore::new();
        store
            .push(make_log("task1", "before flush_now", 100))
            .unwrap();

        store.flush_now().unwrap();

        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].content, "before flush_now");
    }

    #[test]
    fn test_enforce_retention_is_noop_for_memory_backend() {
        let mut store = LogStore::new();
        for i in 0..5 {
            store
                .push(make_log("task1", &format!("line {}", i), i as u64 * 100))
                .unwrap();
        }
        let before = store.query(LogQuery::all()).unwrap().total_count;

        store.enforce_retention().unwrap();

        let after = store.query(LogQuery::all()).unwrap().total_count;
        assert_eq!(before, after);
    }

    #[tokio::test]
    async fn test_drain_and_flush_now_visibility() {
        let mut store = LogStore::new();
        let sender = store.sender();

        sender
            .send(make_log("task1", "shutdown line 1", 100))
            .await
            .unwrap();
        sender
            .send(make_log("task1", "shutdown line 2", 200))
            .await
            .unwrap();

        store.drain_and_flush_now().unwrap();

        let logs = store.get("task1", None).unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].content, "shutdown line 1");
        assert_eq!(logs[1].content, "shutdown line 2");
    }

    // =========================================================================
    // Hybrid runtime: hot buffer + SQLite
    // =========================================================================

    fn hybrid_store() -> LogStore {
        LogStore::new().with_sqlite(
            SqliteLogStorage::open_in_memory().expect("open in-memory sqlite"),
        )
    }

    fn hybrid_store_with_retention(
        policy: crate::log_storage_sqlite::RetentionPolicy,
    ) -> LogStore {
        LogStore::new().with_sqlite(
            SqliteLogStorage::open_in_memory()
                .expect("open in-memory sqlite")
                .with_retention(policy),
        )
    }

    #[test]
    fn hybrid_get_serves_from_hot_buffer() {
        let mut store = hybrid_store();
        store.push(make_log("task1", "one", 100)).unwrap();
        store.push(make_log("task1", "two", 200)).unwrap();
        store.push(make_log("task1", "three", 300)).unwrap();

        // Recent-tail path stays in memory; the broadcast snapshot uses
        // this path, so persisted history shouldn't perturb it.
        let logs = store.get("task1", Some(2)).unwrap();
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].content, "two");
        assert_eq!(logs[1].content, "three");
    }

    #[test]
    fn hybrid_query_routes_to_sqlite() {
        let mut store = hybrid_store();
        for i in 0..5 {
            store
                .push(make_log("task1", &format!("line {i}"), i as u64 * 100))
                .unwrap();
        }
        // Drop the hot buffer so any rows the query returns must have
        // come from SQLite.
        store.buffers.clear();
        store.estimated_memory = 0;

        let result = store.query(LogQuery::process("task1")).unwrap();
        assert_eq!(result.total_count, 5);
        assert_eq!(result.logs.len(), 5);
        assert_eq!(result.logs[0].content, "line 0");
        assert_eq!(result.logs[4].content, "line 4");
    }

    #[tokio::test]
    async fn hybrid_broadcast_delivers_every_line() {
        let mut store = hybrid_store();
        let mut subscriber = store.subscribe();

        store.push(make_log("task1", "alpha", 100)).unwrap();
        store.push(make_log("task1", "beta", 200)).unwrap();
        store.push(make_log("task1", "gamma", 300)).unwrap();

        // Broadcast lands inside push_internal before the SQLite write,
        // so subscribers see every line regardless of what SQLite does.
        let mut received = Vec::new();
        while let Ok(line) = subscriber.try_recv() {
            received.push(line.content);
        }
        assert_eq!(received, vec!["alpha", "beta", "gamma"]);
    }

    #[tokio::test]
    async fn hybrid_drain_writes_batch_to_sqlite() {
        let mut store = hybrid_store();
        let sender = store.sender();

        for i in 0..4 {
            sender
                .send(make_log("task1", &format!("c{i}"), i as u64 * 100))
                .await
                .unwrap();
        }
        store.drain().unwrap();

        // Query path routes to SQLite when attached, so a positive count
        // here proves the drained batch reached the persistent backend.
        let result = store.query(LogQuery::process("task1")).unwrap();
        assert_eq!(result.total_count, 4);
        assert_eq!(result.logs[0].content, "c0");
        assert_eq!(result.logs[3].content, "c3");
    }

    #[test]
    fn hybrid_clear_wipes_both_backends() {
        let mut store = hybrid_store();
        store.push(make_log("task1", "a", 100)).unwrap();
        store.push(make_log("task2", "b", 200)).unwrap();

        store.clear().unwrap();

        assert!(store.get("task1", None).unwrap().is_empty());
        assert!(store.get("task2", None).unwrap().is_empty());
        let result = store.query(LogQuery::all()).unwrap();
        assert_eq!(result.total_count, 0);
    }

    #[test]
    fn hybrid_clear_process_wipes_only_named_in_both_backends() {
        let mut store = hybrid_store();
        store.push(make_log("task1", "a", 100)).unwrap();
        store.push(make_log("task2", "b", 200)).unwrap();
        store.push(make_log("task1", "c", 300)).unwrap();

        store.clear_process("task1").unwrap();

        assert!(store.get("task1", None).unwrap().is_empty());
        let remaining = store.query(LogQuery::all()).unwrap();
        assert_eq!(remaining.total_count, 1);
        assert_eq!(remaining.logs[0].process, "task2");
    }

    #[test]
    fn hybrid_flush_now_propagates_to_sqlite() {
        let mut store = hybrid_store();
        store.push(make_log("task1", "before flush", 100)).unwrap();
        // In-memory SQLite journal mode is `memory`, so the underlying
        // wal_checkpoint is a no-op; this asserts the propagation path
        // returns Ok regardless.
        store.flush_now().unwrap();
        let result = store.query(LogQuery::process("task1")).unwrap();
        assert_eq!(result.total_count, 1);
    }

    #[test]
    fn hybrid_stats_combine_persistent_aggregates_with_memory_pressure() {
        let mut store = hybrid_store();
        store.push(make_log("task1", "a", 100)).unwrap();
        store.push(make_log("task2", "b", 200)).unwrap();
        store.push(make_log("task1", "c", 300)).unwrap();

        let stats = store.stats().unwrap();
        // Aggregates come from SQLite.
        assert_eq!(stats.total_lines, 3);
        assert_eq!(stats.process_count, 2);
        assert_eq!(stats.oldest_timestamp, Some(100));
        assert_eq!(stats.newest_timestamp, Some(300));
        // Memory-side fields come from the hot buffer.
        assert!(stats.memory_bytes > 0);
        assert_eq!(stats.memory_limit, Some(DEFAULT_MEMORY_LIMIT));
        assert_eq!(stats.max_lines_per_process, DEFAULT_MAX_LINES_PER_PROCESS);
    }

    #[test]
    fn hybrid_push_batch_enforces_per_process_retention() {
        let mut store = hybrid_store_with_retention(crate::log_storage_sqlite::RetentionPolicy {
            max_size_bytes: 100 * 1024 * 1024,
            max_lines_per_process: 3,
        });

        // Ten rows into the same process; the after-batch retention sweep
        // should trim the persisted side back to the policy limit while
        // the broadcast and hot-buffer paths still see everything.
        let batch: Vec<LogLine> = (0..10)
            .map(|i| make_log("task1", &format!("l{i}"), i as u64))
            .collect();
        store.push_batch(batch).unwrap();

        let result = store.query(LogQuery::process("task1")).unwrap();
        assert_eq!(result.total_count, 3);
        // The three most recent lines survive: ascending-order id is
        // insertion order, so contents `l7`, `l8`, `l9` remain.
        assert_eq!(result.logs[0].content, "l7");
        assert_eq!(result.logs[2].content, "l9");
    }

    #[test]
    fn hybrid_maybe_enforce_retention_tick_skips_within_cadence() {
        let mut store = hybrid_store_with_retention(crate::log_storage_sqlite::RetentionPolicy {
            max_size_bytes: 100 * 1024 * 1024,
            max_lines_per_process: 3,
        });

        // First batch goes through push_batch and bumps the retention gate.
        let initial: Vec<LogLine> = (0..5)
            .map(|i| make_log("task1", &format!("init{i}"), i as u64))
            .collect();
        store.push_batch(initial).unwrap();
        assert_eq!(
            store.query(LogQuery::process("task1")).unwrap().total_count,
            3
        );

        // Re-seed the persistent side directly so the gate stays fresh;
        // the tick should see "due == false" and skip the SQL.
        let sqlite = store.sqlite.as_mut().expect("sqlite attached");
        sqlite
            .push_batch(vec![
                make_log("task1", "extra1", 100),
                make_log("task1", "extra2", 200),
            ])
            .unwrap();
        assert_eq!(
            store.query(LogQuery::process("task1")).unwrap().total_count,
            5
        );

        store
            .maybe_enforce_retention_tick(Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            store.query(LogQuery::process("task1")).unwrap().total_count,
            5,
            "tick within cadence should be a no-op",
        );
    }

    #[test]
    fn hybrid_maybe_enforce_retention_tick_runs_after_cadence_elapsed() {
        let mut store = hybrid_store_with_retention(crate::log_storage_sqlite::RetentionPolicy {
            max_size_bytes: 100 * 1024 * 1024,
            max_lines_per_process: 3,
        });

        let initial: Vec<LogLine> = (0..5)
            .map(|i| make_log("task1", &format!("init{i}"), i as u64))
            .collect();
        store.push_batch(initial).unwrap();

        let sqlite = store.sqlite.as_mut().expect("sqlite attached");
        sqlite
            .push_batch(vec![
                make_log("task1", "extra1", 100),
                make_log("task1", "extra2", 200),
            ])
            .unwrap();
        assert_eq!(
            store.query(LogQuery::process("task1")).unwrap().total_count,
            5
        );

        // Wind the gate back so the cadence elapses.
        store.last_retention_at = Some(Instant::now() - Duration::from_secs(120));

        store
            .maybe_enforce_retention_tick(Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            store.query(LogQuery::process("task1")).unwrap().total_count,
            3,
            "tick past the cadence should sweep persistent rows",
        );
    }

    #[test]
    fn hybrid_maybe_enforce_retention_tick_runs_first_call_without_gate() {
        let mut store = hybrid_store_with_retention(crate::log_storage_sqlite::RetentionPolicy {
            max_size_bytes: 100 * 1024 * 1024,
            max_lines_per_process: 3,
        });

        // Seed directly so `last_retention_at` stays `None`.
        let sqlite = store.sqlite.as_mut().expect("sqlite attached");
        let initial: Vec<LogLine> = (0..6)
            .map(|i| make_log("task1", &format!("seed{i}"), i as u64))
            .collect();
        sqlite.push_batch(initial).unwrap();
        assert_eq!(
            store.query(LogQuery::process("task1")).unwrap().total_count,
            6
        );

        // First-ever tick runs unconditionally.
        store
            .maybe_enforce_retention_tick(Duration::from_secs(60))
            .unwrap();
        assert_eq!(
            store.query(LogQuery::process("task1")).unwrap().total_count,
            3
        );
        assert!(store.last_retention_at.is_some());
    }

    #[test]
    fn hybrid_maybe_enforce_retention_tick_noop_without_sqlite() {
        let mut store = LogStore::new();
        store
            .maybe_enforce_retention_tick(Duration::from_secs(60))
            .unwrap();
        // No SQLite, no gate bump.
        assert!(store.last_retention_at.is_none());
    }

    #[test]
    fn hybrid_sqlite_write_failure_does_not_drop_broadcast() {
        let mut store = hybrid_store();
        let mut subscriber = store.subscribe();

        // Install a trigger that aborts inserts whose content matches the
        // sentinel, so the next push_batch hits a SQLite write error
        // while the hot-buffer + broadcast path has already succeeded.
        let sentinel = "__POISON__";
        let install_sql = format!(
            "CREATE TRIGGER abort_on_poison
             BEFORE INSERT ON log_entries
             FOR EACH ROW WHEN NEW.content = '{sentinel}'
             BEGIN
                 SELECT RAISE(ABORT, 'poison row rejected');
             END"
        );
        store
            .sqlite
            .as_ref()
            .expect("hybrid store has sqlite")
            .conn_for_test()
            .execute(&install_sql, ())
            .expect("install trigger");

        let err = store
            .push(make_log("task1", sentinel, 100))
            .expect_err("expected SQLite write to fail");
        match err {
            LogStorageError::Write(msg) => assert!(msg.contains("poison"), "msg was {msg}"),
            other => panic!("expected Write, got {other:?}"),
        }

        // Broadcast subscriber saw the line.
        let delivered = subscriber.try_recv().expect("broadcast delivered");
        assert_eq!(delivered.content, sentinel);
        // Hot buffer kept the line so live UI does not silently drop it
        // even though SQLite refused to persist it.
        let mem = store.get("task1", None).unwrap();
        assert_eq!(mem.len(), 1);
        assert_eq!(mem[0].content, sentinel);
    }

    // =========================================================================
    // Degraded-mode contract: SQLite errors surface through the trait while
    // the hot buffer and broadcast keep the live path alive, and subsequent
    // operations resume durable storage once the underlying issue clears.
    //
    // ZAZ-012 milestone 6 pinned the contract via these three scenarios:
    // (1) write failure mid-batch, (2) query failure, (3) lock timeout
    // under contention.
    // =========================================================================

    #[test]
    fn degraded_mode_write_failure_mid_batch_keeps_live_path_alive_and_recovers() {
        let mut store = hybrid_store();
        let mut subscriber = store.subscribe();

        // Seed two persisted rows so the rollback assertion has a positive
        // pre-existing set to compare against.
        store.push(make_log("task1", "seed-a", 50)).unwrap();
        store.push(make_log("task1", "seed-b", 75)).unwrap();

        // Install a trigger that aborts inserts whose content matches the
        // sentinel. Placed mid-batch so the assertion proves atomic rollback
        // of the entire batch, not just the offending row.
        let sentinel = "__POISON__";
        let install_sql = format!(
            "CREATE TRIGGER abort_on_poison
             BEFORE INSERT ON log_entries
             FOR EACH ROW WHEN NEW.content = '{sentinel}'
             BEGIN
                 SELECT RAISE(ABORT, 'poison row rejected');
             END"
        );
        store
            .sqlite
            .as_ref()
            .expect("hybrid store has sqlite")
            .conn_for_test()
            .execute(&install_sql, ())
            .expect("install trigger");

        let batch = vec![
            make_log("task1", "batch-1", 100),
            make_log("task1", sentinel, 200),
            make_log("task1", "batch-3", 300),
        ];
        let err = store
            .push_batch(batch)
            .expect_err("expected SQLite write to fail mid-batch");
        match err {
            LogStorageError::Write(msg) => assert!(msg.contains("poison"), "msg was {msg}"),
            other => panic!("expected Write, got {other:?}"),
        }

        // Live path survives: broadcast received every batch line because
        // push_internal landed them before the SQLite transaction ran.
        let mut delivered = Vec::new();
        while let Ok(line) = subscriber.try_recv() {
            delivered.push(line.content);
        }
        assert_eq!(
            delivered,
            vec!["seed-a", "seed-b", "batch-1", sentinel, "batch-3"],
        );

        // Hot buffer holds the full sequence including the poisoned line.
        let mem = store.get("task1", None).unwrap();
        let mem_contents: Vec<&str> = mem.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(
            mem_contents,
            vec!["seed-a", "seed-b", "batch-1", sentinel, "batch-3"],
        );

        // Persisted set is the seed pair only; the failed batch rolled back
        // atomically, so no partial-batch leak even though one of three rows
        // would have inserted cleanly on its own.
        let persisted = store.query(LogQuery::process("task1")).unwrap();
        let persisted_contents: Vec<&str> =
            persisted.logs.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(persisted_contents, vec!["seed-a", "seed-b"]);

        // Underlying issue clears: drop the trigger and retry. The next
        // batch persists without operator intervention.
        store
            .sqlite
            .as_ref()
            .unwrap()
            .conn_for_test()
            .execute("DROP TRIGGER abort_on_poison", ())
            .expect("drop trigger");

        store
            .push_batch(vec![
                make_log("task1", "recovery-1", 400),
                make_log("task1", "recovery-2", 500),
            ])
            .expect("recovery batch persists");

        let recovered = store.query(LogQuery::process("task1")).unwrap();
        let recovered_contents: Vec<&str> =
            recovered.logs.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(
            recovered_contents,
            vec!["seed-a", "seed-b", "recovery-1", "recovery-2"],
        );
        assert!(
            !recovered_contents.contains(&sentinel),
            "the rolled-back poisoned batch must not appear in the recovered persisted set",
        );
    }

    #[test]
    fn degraded_mode_query_failure_keeps_hot_buffer_get_serving() {
        let mut store = hybrid_store();
        let mut subscriber = store.subscribe();

        store.push(make_log("task1", "alpha", 100)).unwrap();
        store.push(make_log("task1", "beta", 200)).unwrap();

        // Drop the table so any SQLite-routed read fails. The hot buffer
        // path is independent of the persistent backend, so `get` keeps
        // serving recent-tail reads.
        store
            .sqlite
            .as_ref()
            .expect("hybrid store has sqlite")
            .conn_for_test()
            .execute("DROP TABLE log_entries", ())
            .expect("drop table");

        let err = store
            .query(LogQuery::process("task1"))
            .expect_err("expected SQLite query to fail without the table");
        match err {
            LogStorageError::Query(msg) => {
                let lower = msg.to_lowercase();
                assert!(
                    lower.contains("log_entries") || lower.contains("no such table"),
                    "expected query error to name the missing table, got: {msg}",
                );
            }
            other => panic!("expected Query, got {other:?}"),
        }

        let mem = store.get("task1", None).unwrap();
        let mem_contents: Vec<&str> = mem.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(mem_contents, vec!["alpha", "beta"]);

        // Recreate the schema so subsequent writes can resume. Mirrors the
        // initial migration's DDL.
        store
            .sqlite
            .as_ref()
            .unwrap()
            .conn_for_test()
            .execute_batch(
                "CREATE TABLE log_entries (
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
                 CREATE INDEX log_entries_group_id_idx ON log_entries(group_name, id);",
            )
            .expect("recreate schema");

        store
            .push(make_log("task1", "gamma", 300))
            .expect("post-recovery push persists");

        let mut delivered = Vec::new();
        while let Ok(line) = subscriber.try_recv() {
            delivered.push(line.content);
        }
        assert_eq!(delivered, vec!["alpha", "beta", "gamma"]);

        // Persisted set only contains the post-recovery row; the dropped
        // rows are gone, but the query surface works again.
        let persisted = store.query(LogQuery::process("task1")).unwrap();
        let persisted_contents: Vec<&str> =
            persisted.logs.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(persisted_contents, vec!["gamma"]);
    }

    #[test]
    fn degraded_mode_lock_timeout_under_contention_keeps_live_path_alive() {
        // File-backed DB so a second connection can contend over the same
        // WAL writer slot; :memory: databases are private per connection.
        let tempdir = tempfile::TempDir::new().expect("tempdir");
        let db_path = tempdir.path().join("logs.sqlite3");

        let sqlite = SqliteLogStorage::open(&db_path).expect("open sqlite");
        // Shorten the busy timeout so the test does not sit on the
        // production five-second budget while the blocker holds the lock.
        sqlite
            .conn_for_test()
            .busy_timeout(std::time::Duration::from_millis(50))
            .expect("shorten busy timeout");
        let mut store = LogStore::new().with_sqlite(sqlite);
        let mut subscriber = store.subscribe();

        store
            .push(make_log("task1", "before", 100))
            .expect("pre-contention push persists");

        // Open a competing connection and hold the WAL writer slot via
        // BEGIN IMMEDIATE plus an uncommitted insert.
        let blocker =
            rusqlite::Connection::open(&db_path).expect("open competing connection");
        blocker
            .busy_timeout(std::time::Duration::from_millis(0))
            .expect("blocker timeout");
        blocker
            .execute("BEGIN IMMEDIATE", ())
            .expect("blocker begin");
        blocker
            .execute(
                "INSERT INTO log_entries
                    (timestamp_ms, process, group_name, source, output_kind, content)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    150_i64,
                    "blocker",
                    Option::<&str>::None,
                    "process",
                    "combined",
                    "blocker-row",
                ],
            )
            .expect("blocker insert");

        // Push through the hybrid store. The SQLite transaction's first
        // insert hits SQLITE_BUSY and the busy-timeout retry expires.
        let err = store
            .push(make_log("task1", "during", 200))
            .expect_err("expected SQLite write to fail under lock contention");
        match err {
            LogStorageError::Write(msg) => {
                let lower = msg.to_lowercase();
                assert!(
                    lower.contains("lock") || lower.contains("busy"),
                    "expected SQLite busy/lock error, got: {msg}",
                );
            }
            other => panic!("expected Write, got {other:?}"),
        }

        let mut delivered = Vec::new();
        while let Ok(line) = subscriber.try_recv() {
            delivered.push(line.content);
        }
        assert_eq!(delivered, vec!["before", "during"]);

        let mem = store.get("task1", None).unwrap();
        let mem_contents: Vec<&str> = mem.iter().map(|l| l.content.as_str()).collect();
        assert_eq!(mem_contents, vec!["before", "during"]);

        // Release the lock. Subsequent pushes resume without operator action.
        blocker.execute("COMMIT", ()).expect("blocker commit");
        drop(blocker);

        store
            .push(make_log("task1", "after", 300))
            .expect("post-recovery push persists");

        let persisted = store.query(LogQuery::process("task1")).unwrap();
        let persisted_contents: Vec<&str> =
            persisted.logs.iter().map(|l| l.content.as_str()).collect();
        assert!(
            persisted_contents.contains(&"before"),
            "pre-contention row should still be persisted: {persisted_contents:?}",
        );
        assert!(
            persisted_contents.contains(&"after"),
            "post-recovery row should be persisted: {persisted_contents:?}",
        );
        assert!(
            !persisted_contents.contains(&"during"),
            "contended row rolled back when the busy timeout fired: {persisted_contents:?}",
        );
    }
}
