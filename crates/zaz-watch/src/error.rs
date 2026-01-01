//! Error types for file watching.

use std::path::PathBuf;
use thiserror::Error;

/// Errors that can occur during file watching.
#[derive(Debug, Error)]
pub enum WatchError {
    /// Failed to initialize the file watcher.
    #[error("failed to initialize watcher: {0}")]
    Init(#[from] notify::Error),

    /// Failed to compile glob pattern.
    #[error("invalid glob pattern '{pattern}': {source}")]
    InvalidPattern {
        pattern: String,
        #[source]
        source: globset::Error,
    },

    /// Failed to watch a path.
    #[error("failed to watch path {path}: {source}")]
    WatchPath {
        path: PathBuf,
        #[source]
        source: notify::Error,
    },
}
