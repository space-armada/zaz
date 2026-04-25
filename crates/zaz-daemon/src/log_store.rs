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
use crate::log_storage::{LogQuery, LogQueryResult, LogStorage, LogStorageStats};
use std::collections::{HashMap, VecDeque};
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
        }
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
    pub fn drain(&mut self) {
        while let Ok(log) = self.rx.try_recv() {
            self.push_internal(log);
        }
    }

    /// Push a log directly to storage (for synchronous code paths).
    pub fn push(&mut self, log: LogLine) {
        self.push_internal(log);
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
    fn push(&mut self, log: LogLine) {
        self.push_internal(log);
    }

    fn get(&self, name: &str, limit: Option<usize>) -> Vec<LogLine> {
        if name == "*" {
            self.get_all_internal(limit)
        } else {
            self.get_process_internal(name, limit)
        }
    }

    fn query(&self, query: LogQuery) -> LogQueryResult {
        let (logs, _) = self.query_internal(&query);
        let total_count = logs.len();

        // Apply pagination
        let offset = query.offset.unwrap_or(0);
        let paginated: Vec<LogLine> = match query.limit {
            Some(limit) => logs.into_iter().skip(offset).take(limit).collect(),
            None => logs.into_iter().skip(offset).collect(),
        };

        let has_more = offset + paginated.len() < total_count;

        LogQueryResult {
            logs: paginated,
            total_count,
            has_more,
            offset,
        }
    }

    fn stats(&self) -> LogStorageStats {
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

        LogStorageStats {
            total_lines,
            process_count,
            memory_bytes: self.estimated_memory,
            oldest_timestamp: oldest,
            newest_timestamp: newest,
            memory_limit: self.memory_limit,
            max_lines_per_process: self.max_lines_per_process,
        }
    }

    fn clear(&mut self) {
        self.buffers.clear();
        self.estimated_memory = 0;
    }

    fn clear_process(&mut self, name: &str) {
        if let Some(buffer) = self.buffers.remove(name) {
            let memory_freed: usize = buffer.iter().map(Self::estimate_log_size).sum();
            self.estimated_memory = self.estimated_memory.saturating_sub(memory_freed);
        }
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

        store.push(make_log("task1", "line 1", 100));
        store.push(make_log("task1", "line 2", 200));

        let logs = store.get("task1", None);
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].content, "line 1");
        assert_eq!(logs[1].content, "line 2");
    }

    #[test]
    fn test_get_nonexistent_process() {
        let store = LogStore::new();
        let logs = store.get("nonexistent", None);
        assert!(logs.is_empty());
    }

    #[test]
    fn test_get_with_limit() {
        let mut store = LogStore::new();

        for i in 0..10 {
            store.push(make_log("task1", &format!("line {}", i), i as u64 * 100));
        }

        let logs = store.get("task1", Some(3));
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
        store.push(make_log("task1", "task1 line 1", 300));
        store.push(make_log("task2", "task2 line 1", 100));
        store.push(make_log("task1", "task1 line 2", 400));
        store.push(make_log("task2", "task2 line 2", 200));

        let logs = store.get("*", None);
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
            store.push(make_log("task1", &format!("line {}", i), i as u64 * 100));
        }

        let logs = store.get("task1", None);
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
        assert!(store.get("task1", None).is_empty());

        // Drain the channel
        store.drain();

        // Now logs should be visible
        let logs = store.get("task1", None);
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
        assert!(store.get("task1", None).is_empty());

        // Drain and get
        store.drain();
        assert_eq!(store.get("task1", None).len(), 1);

        // Send more logs
        sender.send(make_log("task1", "line 2", 200)).await.unwrap();

        // Still only 1 visible (haven't drained)
        assert_eq!(store.get("task1", None).len(), 1);

        // Drain again
        store.drain();
        assert_eq!(store.get("task1", None).len(), 2);
    }

    #[tokio::test]
    async fn test_interleaved_push_and_channel() {
        let mut store = LogStore::new();
        let sender = store.sender();

        // Direct push
        store.push(make_log("task1", "direct 1", 100));

        // Channel send
        sender
            .send(make_log("task1", "channel 1", 200))
            .await
            .unwrap();

        // Direct push again
        store.push(make_log("task1", "direct 2", 300));

        // Only direct pushes visible
        let logs = store.get("task1", None);
        assert_eq!(logs.len(), 2);

        // Drain
        store.drain();

        // Now all 3 visible
        let logs = store.get("task1", None);
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

        store.drain();

        // Each process has its logs
        assert_eq!(store.get("task1", None).len(), 1);
        assert_eq!(store.get("task2", None).len(), 1);
        assert_eq!(store.get("task3", None).len(), 1);

        // All logs sorted by timestamp
        let all = store.get("*", None);
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
        store.push(make_log("task1", "broadcast test", 100));

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
        store.drain();

        // Now subscriber should have it
        let received = subscriber.try_recv().unwrap();
        assert_eq!(received.content, "channel broadcast");
    }

    #[test]
    fn test_query_pagination() {
        use crate::log_storage::LogStorage;

        let mut store = LogStore::new();

        // Add 20 logs
        for i in 0..20 {
            store.push(make_log("task1", &format!("line {}", i), i as u64 * 100));
        }

        // Query without pagination
        let result = store.query(LogQuery::process("task1"));
        assert_eq!(result.logs.len(), 20);
        assert_eq!(result.total_count, 20);
        assert!(!result.has_more);
        assert_eq!(result.offset, 0);

        // Query with limit
        let result = store.query(LogQuery::process("task1").with_limit(5));
        assert_eq!(result.logs.len(), 5);
        assert_eq!(result.total_count, 20);
        assert!(result.has_more);
        assert_eq!(result.offset, 0);
        assert_eq!(result.logs[0].content, "line 0");

        // Query with offset and limit
        let result = store.query(LogQuery::process("task1").with_offset(10).with_limit(5));
        assert_eq!(result.logs.len(), 5);
        assert_eq!(result.total_count, 20);
        assert!(result.has_more);
        assert_eq!(result.offset, 10);
        assert_eq!(result.logs[0].content, "line 10");

        // Query at the end
        let result = store.query(LogQuery::process("task1").with_offset(15).with_limit(10));
        assert_eq!(result.logs.len(), 5); // Only 5 remaining
        assert_eq!(result.total_count, 20);
        assert!(!result.has_more);
        assert_eq!(result.offset, 15);
    }

    #[test]
    fn test_query_search() {
        use crate::log_storage::LogStorage;

        let mut store = LogStore::new();

        store.push(make_log("task1", "INFO: started", 100));
        store.push(make_log("task1", "ERROR: something failed", 200));
        store.push(make_log("task1", "INFO: processing", 300));
        store.push(make_log("task1", "ERROR: another failure", 400));
        store.push(make_log("task1", "INFO: done", 500));

        // Search for ERROR (case-insensitive)
        let result = store.query(LogQuery::process("task1").with_search("error"));
        assert_eq!(result.logs.len(), 2);
        assert_eq!(result.total_count, 2);
        assert!(result.logs[0].content.contains("ERROR"));
        assert!(result.logs[1].content.contains("ERROR"));

        // Search with pagination
        let result = store.query(LogQuery::process("task1").with_search("INFO").with_limit(2));
        assert_eq!(result.logs.len(), 2);
        assert_eq!(result.total_count, 3);
        assert!(result.has_more);
    }

    #[test]
    fn test_query_all_processes() {
        use crate::log_storage::LogStorage;

        let mut store = LogStore::new();

        store.push(make_log("task1", "task1 line", 100));
        store.push(make_log("task2", "task2 line", 200));
        store.push(make_log("task3", "task3 line", 150));

        // Query all processes
        let result = store.query(LogQuery::all());
        assert_eq!(result.logs.len(), 3);

        // Should be sorted by timestamp
        assert_eq!(result.logs[0].timestamp, 100);
        assert_eq!(result.logs[1].timestamp, 150);
        assert_eq!(result.logs[2].timestamp, 200);

        // Pagination on all
        let result = store.query(LogQuery::all().with_limit(2));
        assert_eq!(result.logs.len(), 2);
        assert!(result.has_more);
    }

    #[test]
    fn test_memory_eviction() {
        use crate::log_storage::LogStorage;

        // Create store with tiny memory limit
        let mut store = LogStore::new().with_memory_limit(1000); // ~1KB limit

        // Add logs until eviction kicks in
        for i in 0..100 {
            store.push(make_log(
                "task1",
                &format!("a longer line to take up more memory {}", i),
                i as u64 * 100,
            ));
        }

        // Should have evicted some logs due to memory pressure
        let stats = store.stats();
        assert!(stats.memory_bytes < 1000); // Under the target (80% of limit = 800)

        // Logs should still be retrievable
        let result = store.query(LogQuery::process("task1"));
        assert!(result.total_count < 100); // Some were evicted
        assert!(result.total_count > 0); // But not all
    }

    #[test]
    fn test_max_lines_per_process_enforcement() {
        use crate::log_storage::LogStorage;

        let mut store = LogStore::new();
        store.set_max_lines_per_process(10);

        // Add 20 logs
        for i in 0..20 {
            store.push(make_log("task1", &format!("line {}", i), i as u64 * 100));
        }

        // Should only have 10
        let result = store.query(LogQuery::process("task1"));
        assert_eq!(result.total_count, 10);

        // Should have the most recent 10 (lines 10-19)
        assert_eq!(result.logs[0].content, "line 10");
        assert_eq!(result.logs[9].content, "line 19");
    }
}
