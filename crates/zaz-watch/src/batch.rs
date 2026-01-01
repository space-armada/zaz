//! Event batching and debouncing.

use crate::FileEvent;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A batch of file events accumulated during the debounce period.
#[derive(Debug, Clone, Default)]
pub struct EventBatch {
    events: HashMap<PathBuf, FileEvent>,
    first_event: Option<Instant>,
}

impl EventBatch {
    /// Create a new empty batch.
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an event to the batch.
    pub fn add(&mut self, event: FileEvent) {
        if self.first_event.is_none() {
            self.first_event = Some(Instant::now());
        }
        // Later events for the same path override earlier ones
        self.events.insert(event.path.clone(), event);
    }

    /// Check if the batch is ready to be flushed (debounce period elapsed).
    pub fn is_ready(&self, debounce: Duration) -> bool {
        self.first_event
            .map(|t| t.elapsed() >= debounce)
            .unwrap_or(false)
    }

    /// Check if the batch is empty.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Get the number of events in the batch.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Consume the batch and return all events.
    pub fn drain(&mut self) -> Vec<FileEvent> {
        self.first_event = None;
        self.events.drain().map(|(_, e)| e).collect()
    }

    /// Get all events without consuming.
    pub fn events(&self) -> impl Iterator<Item = &FileEvent> {
        self.events.values()
    }
}
