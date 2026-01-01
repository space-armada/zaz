//! File system watcher implementation.

use crate::{EventBatch, PatternSet, WatchError};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::broadcast;

/// Configuration for the file watcher.
#[derive(Debug, Clone)]
pub struct WatcherConfig {
    /// Root directory to watch.
    pub root: PathBuf,

    /// Debounce duration.
    pub debounce: Duration,

    /// Default patterns to ignore.
    pub default_ignores: Vec<String>,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            debounce: Duration::from_millis(100),
            default_ignores: default_ignores(),
        }
    }
}

/// Default ignore patterns for common files that shouldn't trigger rebuilds.
pub fn default_ignores() -> Vec<String> {
    vec![
        "**/.git/**".to_string(),
        "**/.hg/**".to_string(),
        "**/.svn/**".to_string(),
        "**/.DS_Store".to_string(),
        "**/node_modules/**".to_string(),
        "**/target/**".to_string(),
        "**/*.swp".to_string(),
        "**/*~".to_string(),
        "**/#*#".to_string(),
        "**/.#*".to_string(),
    ]
}

/// A file event from the watcher.
#[derive(Debug, Clone)]
pub struct FileEvent {
    /// The path that changed.
    pub path: PathBuf,

    /// The kind of change.
    pub kind: FileEventKind,
}

/// The kind of file change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileEventKind {
    Created,
    Modified,
    Deleted,
    Renamed,
}

/// File system watcher that monitors for changes.
pub struct Watcher {
    #[allow(dead_code)]
    watcher: RecommendedWatcher,
    receiver: mpsc::Receiver<Result<notify::Event, notify::Error>>,
    sender: broadcast::Sender<Vec<FileEvent>>,
    config: WatcherConfig,
    batch: EventBatch,
}

impl Watcher {
    /// Create a new watcher with the given configuration.
    pub fn new(config: WatcherConfig) -> Result<Self, WatchError> {
        let (tx, rx) = mpsc::channel();

        let watcher = notify::recommended_watcher(tx)?;
        let (broadcast_tx, _) = broadcast::channel(16);

        Ok(Self {
            watcher,
            receiver: rx,
            sender: broadcast_tx,
            config,
            batch: EventBatch::new(),
        })
    }

    /// Start watching a directory.
    pub fn watch(&mut self, path: &Path) -> Result<(), WatchError> {
        self.watcher
            .watch(path, RecursiveMode::Recursive)
            .map_err(|e| WatchError::WatchPath {
                path: path.to_path_buf(),
                source: e,
            })
    }

    /// Subscribe to file events.
    pub fn subscribe(&self) -> broadcast::Receiver<Vec<FileEvent>> {
        self.sender.subscribe()
    }

    /// Process pending events and return batched events if ready.
    pub fn poll(&mut self, patterns: &PatternSet) -> Option<Vec<FileEvent>> {
        // Collect all pending events
        while let Ok(result) = self.receiver.try_recv() {
            if let Ok(event) = result {
                for path in event.paths {
                    if patterns.matches(&path) {
                        let kind = match event.kind {
                            notify::EventKind::Create(_) => FileEventKind::Created,
                            notify::EventKind::Modify(_) => FileEventKind::Modified,
                            notify::EventKind::Remove(_) => FileEventKind::Deleted,
                            _ => continue,
                        };

                        self.batch.add(FileEvent { path, kind });
                    }
                }
            }
        }

        // Check if batch is ready
        if !self.batch.is_empty() && self.batch.is_ready(self.config.debounce) {
            let events = self.batch.drain();
            let _ = self.sender.send(events.clone());
            Some(events)
        } else {
            None
        }
    }
}
