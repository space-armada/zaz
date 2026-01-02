//! Error types for the daemon.

use thiserror::Error;

/// Errors that can occur in the daemon.
#[derive(Debug, Error)]
pub enum DaemonError {
    /// Failed to bind to socket.
    #[error("failed to bind to socket: {0}")]
    Bind(#[from] std::io::Error),

    /// Configuration error.
    #[error("configuration error: {0}")]
    Config(#[from] zaz_config::ConfigError),

    /// Watch error.
    #[error("watch error: {0}")]
    Watch(#[from] zaz_watch::WatchError),

    /// Process error.
    #[error("process error: {0}")]
    Process(#[from] zaz_process::ProcessError),

    /// Serialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),

    /// Group not found.
    #[error("group not found: {0}")]
    GroupNotFound(String),

    /// Cyclic dependency detected.
    #[error("cyclic dependency detected involving: {0}")]
    CyclicDependency(String),

    /// Variable expansion error.
    #[error("variable expansion error: {0}")]
    VarExpansion(String),

    /// Task failed.
    #[error("task '{task}' failed: {error}")]
    TaskFailed { task: String, error: String },
}
