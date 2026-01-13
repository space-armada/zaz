//! Log storage with async channel support.
//!
//! This module provides a testable log storage abstraction that:
//! - Receives logs from spawned tasks via an async channel
//! - Stores logs in per-process ring buffers
//! - Returns logs sorted by timestamp
//!
//! The key invariant is: logs sent to the channel are available
//! via `get()` after calling `drain()`.

use crate::api::{LogLine, LogSource};
use std::collections::{HashMap, VecDeque};
use tokio::sync::{broadcast, mpsc};

/// Maximum log lines to keep per process.
const MAX_LOG_LINES: usize = 1000;

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
    max_lines: usize,

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
            max_lines: MAX_LOG_LINES,
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

        // Store in per-process buffer
        let buffer = self.buffers.entry(log.process.clone()).or_default();
        buffer.push_back(log.clone());

        // Trim to max size
        while buffer.len() > self.max_lines {
            buffer.pop_front();
        }

        // Broadcast to subscribers (ignore errors if no subscribers)
        let _ = self.broadcast_tx.send(log);
    }

    /// Get logs for a process.
    ///
    /// If `name` is "*", returns all logs sorted by timestamp.
    pub fn get(&self, name: &str, limit: Option<usize>) -> Vec<LogLine> {
        if name == "*" {
            self.get_all(limit)
        } else {
            self.get_process(name, limit)
        }
    }

    /// Get all logs sorted by timestamp.
    fn get_all(&self, limit: Option<usize>) -> Vec<LogLine> {
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

    /// Get logs for a specific process.
    fn get_process(&self, name: &str, limit: Option<usize>) -> Vec<LogLine> {
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
}

impl Default for LogStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Test-only methods for LogStore.
#[cfg(test)]
impl LogStore {
    /// Set maximum lines to keep per process.
    pub fn with_max_lines(mut self, max_lines: usize) -> Self {
        self.max_lines = max_lines;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
