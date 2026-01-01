//! File watching for zaz.
//!
//! Provides filesystem monitoring with glob pattern matching and debouncing.

mod batch;
mod error;
mod glob;
mod watcher;

pub use batch::EventBatch;
pub use error::WatchError;
pub use glob::PatternSet;
pub use watcher::{default_ignores, FileEvent, FileEventKind, Watcher, WatcherConfig};
